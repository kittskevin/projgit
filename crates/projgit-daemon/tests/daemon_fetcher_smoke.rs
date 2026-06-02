//! End-to-end smoke for [`DaemonFetcher`]: spawn the daemon in-thread
//! against a local fixture repo, then exercise the fetcher's
//! `fetch_object` and `prefetch_headers` over a real unix socket.
//!
//! No FUSE involved — this is just the daemon control plane + the
//! fetcher impl. The sidecar-side end-to-end (DaemonFetcher feeding
//! a `HydratingObjectStore` plugged into a FUSE mount) lives in
//! `crates/projgit-cli/tests/sidecar_smoke.rs` once Stage 3c lands.
//!
//! Cfg-gated to Linux + macOS to match the daemon.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_core::{Fetcher, HeaderProbe};
use projgit_daemon::protocol::{read_message, write_message, Request, Response};
use projgit_daemon::server::{run, DaemonConfig};
use projgit_daemon::DaemonFetcher;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "projgit-test")
        .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
        .env("GIT_COMMITTER_NAME", "projgit-test")
        .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

fn build_fixture(label: &str) -> (PathBuf, gix::ObjectId) {
    let base = std::env::temp_dir().join(format!("projgit-fetcher-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    std::fs::write(base.join("hello.txt"), b"hello from daemon_fetcher\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "initial"]);
    let oid_bytes = git(&base, &["rev-parse", "HEAD:hello.txt"]);
    let oid_hex = String::from_utf8(oid_bytes).unwrap().trim().to_owned();
    let oid = gix::ObjectId::from_hex(oid_hex.as_bytes()).unwrap();
    (base, oid)
}

fn temp_path(prefix: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "projgit-fetcher-{prefix}-{}{suffix}",
        std::process::id()
    ))
}

fn spawn_daemon(label: &str) -> (PathBuf, thread::JoinHandle<anyhow::Result<()>>) {
    let socket_path = temp_path(label, ".sock");
    let _ = std::fs::remove_file(&socket_path);
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        socket_mode: 0o600,
        cache_dir: None,
    };
    let handle = thread::spawn(move || run(config));
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if socket_path.exists() {
            return (socket_path, handle);
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("daemon never created socket");
}

fn shutdown(sock: &PathBuf, handle: thread::JoinHandle<anyhow::Result<()>>) {
    let mut s = UnixStream::connect(sock).unwrap();
    write_message(&mut s, &Request::Shutdown).unwrap();
    let _: Response = read_message(&mut s).unwrap();
    handle.join().unwrap().unwrap();
}

fn attach(sock: &PathBuf, source: &Path) {
    let mut s = UnixStream::connect(sock).unwrap();
    write_message(
        &mut s,
        &Request::Attach {
            source: source.to_string_lossy().into_owned(),
        },
    )
    .unwrap();
    match read_message::<_, Response>(&mut s).unwrap() {
        Response::Attached { .. } => {}
        other => panic!("attach: got {other:?}"),
    }
}

// -----------------------------------------------------------------------------

#[test]
fn fetch_object_against_running_daemon() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, blob_oid) = build_fixture("fetch");
    let (sock, handle) = spawn_daemon("fetch");
    attach(&sock, &repo);

    let fetcher = DaemonFetcher::new(sock.clone());
    fetcher.fetch_object(blob_oid).expect("fetch_object");

    // Calling again should still succeed (idempotent on the daemon
    // side; the coalescer keys evict on completion).
    fetcher.fetch_object(blob_oid).expect("fetch_object #2");

    shutdown(&sock, handle);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn fetch_object_unknown_surfaces_backend_error() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, _) = build_fixture("unknown");
    let (sock, handle) = spawn_daemon("unknown");
    attach(&sock, &repo);

    let fetcher = DaemonFetcher::new(sock.clone());
    let absent = gix::ObjectId::from_hex(b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
    match fetcher.fetch_object(absent) {
        Err(projgit_core::FetcherError::Backend(o, msg)) => {
            assert_eq!(o, absent);
            // Daemon's NoopFetcher cannot hydrate -> FETCH_FAILED.
            assert!(
                msg.contains("fetch_failed") || msg.contains("daemon"),
                "msg = {msg:?}",
            );
        }
        other => panic!("expected Backend error, got {other:?}"),
    }

    shutdown(&sock, handle);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn fetch_object_after_daemon_crash_returns_transport_error() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, blob_oid) = build_fixture("crash");
    let (sock, handle) = spawn_daemon("crash");
    attach(&sock, &repo);

    let fetcher = DaemonFetcher::new(sock.clone());
    fetcher.fetch_object(blob_oid).expect("pre-crash fetch ok");

    // Shut the daemon down and confirm the next fetch fails as a
    // Transport error (connect refused). This is the failure-mode
    // contract for Stage 3: warm reads through the shared CAS keep
    // working in the sidecar, but cold-fetch RPCs return errors
    // until the daemon is restarted.
    shutdown(&sock, handle);

    match fetcher.fetch_object(blob_oid) {
        Err(projgit_core::FetcherError::Transport(o, msg)) => {
            assert_eq!(o, blob_oid);
            assert!(
                msg.contains("connect") || msg.contains("No such file") || msg.contains("refused"),
                "msg = {msg:?}",
            );
        }
        other => panic!("expected Transport error after daemon shutdown, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn prefetch_headers_against_running_daemon() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, blob_oid) = build_fixture("prefetch");
    let (sock, handle) = spawn_daemon("prefetch");
    attach(&sock, &repo);

    let fetcher = DaemonFetcher::new(sock.clone());
    let absent = gix::ObjectId::from_hex(b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
    let probes = fetcher.prefetch_headers(&[blob_oid, absent]);
    assert_eq!(probes.len(), 2);
    match &probes[0] {
        HeaderProbe::Present(o) | HeaderProbe::PresentWithHeader(o, _, _) => {
            assert_eq!(*o, blob_oid);
        }
        other => panic!("probe[0] = {other:?}"),
    }
    match &probes[1] {
        HeaderProbe::Error(o, _) => assert_eq!(*o, absent),
        other => panic!("probe[1] = {other:?}"),
    }

    shutdown(&sock, handle);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn prefetch_headers_empty_input_returns_empty_no_rpc() {
    // Don't even spawn a daemon — the fetcher should short-circuit
    // empty input without touching the socket.
    let fetcher = DaemonFetcher::new(PathBuf::from("/nonexistent/sock"));
    let probes = fetcher.prefetch_headers(&[]);
    assert!(probes.is_empty());
}
