//! Stage 3 **cross-mount-namespace** end-to-end test.
//!
//! Spawns the daemon in the parent mount namespace and the sidecar
//! inside `unshare --user --map-root-user --mount --propagation=private`,
//! the closest in-CI proxy for "daemon on the host, sidecar in a
//! container" without requiring docker. Confirms:
//!
//! 1. The sidecar can `mount(2)` a FUSE filesystem inside an
//!    isolated mount namespace (works because the user-namespace
//!    grants fake-CAP_SYS_ADMIN inside the namespace).
//! 2. The sidecar can connect to the parent-namespace daemon's
//!    unix socket — sockets work across mount-ns by construction;
//!    user-ns credential mapping doesn't interfere with same-host
//!    UID-checked peer credentials when the socket lives on a
//!    bind-shared filesystem (`/tmp`).
//! 3. Files served by the sidecar are readable *inside* its
//!    namespace; the mount is torn down cleanly when the namespace
//!    exits (kernel-guaranteed, not asserted here).
//!
//! Doubly gated: `#[ignore]` (FUSE + git CLI) **and** a runtime
//! probe of `unshare --user --map-root-user --mount` — many CI
//! environments and rootless containers disable unprivileged user
//! namespaces (`/proc/sys/kernel/unprivileged_userns_clone=0`),
//! and the test skips with a clear message there rather than
//! failing.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Locate sibling binaries (same shape as xprocess_mount_smoke.rs)
// -----------------------------------------------------------------------------

fn target_profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(Path::parent)
        .expect("test binary path has a profile dir grandparent")
        .to_path_buf()
}

fn projgitd_binary() -> PathBuf {
    target_profile_dir().join("projgitd")
}

fn projgit_binary() -> PathBuf {
    target_profile_dir().join("projgit")
}

fn ensure_binaries_built() {
    if projgitd_binary().exists() && projgit_binary().exists() {
        return;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "projgit-daemon", "-p", "projgit-cli"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed: {status}");
}

// -----------------------------------------------------------------------------
// Probes
// -----------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Probe whether `unshare --user --map-root-user --mount` actually
/// works for this user on this kernel. Returns `true` if yes;
/// `false` (with the test skipping) otherwise.
fn unshare_userns_mount_works() -> bool {
    Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// -----------------------------------------------------------------------------
// Fixture
// -----------------------------------------------------------------------------

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
        "projgit-xns-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    std::fs::write(base.join("hello.txt"), b"hello from a private mount namespace\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "initial"]);
    base
}

fn temp_path(prefix: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "projgit-xns-{prefix}-{}{suffix}",
        std::process::id()
    ))
}

// -----------------------------------------------------------------------------
// Daemon helper (parent-namespace daemon — same shape as xprocess test)
// -----------------------------------------------------------------------------

/// Same shape as `xprocess_mount_smoke::spawn_daemon_process`.
/// `#[allow(clippy::zombie_processes)]`: caller owns the Child;
/// panic paths kill + wait before bailing.
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
    panic!("projgitd never created {}", socket.display());
}

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
// Test
// -----------------------------------------------------------------------------

#[test]
#[ignore = "requires FUSE + git CLI + unprivileged user namespaces; runs the \
            sidecar inside `unshare --user --map-root-user --mount`"]
fn sidecar_in_private_mount_namespace_serves_files() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    if !unshare_userns_mount_works() {
        eprintln!(
            "SKIP: `unshare --user --map-root-user --mount` not permitted \
             (likely kernel.unprivileged_userns_clone=0 or no userns support)"
        );
        return;
    }
    ensure_binaries_built();

    let repo = build_fixture("ns");
    let socket = temp_path("ns", ".sock");
    let _ = std::fs::remove_file(&socket);
    let mp = temp_path("ns-mp", "");
    std::fs::create_dir_all(&mp).unwrap();

    // Marker the in-namespace script writes once it has read the
    // file through the FUSE mount. We assert on its content from
    // the parent namespace to prove the sidecar served the bytes.
    let marker = temp_path("ns-marker", ".txt");
    let _ = std::fs::remove_file(&marker);

    let mut daemon = spawn_daemon_process(&socket);

    // The sidecar runs inside a private mount namespace mapped as
    // fake-root via user-namespace. The script:
    //   1. Spawns `projgit mount --daemon-socket … --ref main`
    //   2. Polls until the FUSE mount becomes visible inside this
    //      namespace (st_dev change).
    //   3. Reads MP/hello.txt and writes the bytes to MARKER (in
    //      bind-shared /tmp so the parent test can see it).
    //   4. SIGINT's the sidecar; the FUSE session drops on Drop;
    //      the mount goes away when the namespace exits anyway.
    //
    // The script must do everything inside the namespace because
    // anything mounted in the namespace is *only* visible to
    // processes that share that namespace. The parent test
    // process never sees the FUSE mount — that's the whole point
    // of the topology being tested.
    let projgit = projgit_binary().to_string_lossy().into_owned();
    let source = repo.to_string_lossy().into_owned();
    let mp_str = mp.to_string_lossy().into_owned();
    let sock_str = socket.to_string_lossy().into_owned();
    let marker_str = marker.to_string_lossy().into_owned();
    let script = format!(
        r#"
set -e
"{projgit}" mount --daemon-socket "{sock_str}" --ref main --no-dotgit "{source}" "{mp_str}" >/dev/null 2>&1 &
P=$!
trap 'kill -INT $P 2>/dev/null; wait $P 2>/dev/null; true' EXIT
parent_dev=$(stat -c %d "$(dirname "{mp_str}")")
mount_dev=$parent_dev
for i in $(seq 1 60); do
    sleep 0.1
    mount_dev=$(stat -c %d "{mp_str}" 2>/dev/null || echo "$parent_dev")
    if [ "$mount_dev" != "$parent_dev" ]; then
        break
    fi
done
if [ "$mount_dev" = "$parent_dev" ]; then
    echo "MOUNT_TIMEOUT" >"{marker_str}"
    exit 1
fi
cat "{mp_str}/hello.txt" >"{marker_str}"
"#
    );

    let status = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--propagation=private"])
        .args(["bash", "-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn unshare");

    // Stop the daemon before any assertions so a panic later still
    // cleans up the background process.
    stop_gracefully(&mut daemon, "daemon");

    assert!(status.success(), "unshare script failed: {status}");

    let marker_content =
        std::fs::read_to_string(&marker).expect("in-namespace script must write marker");
    assert_eq!(
        marker_content, "hello from a private mount namespace\n",
        "sidecar in private mount namespace must serve the fixture file",
    );

    // The parent namespace never saw the FUSE mount. (Kernel
    // guarantees this for `--propagation=private`; we don't probe
    // mid-flight because the mount tears down when the script
    // exits, so there's no observable window — but the namespace
    // boundary itself is the load-bearing test, not a stat probe.)

    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&mp);
    let _ = std::fs::remove_dir_all(&repo);
}
