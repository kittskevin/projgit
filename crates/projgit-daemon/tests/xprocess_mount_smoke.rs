//! Stage 3 **cross-process** end-to-end test: spawns the real
//! `projgitd` and `projgit` binaries as separate OS processes (not
//! the in-thread daemon used by the other smoke tests) and exercises
//! the full sidecar topology end-to-end.
//!
//! This is the closest in-CI approximation of the production
//! deployment shape described in
//! [`docs/design/projgitd.md`](../../../../docs/design/projgitd.md):
//! daemon and sidecar are independent processes communicating only
//! over the unix socket + the shared on-disk CAS. The
//! sidecar-in-thread tests in `sidecar_mount_smoke.rs` cover the
//! library API; this test covers the actual compiled binaries +
//! their CLI argument surfaces + their inter-process lifecycle.
//!
//! Two test cases:
//!
//! 1. `xprocess_mount_serves_files` — both binaries spawned as
//!    separate processes; sidecar mounts a local fixture through
//!    `--daemon-socket`; a third reader (this test process) reads
//!    file content through the kernel mount; both children are
//!    stopped cleanly.
//!
//! 2. `xprocess_warm_reads_survive_daemon_kill` — the headline
//!    Stage 3 failure-mode contract validated at the OS-process
//!    level: `kill -KILL <projgitd>` mid-mount; the sidecar's
//!    mount keeps serving cached pages and the kernel sees no
//!    EIO on warm reads.
//!
//! `#[ignore]`-gated because the test needs FUSE (`/dev/fuse`)
//! and the system `git` CLI. Run with
//! `cargo test -p projgit-daemon --test xprocess_mount_smoke
//! -- --ignored --nocapture`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Locate sibling binaries
// -----------------------------------------------------------------------------

/// Resolve the `target/<profile>/` directory the current test binary
/// was built into. Cargo places integration tests at
/// `target/<profile>/deps/<name>-<hash>`, so the grandparent is the
/// profile dir where sibling binaries live.
fn target_profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(Path::parent)
        .expect("test binary path has a profile dir grandparent")
        .to_path_buf()
}

/// `target/<profile>/projgitd` (or `.exe` on Windows; not relevant
/// here — the file is cfg-gated to unix).
fn projgitd_binary() -> PathBuf {
    target_profile_dir().join(if cfg!(windows) { "projgitd.exe" } else { "projgitd" })
}

/// `target/<profile>/projgit`.
fn projgit_binary() -> PathBuf {
    target_profile_dir().join(if cfg!(windows) { "projgit.exe" } else { "projgit" })
}

/// Ensure both binaries exist (rebuild on demand). `cargo test` only
/// builds the binaries of the crate under test, so a standalone
/// `cargo test -p projgit-daemon --test xprocess_mount_smoke` won't
/// have `projgit` in `target/debug/` unless someone already ran
/// `cargo build --workspace`. Building here keeps the test
/// self-contained.
fn ensure_binaries_built() {
    if projgitd_binary().exists() && projgit_binary().exists() {
        return;
    }
    eprintln!("xprocess_mount_smoke: building projgitd + projgit on first run…");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "projgit-daemon", "-p", "projgit-cli"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed: {status}");
    assert!(
        projgitd_binary().exists(),
        "projgitd binary missing at {}",
        projgitd_binary().display(),
    );
    assert!(
        projgit_binary().exists(),
        "projgit binary missing at {}",
        projgit_binary().display(),
    );
}

// -----------------------------------------------------------------------------
// Fixture + path helpers
// -----------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(cwd: &Path, args: &[&str]) {
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
}

fn build_fixture(label: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "projgit-xproc-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    std::fs::write(base.join("README.md"), b"# xprocess fixture\n").unwrap();
    std::fs::write(base.join("hello.txt"), b"hello from xprocess\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "initial"]);
    base
}

fn temp_path(prefix: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "projgit-xproc-{prefix}-{}{suffix}",
        std::process::id()
    ))
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

/// Inverse: wait until `mountpoint` stops being a FUSE mount.
fn wait_for_unmount(mountpoint: &Path, timeout: Duration) -> bool {
    use std::os::unix::fs::MetadataExt;
    let parent_dev = mountpoint
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(0);
    let start = Instant::now();
    while start.elapsed() < timeout {
        let dev = std::fs::metadata(mountpoint).map(|m| m.dev()).unwrap_or(0);
        if dev == parent_dev {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Spawn `projgitd` listening on `socket`. Returns the child and
/// blocks until the socket file exists (so a subsequent
/// `--daemon-socket` flag will find a live listener).
///
/// `#[allow(clippy::zombie_processes)]`: we return ownership of
/// `Child` to the caller for the success path; the failure paths
/// kill + wait before panicking so no zombies escape.
#[allow(clippy::zombie_processes)]
fn spawn_daemon_process(socket: &Path) -> Child {
    let mut child = Command::new(projgitd_binary())
        .arg("--socket")
        .arg(socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn projgitd");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if socket.exists() {
            return child;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("projgitd died before binding socket: {status:?}");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("projgitd never created {} within 5s", socket.display());
}

/// Spawn `projgit mount --daemon-socket SOCK SOURCE MOUNTPOINT --ref REF
/// --no-dotgit`. Returns the child; the caller waits for the mount
/// to come up and kills the child to unmount.
///
/// `#[allow(clippy::zombie_processes)]`: ownership of the spawned
/// child is returned to the caller, which is expected to
/// `stop_gracefully` it (kill + wait) before the test ends.
#[allow(clippy::zombie_processes)]
fn spawn_sidecar_process(socket: &Path, source: &Path, mountpoint: &Path, ref_name: &str) -> Child {
    Command::new(projgit_binary())
        .arg("mount")
        .arg("--daemon-socket")
        .arg(socket)
        .arg("--ref")
        .arg(ref_name)
        .arg("--no-dotgit")
        .arg(source)
        .arg(mountpoint)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn projgit mount")
}

/// Stop a child by sending SIGINT and waiting briefly; falls back to
/// SIGKILL if it doesn't exit. Used for the sidecar (which traps
/// Ctrl-C to drop the FUSE session) and the daemon (which traps
/// SIGINT to self-connect with `Shutdown`).
fn stop_gracefully(child: &mut Child, label: &str) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let pid = Pid::from_raw(child.id() as i32);
    let _ = kill(pid, Signal::SIGINT);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    eprintln!("{label}: SIGINT didn't take, sending SIGKILL");
    let _ = child.kill();
    let _ = child.wait();
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
#[ignore = "requires FUSE + git CLI; runs the projgitd and projgit binaries as \
            separate OS processes (run inside the devcontainer)"]
fn xprocess_mount_serves_files() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    ensure_binaries_built();

    let repo = build_fixture("serve");
    let socket = temp_path("serve", ".sock");
    let _ = std::fs::remove_file(&socket);
    let mp = temp_path("serve-mp", "");
    std::fs::create_dir_all(&mp).unwrap();

    let mut daemon = spawn_daemon_process(&socket);
    let mut sidecar = spawn_sidecar_process(&socket, &repo, &mp, "main");

    let mounted = wait_for_mount(&mp, Duration::from_secs(10));
    assert!(
        mounted,
        "sidecar process never produced a FUSE mount at {}",
        mp.display(),
    );

    // Read content through the kernel — this is the OS-level proof
    // that the cross-process daemon+sidecar topology serves files:
    // - the daemon owns /run/projgitd.sock and the on-disk CAS;
    // - the sidecar holds the /dev/fuse fd in a different process;
    // - this test (a third process) is the agent reading the mount.
    let hello = std::fs::read_to_string(mp.join("hello.txt")).expect("read hello.txt");
    assert_eq!(hello, "hello from xprocess\n");
    let readme = std::fs::read_to_string(mp.join("README.md")).expect("read README.md");
    assert_eq!(readme, "# xprocess fixture\n");

    // Directory enumeration also works across the process boundary.
    let mut names: Vec<String> = std::fs::read_dir(&mp)
        .expect("readdir")
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert!(names.contains(&"hello.txt".to_owned()));
    assert!(names.contains(&"README.md".to_owned()));

    // Teardown: stop the sidecar first (drops the FUSE session →
    // unmount) then the daemon.
    stop_gracefully(&mut sidecar, "sidecar");
    assert!(
        wait_for_unmount(&mp, Duration::from_secs(5)),
        "mount at {} should be gone after sidecar exit",
        mp.display(),
    );
    stop_gracefully(&mut daemon, "daemon");

    let _ = std::fs::remove_dir_all(&mp);
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_file(&socket);
}

#[test]
#[ignore = "requires FUSE + git CLI; runs the projgitd and projgit binaries as \
            separate OS processes (run inside the devcontainer)"]
fn xprocess_warm_reads_survive_daemon_kill() {
    // OS-process-level version of the failure-mode contract in
    // `docs/design/projgitd.md` §3: SIGKILL the daemon mid-mount;
    // the sidecar's mount must keep serving cached pages because
    // the sidecar — not the daemon — holds the /dev/fuse fd, and
    // because the pack bytes are mmap'd from the shared on-disk
    // CAS (no IPC for warm reads).
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    ensure_binaries_built();

    let repo = build_fixture("crash");
    let socket = temp_path("crash", ".sock");
    let _ = std::fs::remove_file(&socket);
    let mp = temp_path("crash-mp", "");
    std::fs::create_dir_all(&mp).unwrap();

    let mut daemon = spawn_daemon_process(&socket);
    let mut sidecar = spawn_sidecar_process(&socket, &repo, &mp, "main");

    assert!(
        wait_for_mount(&mp, Duration::from_secs(10)),
        "sidecar never mounted at {}",
        mp.display(),
    );

    // Warm up: read both files with the daemon alive so the bytes
    // land in the sidecar's small-blob LRU and the OS page cache.
    let warm = std::fs::read_to_string(mp.join("hello.txt")).expect("warm read");
    assert_eq!(warm, "hello from xprocess\n");
    let _ = std::fs::read_to_string(mp.join("README.md")).expect("warm read README");

    // Kill -9 the daemon. Don't even let it run its shutdown handler.
    let _ = daemon.kill();
    let _ = daemon.wait();
    // Give the kernel a moment to notice the socket peer is gone
    // (the listener fd is freed when the daemon's process struct
    // is reaped). Future connects from the sidecar will fail
    // immediately with ECONNREFUSED / ENOENT.
    thread::sleep(Duration::from_millis(50));

    // Warm reads MUST still work. Both files were already warm in
    // both caches; the sidecar serves them from its own
    // ObjectStore + mmap'd packs without ever consulting the
    // daemon. This is the load-bearing Stage 3 property.
    let post_kill = std::fs::read_to_string(mp.join("hello.txt"))
        .expect("warm read must survive daemon kill (Stage 3 contract)");
    assert_eq!(post_kill, "hello from xprocess\n");

    // readdir also works — it uses the tree LRU + local
    // ObjectStore, both daemon-free.
    let names: Vec<_> = std::fs::read_dir(&mp)
        .expect("readdir after kill")
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        names.iter().any(|n| n == "hello.txt"),
        "readdir post-kill: {names:?}",
    );

    // Sidecar still alive; stop it cleanly.
    stop_gracefully(&mut sidecar, "sidecar");
    assert!(wait_for_unmount(&mp, Duration::from_secs(5)));

    let _ = std::fs::remove_dir_all(&mp);
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_file(&socket);
}
