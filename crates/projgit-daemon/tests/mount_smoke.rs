//! Stage 2b end-to-end test: spawn the daemon, send a `Mount` request
//! pointing at a local fixture repo, verify the mount actually serves
//! file content through the kernel, send `Status` and check cache
//! counters, send `Umount`, verify the mount goes away, send
//! `Shutdown`, verify the daemon exits.
//!
//! `#[ignore]`-gated because it needs FUSE (`/dev/fuse`) and the
//! system `git` CLI. Matches the convention in
//! `crates/projgit-fuse/tests/mount_smoke.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_daemon::protocol::{read_message, write_message, Request, Response};
use projgit_daemon::server::{run, DaemonConfig};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Fixture helpers (mirror crates/projgit-fuse/tests/mount_smoke.rs).
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

/// Build a small local repo with one commit on `main`.
fn build_fixture(label: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "projgit-daemon-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    std::fs::write(base.join("README.md"), b"# daemon fixture\n").unwrap();
    std::fs::write(base.join("hello.txt"), b"hello from the daemon\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "initial"]);
    base
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
        pid_file: None,
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

/// Wait until `mountpoint` becomes a FUSE mount (st_dev differs from
/// parent) or `timeout` elapses.
fn wait_for_mount(mountpoint: &Path, timeout: Duration) -> bool {
    use std::os::unix::fs::MetadataExt;
    let parent_dev = mountpoint
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(0);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(m) = std::fs::metadata(mountpoint) {
            if m.dev() != parent_dev {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

// -----------------------------------------------------------------------------
// The test.
// -----------------------------------------------------------------------------

#[test]
#[ignore = "requires FUSE + git CLI; run inside the devcontainer"]
fn mount_status_umount_lifecycle_via_daemon() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let repo = build_fixture("smoke-fixture");

    // Two mountpoints — exercises the multi-mount-per-daemon path
    // (cache sharing) on top of the single-mount lifecycle.
    let mp_a = temp_path("mp-a", "");
    let mp_b = temp_path("mp-b", "");
    std::fs::create_dir_all(&mp_a).unwrap();
    std::fs::create_dir_all(&mp_b).unwrap();

    let (sock, handle) = spawn_daemon("lifecycle");

    // First mount.
    let mount_req = |mp: &PathBuf| Request::Mount {
        source: repo.to_string_lossy().into_owned(),
        ref_name: "main".into(),
        mountpoint: mp.clone(),
        no_dotgit: true, // skip dotgit; the test only checks file content
        allow_other: false,
    };
    match rpc(&sock, &mount_req(&mp_a)) {
        Response::Ok => {}
        other => panic!("mount A: got {other:?}"),
    }
    assert!(
        wait_for_mount(&mp_a, Duration::from_secs(5)),
        "mount A never came up"
    );

    // Read a file via the kernel; proves the daemon-served mount is
    // actually live and serving FUSE traffic.
    let hello_a = std::fs::read_to_string(mp_a.join("hello.txt")).expect("read A hello.txt");
    assert_eq!(hello_a, "hello from the daemon\n");

    // Second mount of the same source — shared ObjectStore should be
    // re-used. Status will show both.
    match rpc(&sock, &mount_req(&mp_b)) {
        Response::Ok => {}
        other => panic!("mount B: got {other:?}"),
    }
    assert!(wait_for_mount(&mp_b, Duration::from_secs(5)));

    // Read the same file via the second mount — should hit the shared
    // blob cache populated by mount A.
    let hello_b = std::fs::read_to_string(mp_b.join("hello.txt")).expect("read B hello.txt");
    assert_eq!(hello_b, "hello from the daemon\n");

    // Status snapshot.
    match rpc(&sock, &Request::Status) {
        Response::Status(r) => {
            assert_eq!(
                r.source.as_deref(),
                Some(repo.to_string_lossy().as_ref()),
                "source should be set after first Mount"
            );
            assert_eq!(r.mounts.len(), 2, "both mounts should appear");
            let cache = r.cache.expect("cache stats present");
            // Mount B reading the same OID as A must produce at least
            // one blob_cache hit (the Stage 1 amortisation property,
            // now also surfaced via the daemon).
            assert!(
                cache.blob_hits >= 1,
                "expected shared blob_cache hits across daemon mounts; got {cache:?}",
            );
        }
        other => panic!("status: got {other:?}"),
    }

    // Umount both, in arbitrary order.
    match rpc(&sock, &Request::Umount { mountpoint: mp_b.clone() }) {
        Response::Ok => {}
        other => panic!("umount B: got {other:?}"),
    }
    match rpc(&sock, &Request::Umount { mountpoint: mp_a.clone() }) {
        Response::Ok => {}
        other => panic!("umount A: got {other:?}"),
    }

    // After umount the kernel should drop the FUSE mount on Drop
    // of the BackgroundSession.
    let start = Instant::now();
    let mut still_mounted = true;
    while start.elapsed() < Duration::from_secs(3) {
        use std::os::unix::fs::MetadataExt;
        let parent_dev = mp_a
            .parent()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.dev())
            .unwrap_or(0);
        let dev = std::fs::metadata(&mp_a).map(|m| m.dev()).unwrap_or(0);
        if dev == parent_dev {
            still_mounted = false;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(!still_mounted, "mount A should have been removed");

    // Umount a path we never registered should be NO_SUCH_MOUNT.
    match rpc(
        &sock,
        &Request::Umount {
            mountpoint: PathBuf::from("/tmp"),
        },
    ) {
        Response::Err { code, .. } => assert_eq!(code, "no_such_mount"),
        other => panic!("expected no_such_mount, got {other:?}"),
    }

    // Status now reports zero mounts.
    match rpc(&sock, &Request::Status) {
        Response::Status(r) => assert_eq!(r.mounts.len(), 0),
        other => panic!("status (post-umount): got {other:?}"),
    }

    // Shutdown.
    match rpc(&sock, &Request::Shutdown) {
        Response::Ok => {}
        other => panic!("shutdown: got {other:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while !handle.is_finished() {
        if Instant::now() > deadline {
            panic!("daemon never exited");
        }
        thread::sleep(Duration::from_millis(25));
    }
    handle.join().unwrap().unwrap();

    // Cleanup
    let _ = std::fs::remove_dir_all(&mp_a);
    let _ = std::fs::remove_dir_all(&mp_b);
}

#[test]
#[ignore = "requires FUSE + git CLI; run inside the devcontainer"]
fn second_mount_with_different_source_rejected_as_source_mismatch() {
    if !git_available() {
        return;
    }
    let repo_a = build_fixture("smoke-srcA");
    let repo_b = build_fixture("smoke-srcB");
    let mp = temp_path("mp-srcA", "");
    std::fs::create_dir_all(&mp).unwrap();

    let (sock, handle) = spawn_daemon("source-mismatch");

    let req_a = Request::Mount {
        source: repo_a.to_string_lossy().into_owned(),
        ref_name: "main".into(),
        mountpoint: mp.clone(),
        no_dotgit: true,
        allow_other: false,
    };
    match rpc(&sock, &req_a) {
        Response::Ok => {}
        other => panic!("first mount: got {other:?}"),
    }
    assert!(wait_for_mount(&mp, Duration::from_secs(5)));

    // Second mount with a different source must be rejected.
    let mp_b = temp_path("mp-srcB", "");
    std::fs::create_dir_all(&mp_b).unwrap();
    let req_b = Request::Mount {
        source: repo_b.to_string_lossy().into_owned(),
        ref_name: "main".into(),
        mountpoint: mp_b.clone(),
        no_dotgit: true,
        allow_other: false,
    };
    match rpc(&sock, &req_b) {
        Response::Err { code, .. } => assert_eq!(code, "source_mismatch"),
        other => panic!("second mount: got {other:?}"),
    }

    // Cleanup.
    let _ = rpc(&sock, &Request::Umount { mountpoint: mp.clone() });
    let _ = rpc(&sock, &Request::Shutdown);
    handle.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&mp);
    let _ = std::fs::remove_dir_all(&mp_b);
}
