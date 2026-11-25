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
    std::env::temp_dir().join(format!("projgitd-test-{label}-{}.sock", std::process::id()))
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
        pool_size: 1,
        pid_file: None,
        maintenance_interval_secs: None,
        blob_cache_max_bytes: None,
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
    assert!(
        !sock.exists(),
        "socket file lingering at {}",
        sock.display()
    );
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
        isolated_dotgit: false,
        allow_other: false,
        writable_scratch: None,
    };
    match rpc(&sock, &req) {
        Response::Err { code, .. } => {
            assert_eq!(
                code, "mount_failed",
                "mountpoint canonicalize should fail first"
            );
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

#[test]
fn background_maintenance_evicts_excess_l2_blobs_under_budget() {
    use std::process::Command;
    use std::time::SystemTime;

    if Command::new("git")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let repo = std::env::temp_dir().join(format!(
        "projgitd-test-l2-evict-repo-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();

    let run_git = |args: &[&str]| -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "projgit-test")
            .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
            .env("GIT_COMMITTER_NAME", "projgit-test")
            .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };

    run_git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("old.bin"), vec![0x11u8; 4096]).unwrap();
    std::fs::write(repo.join("new.bin"), vec![0x22u8; 4096]).unwrap();
    run_git(&["add", "-A"]);
    run_git(&["commit", "-q", "-m", "initial"]);
    run_git(&["repack", "-a", "-d", "-q"]);

    let old_oid = run_git(&["rev-parse", "HEAD:old.bin"]);
    let new_oid = run_git(&["rev-parse", "HEAD:new.bin"]);

    let socket_path = temp_socket("l2-evict");
    let _ = std::fs::remove_file(&socket_path);
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        socket_mode: 0o600,
        cache_dir: None,
        cache_depth: None,
        trace: false,
        pool_size: 1,
        pid_file: None,
        maintenance_interval_secs: Some(1),
        blob_cache_max_bytes: Some(4096),
    };
    let handle = thread::spawn(move || run(config));

    let start = Instant::now();
    while !socket_path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "daemon never created socket"
        );
        thread::sleep(Duration::from_millis(10));
    }

    // Attach to the local fixture repo and materialize both blobs via Fetch RPCs.
    match rpc(
        &socket_path,
        &Request::Attach {
            source: repo.to_string_lossy().into_owned(),
        },
    ) {
        Response::Attached { .. } => {}
        other => panic!("expected Attached, got {other:?}"),
    }
    assert!(matches!(
        rpc(
            &socket_path,
            &Request::Fetch {
                oid: old_oid.clone()
            }
        ),
        Response::Ok
    ));
    assert!(matches!(
        rpc(
            &socket_path,
            &Request::Fetch {
                oid: new_oid.clone()
            }
        ),
        Response::Ok
    ));

    let blobs_dir = repo.join(".git").join("blobs");
    let old_blob_path = blobs_dir.join(&old_oid[..2]).join(&old_oid);
    let new_blob_path = blobs_dir.join(&new_oid[..2]).join(&new_oid);
    assert!(old_blob_path.is_file(), "old blob should be materialized");
    assert!(new_blob_path.is_file(), "new blob should be materialized");

    // Backdate old_blob_path so it is deterministically the LRU victim.
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&old_blob_path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&old_blob_path, perms).unwrap();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&old_blob_path)
            .unwrap();
        let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let times = std::fs::FileTimes::new()
            .set_accessed(past)
            .set_modified(past);
        f.set_times(times).unwrap();
        f.set_modified(past).unwrap();
    }

    // Wait for the 1-second background maintenance tick to evict old_blob_path.
    let evict_deadline = Instant::now() + Duration::from_secs(5);
    while old_blob_path.exists() {
        assert!(
            Instant::now() < evict_deadline,
            "background maintenance did not evict oldest blob within deadline"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        new_blob_path.is_file(),
        "newest blob within the 4096-byte budget must remain"
    );
    assert!(
        blobs_dir.join("headers.bin").is_file(),
        "headers.bin must remain intact"
    );

    let _ = rpc(&socket_path, &Request::Shutdown);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&repo);
}
