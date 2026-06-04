//! Stage 3a end-to-end test for the sidecar-mode control plane:
//! `Attach`, `Fetch`, `PrefetchHeaders`. No FUSE — this just
//! exercises the new RPCs end-to-end against a local fixture repo.
//!
//! Always-on (no `#[ignore]`): the daemon doesn't need a real
//! mount to hydrate objects through its own `HydratingObjectStore`,
//! and the test uses a local-path source so `NoopFetcher` is
//! enough. The mount-side tests stay in `mount_smoke.rs`.
//!
//! Cfg-gated to Linux + macOS to match the daemon itself.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_daemon::protocol::{
    codes, read_message, write_message, HeaderProbeWire, Request, Response,
};
use projgit_daemon::server::{run, DaemonConfig};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Fixture helpers (mirror mount_smoke.rs to keep the test self-contained).
// -----------------------------------------------------------------------------

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

/// Build a tiny local repo with one commit on `main`. Returns
/// (repo_path, blob_oid_hex_for_hello_txt).
fn build_fixture(label: &str) -> (PathBuf, String) {
    let base = std::env::temp_dir().join(format!("projgit-daemon-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    std::fs::write(base.join("hello.txt"), b"hello from fetch_smoke\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "initial"]);
    let oid_bytes = git(&base, &["rev-parse", "HEAD:hello.txt"]);
    let oid = String::from_utf8(oid_bytes).unwrap().trim().to_owned();
    (base, oid)
}

fn temp_path(prefix: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "projgit-daemon-{prefix}-{}{suffix}",
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
        cache_depth: None,
        trace: false,
        pool_size: 1,
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

fn rpc(socket: &PathBuf, req: &Request) -> Response {
    let mut s = UnixStream::connect(socket).expect("connect");
    write_message(&mut s, req).expect("write");
    read_message(&mut s).expect("read")
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn fetch_before_attach_is_not_attached() {
    let (sock, handle) = spawn_daemon("fetch-no-attach");
    match rpc(
        &sock,
        &Request::Fetch {
            oid: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into(),
        },
    ) {
        Response::Err { code, .. } => assert_eq!(code, codes::NOT_ATTACHED),
        other => panic!("got {other:?}"),
    }
    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
}

#[test]
fn fetch_with_bad_oid_surfaces_bad_oid() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, _) = build_fixture("bad-oid-fixture");
    let (sock, handle) = spawn_daemon("fetch-bad-oid");

    match rpc(
        &sock,
        &Request::Attach {
            source: repo.to_string_lossy().into_owned(),
        },
    ) {
        Response::Attached { .. } => {}
        other => panic!("attach: got {other:?}"),
    }

    match rpc(
        &sock,
        &Request::Fetch {
            oid: "not-a-real-oid".into(),
        },
    ) {
        Response::Err { code, .. } => assert_eq!(code, codes::BAD_OID),
        other => panic!("got {other:?}"),
    }

    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn attach_returns_git_dir_and_is_idempotent() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, _) = build_fixture("attach-fixture");
    let (sock, handle) = spawn_daemon("attach");

    let git_dir_a = match rpc(
        &sock,
        &Request::Attach {
            source: repo.to_string_lossy().into_owned(),
        },
    ) {
        Response::Attached { git_dir } => git_dir,
        other => panic!("attach: got {other:?}"),
    };
    assert!(
        git_dir_a.exists(),
        "git_dir {} should exist on disk",
        git_dir_a.display()
    );
    assert!(
        git_dir_a.join("HEAD").exists() || git_dir_a.join("refs").exists(),
        "git_dir {} should look like a git dir",
        git_dir_a.display()
    );

    // Second attach with the same source returns the same path.
    let git_dir_b = match rpc(
        &sock,
        &Request::Attach {
            source: repo.to_string_lossy().into_owned(),
        },
    ) {
        Response::Attached { git_dir } => git_dir,
        other => panic!("attach #2: got {other:?}"),
    };
    assert_eq!(
        git_dir_a, git_dir_b,
        "idempotent Attach must return the same git_dir"
    );

    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn attach_then_fetch_hydrates_object_through_daemon() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, blob_oid) = build_fixture("fetch-fixture");
    let (sock, handle) = spawn_daemon("fetch");

    match rpc(
        &sock,
        &Request::Attach {
            source: repo.to_string_lossy().into_owned(),
        },
    ) {
        Response::Attached { .. } => {}
        other => panic!("attach: got {other:?}"),
    }

    // Fetch a real blob OID; for a local-source daemon this just
    // reads through the existing store (no upstream needed) but
    // proves the RPC path end-to-end.
    match rpc(
        &sock,
        &Request::Fetch {
            oid: blob_oid.clone(),
        },
    ) {
        Response::Ok => {}
        other => panic!("fetch: got {other:?}"),
    }

    // Fetching the same OID twice must succeed both times (no
    // single-shot caching gotchas).
    match rpc(
        &sock,
        &Request::Fetch {
            oid: blob_oid.clone(),
        },
    ) {
        Response::Ok => {}
        other => panic!("fetch #2: got {other:?}"),
    }

    // Status now shows non-zero header cache traffic from the
    // server-side `header()` call inside `fetch_one`. (The exact
    // hit/miss split depends on whether the header LRU absorbed
    // the second call; just check that *some* activity registered.)
    match rpc(&sock, &Request::Status) {
        Response::Status(r) => {
            let cache = r.cache.expect("attached daemon has cache stats");
            assert!(
                cache.header_hits + cache.header_misses >= 1,
                "expected at least one header cache touch after Fetch; got {cache:?}",
            );
        }
        other => panic!("status: got {other:?}"),
    }

    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn prefetch_headers_returns_one_probe_per_input() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, blob_oid) = build_fixture("prefetch-fixture");
    let (sock, handle) = spawn_daemon("prefetch");

    match rpc(
        &sock,
        &Request::Attach {
            source: repo.to_string_lossy().into_owned(),
        },
    ) {
        Response::Attached { .. } => {}
        other => panic!("attach: got {other:?}"),
    }

    // Mix a real OID with one that's syntactically valid hex but
    // not present in the store. The valid-but-absent OID should
    // surface as a per-probe Error; the real one as Present /
    // PresentWithHeader.
    let absent = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_owned();
    match rpc(
        &sock,
        &Request::PrefetchHeaders {
            oids: vec![blob_oid.clone(), absent.clone()],
        },
    ) {
        Response::HeaderProbes { probes } => {
            assert_eq!(probes.len(), 2, "one probe per input OID");
            // First probe: real OID, present.
            match &probes[0] {
                HeaderProbeWire::Present { oid }
                | HeaderProbeWire::PresentWithHeader { oid, .. } => {
                    assert_eq!(oid, &blob_oid);
                }
                other => panic!("probe[0] = {other:?}"),
            }
            // Second probe: absent OID, error.
            match &probes[1] {
                HeaderProbeWire::Error { oid, .. } => assert_eq!(oid, &absent),
                other => panic!("probe[1] = {other:?}"),
            }
        }
        other => panic!("prefetch_headers: got {other:?}"),
    }

    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn attach_to_different_source_is_source_mismatch() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo_a, _) = build_fixture("mismatch-a");
    let (repo_b, _) = build_fixture("mismatch-b");
    let (sock, handle) = spawn_daemon("attach-mismatch");

    match rpc(
        &sock,
        &Request::Attach {
            source: repo_a.to_string_lossy().into_owned(),
        },
    ) {
        Response::Attached { .. } => {}
        other => panic!("attach A: got {other:?}"),
    }
    match rpc(
        &sock,
        &Request::Attach {
            source: repo_b.to_string_lossy().into_owned(),
        },
    ) {
        Response::Err { code, .. } => assert_eq!(code, codes::SOURCE_MISMATCH),
        other => panic!("attach B: got {other:?}"),
    }

    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&repo_a);
    let _ = std::fs::remove_dir_all(&repo_b);
}
