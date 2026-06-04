//! End-to-end socket smoke test for Stage 2a: spawn the daemon in a
//! thread, connect over a temp-path unix socket, exercise
//! `Ping`/`Status`/`Shutdown` round-trips, and verify the daemon
//! exits cleanly.
//!
//! Tests the full path: listener `accept()` → per-connection thread
//! → `dispatch` → `write_message` → client `read_message`. The unit
//! tests in `src/server.rs` only cover `dispatch` in isolation.
//!
//! Cfg-gated to Linux + macOS to match the daemon itself.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_daemon::protocol::{read_message, write_message, Request, Response};
use projgit_daemon::server::{run, DaemonConfig};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn temp_socket(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "projgitd-test-{label}-{}.sock",
        std::process::id()
    ))
}

/// Spawn the daemon in a background thread, return (socket_path,
/// JoinHandle) once the socket file is observable on disk.
fn spawn_daemon(label: &str) -> (PathBuf, thread::JoinHandle<anyhow::Result<()>>) {
    let socket_path = temp_socket(label);
    // Clean stale.
    let _ = std::fs::remove_file(&socket_path);
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        socket_mode: 0o600,
        cache_dir: None,
        cache_depth: None,
        trace: false,
    };
    let handle = thread::spawn(move || run(config));

    // Poll until the socket appears (avoids races between `bind()`
    // returning and the test trying to connect).
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if socket_path.exists() {
            return (socket_path, handle);
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("daemon never created socket at {}", socket_path.display());
}

/// One request → one response → close.
fn rpc(socket: &PathBuf, req: &Request) -> Response {
    let mut stream = UnixStream::connect(socket).expect("connect");
    write_message(&mut stream, req).expect("write");
    read_message(&mut stream).expect("read")
}

#[test]
fn ping_status_shutdown_roundtrip() {
    let (sock, handle) = spawn_daemon("ping");

    match rpc(&sock, &Request::Ping) {
        Response::Pong => {}
        other => panic!("ping: got {other:?}"),
    }

    match rpc(&sock, &Request::Status) {
        Response::Status(r) => {
            // No source, no mounts in 2a.
            assert!(r.source.is_none());
            assert!(r.mounts.is_empty());
            assert!(r.cache.is_none());
        }
        other => panic!("status: got {other:?}"),
    }

    match rpc(&sock, &Request::Shutdown) {
        Response::Ok => {}
        other => panic!("shutdown: got {other:?}"),
    }

    // Daemon should exit on its own. join() with a generous deadline.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() {
        if Instant::now() > deadline {
            panic!("daemon never exited after Shutdown");
        }
        thread::sleep(Duration::from_millis(20));
    }
    handle
        .join()
        .expect("daemon thread panicked")
        .expect("daemon returned Err");

    // Socket file should have been cleaned up.
    assert!(!sock.exists(), "socket file lingering at {}", sock.display());
}

#[test]
fn mount_rejects_nonexistent_source() {
    // Stage 2b: Mount is no longer a stub. A bad mountpoint surfaces
    // as `mount_failed` (canonicalize fails before we even touch the
    // source).
    let (sock, handle) = spawn_daemon("mount-bad-mp");

    let req = Request::Mount {
        source: "/nonexistent/path/to/repo".into(),
        ref_name: "main".into(),
        mountpoint: PathBuf::from("/tmp/this/path/never/exists"),
        no_dotgit: false,
        allow_other: false,
    };
    match rpc(&sock, &req) {
        Response::Err { code, .. } => {
            assert_eq!(code, "mount_failed", "mountpoint canonicalize should fail first");
        }
        other => panic!("got {other:?}"),
    }

    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
}

#[test]
fn shutdown_after_multiple_pings() {
    // Verify the dispatch / per-connection-thread model survives
    // back-to-back connections.
    let (sock, handle) = spawn_daemon("multi");
    for _ in 0..5 {
        match rpc(&sock, &Request::Ping) {
            Response::Pong => {}
            other => panic!("got {other:?}"),
        }
    }
    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
}
