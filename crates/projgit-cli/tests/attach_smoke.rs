//! Stage 2c end-to-end test: spawn the daemon (via its library API,
//! inside this test process), exercise `projgit attach` as a real
//! subprocess against it, verify the round-trip works for every
//! subcommand.
//!
//! Spawning `projgit attach` (not the binary directly) is the point —
//! this test covers the CLI side, not the daemon side (which has its
//! own tests in `crates/projgit-daemon/tests/`).
//!
//! `#[ignore]`-gated because the mount-test arm needs FUSE; the
//! ping/status/shutdown arm doesn't, but keeping them in one binary
//! makes the test simpler.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_daemon::server::{run, DaemonConfig};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PROJGIT_BIN: &str = env!("CARGO_BIN_EXE_projgit");

fn temp_path(label: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "projgit-cli-attach-{label}-{}{suffix}",
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
    };
    let handle = thread::spawn(move || run(config));
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if socket_path.exists() {
            return (socket_path, handle);
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("daemon never came up");
}

/// Run `projgit attach --socket <sock> <args>` and return (status,
/// stdout). Fails the test on non-zero status unless `allow_err`.
fn attach(socket: &PathBuf, args: &[&str], allow_err: bool) -> (std::process::ExitStatus, String) {
    let output = Command::new(PROJGIT_BIN)
        .arg("attach")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn projgit attach");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() && !allow_err {
        panic!(
            "projgit attach {args:?} failed (status={}):\nstdout={stdout}\nstderr={stderr}",
            output.status
        );
    }
    (output.status, stdout)
}

#[test]
fn attach_ping_status_shutdown_via_cli() {
    let (sock, handle) = spawn_daemon("ping");

    let (_, stdout) = attach(&sock, &["ping"], false);
    assert!(stdout.contains("pong"), "expected `pong` in stdout: {stdout}");

    let (_, stdout) = attach(&sock, &["status"], false);
    // No source attached → status text reflects that.
    assert!(
        stdout.contains("(no Mount request yet)"),
        "status output unexpected: {stdout}",
    );
    assert!(stdout.contains("mounts    : 0"));

    let (_, _) = attach(&sock, &["shutdown"], false);

    // Daemon should exit on its own after shutdown.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() {
        if Instant::now() > deadline {
            panic!("daemon never exited after shutdown");
        }
        thread::sleep(Duration::from_millis(25));
    }
    handle.join().unwrap().unwrap();
}

#[test]
#[ignore = "requires FUSE + git CLI; run inside the devcontainer"]
fn attach_full_mount_lifecycle_via_cli() {
    use std::process::Command;

    // Build a tiny fixture repo with the system git CLI.
    let repo = temp_path("fixture", "");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    let run_git = |args: &[&str]| {
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
        assert!(out.status.success(), "git {args:?}: {:?}", out);
    };
    run_git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), b"contents from cli test\n").unwrap();
    run_git(&["add", "-A"]);
    run_git(&["commit", "-q", "-m", "init"]);

    let mp = temp_path("mp", "");
    std::fs::create_dir_all(&mp).unwrap();

    let (sock, handle) = spawn_daemon("mount");

    // mount
    attach(
        &sock,
        &[
            "mount",
            repo.to_str().unwrap(),
            "--ref",
            "main",
            "--mountpoint",
            mp.to_str().unwrap(),
            "--no-dotgit",
        ],
        false,
    );

    // Verify the mount is live by reading the file from the kernel.
    use std::os::unix::fs::MetadataExt;
    let parent_dev = mp
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(m) = std::fs::metadata(&mp) {
            if m.dev() != parent_dev {
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    let content = std::fs::read_to_string(mp.join("file.txt")).expect("read mounted file");
    assert_eq!(content, "contents from cli test\n");

    // status — should show 1 mount
    let (_, stdout) = attach(&sock, &["status"], false);
    assert!(stdout.contains("mounts    : 1"), "expected one mount: {stdout}");

    // umount
    attach(
        &sock,
        &["umount", "--mountpoint", mp.to_str().unwrap()],
        false,
    );

    // umount of a path that was never registered should exit non-zero.
    let (status, _) = attach(
        &sock,
        &["umount", "--mountpoint", "/tmp"],
        true,
    );
    assert!(
        !status.success(),
        "umount of unknown path should fail; got {status}",
    );

    // shutdown
    attach(&sock, &["shutdown"], false);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !handle.is_finished() {
        if Instant::now() > deadline {
            panic!("daemon never exited");
        }
        thread::sleep(Duration::from_millis(25));
    }
    handle.join().unwrap().unwrap();

    let _ = std::fs::remove_dir_all(&mp);
    let _ = std::fs::remove_dir_all(&repo);
}
