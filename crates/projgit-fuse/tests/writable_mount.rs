//! Runtime integration test for the writable worktree overlay
//! (`projgit_fuse::WritableFs`, Phase 2 Stage 2).
//!
//! Mirrors `spikes/writable-nofork` but with the PRODUCTION pieces:
//! mount a `WritableFs` over a `ProjectionFsProvider`, seed a real
//! `.git/index` from `dotgit::build_writable_index_bytes` (R1), and
//! drive STOCK git against the mount:
//!
//!   1. clean `git status` (no fork, no core.virtualFilesystem) — also
//!      the end-to-end validation of R1 (clean + hydration-free);
//!   2. edit a file inside the mount -> status reports exactly it ->
//!      `git add` stages it;
//!   3. create a new file -> status reports it untracked -> `git add`.
//!
//! `#[ignore]`-gated: requires `/dev/fuse` (run inside the devcontainer).
//!
//! ```sh
//! cargo test -p projgit-fuse --test writable_mount -- --ignored --nocapture
//! ```

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_core::{
    dotgit, HydratingObjectStore, NoopFetcher, ObjectStore, Projection, ProjectionFsProvider,
    RootOverlay,
};
use projgit_fuse::{mount_writable_background, MountConfig};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new("git")
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
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let (ok, stdout, stderr) = git(cwd, args);
    assert!(ok, "git {args:?} failed: {stderr}");
    stdout
}

/// Build a small source repo and `git clone --no-checkout` it into a
/// `served/` dir (objects + HEAD, no worktree files). Returns
/// `(served_dir, head_oid)`.
fn build_served(name: &str) -> (PathBuf, gix::ObjectId) {
    let base = std::env::temp_dir().join(format!("projgit-wr-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let served = base.join("served");
    std::fs::create_dir_all(&src).unwrap();

    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "core.autocrlf", "false"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::write(src.join("README.md"), b"# fixture repo\n").unwrap();
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/a.txt"), b"alpha\n").unwrap();
    std::fs::write(src.join("dir/b.txt"), b"bravo\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "initial"]);

    let url = format!("file://{}", src.display());
    git_ok(
        &base,
        &["clone", "-q", "--no-checkout", &url, served.to_str().unwrap()],
    );

    let head_hex = git_ok(&served, &["rev-parse", "HEAD"]);
    let head = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).expect("valid hex");
    (served, head)
}

struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_for_mount(mp: &Path, timeout: Duration) -> bool {
    let parent_dev = mp
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(0);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(m) = std::fs::metadata(mp) {
            if m.dev() != parent_dev {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn writable_mount_status_edit_add() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let (served, head) = build_served("status-edit-add");
    let base = served.parent().unwrap().to_path_buf();
    let _guard = DirGuard(base.clone());
    let git_dir = served.join(".git");

    // Seed a writable-mode index (R1) + checkStat=minimal config.
    let store = Arc::new(ObjectStore::open(&served).expect("open store"));
    let index_bytes = dotgit::build_writable_index_bytes(&store, head).expect("writable index");
    std::fs::write(git_dir.join("index"), &index_bytes).expect("seed index");
    git_ok(&served, &["config", "core.checkStat", "minimal"]);
    git_ok(&served, &["config", "core.fsmonitor", "false"]);

    // Build the read-only projection + writable overlay mount.
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(Projection::Commit(head), hydrating, RootOverlay::new(), 1)
            .expect("ProjectionFsProvider::new"),
    );

    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let _session = mount_writable_background(provider, &mnt, &MountConfig::default())
        .expect("mount_writable_background");
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(5)),
        "mountpoint never became a FUSE mount"
    );

    // `git` drives the virtual worktree via an external git-dir.
    let mnt_s = mnt.to_str().unwrap();
    let gd_s = git_dir.to_str().unwrap();
    let gitw = |args: &[&str]| -> String {
        let mut full = vec!["--git-dir", gd_s, "--work-tree", mnt_s, "-c", "safe.directory=*"];
        full.extend_from_slice(args);
        git_ok(&base, &full)
    };

    // ---- 1. clean status (R1 end-to-end: clean, no fork) ----
    let status0 = gitw(&["status", "--porcelain"]);
    assert!(
        status0.trim().is_empty(),
        "fresh writable mount must be clean, got:\n{status0}"
    );

    // ---- 2. edit a tracked file inside the mount ----
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/a.txt"))
            .expect("open dir/a.txt for append");
        f.write_all(b"EDIT\n").expect("append");
    }
    let after_edit = std::fs::read_to_string(mnt.join("dir/a.txt")).expect("read back");
    assert_eq!(after_edit, "alpha\nEDIT\n", "edit must be visible in the mount");

    let status1 = gitw(&["status", "--porcelain"]);
    assert_eq!(
        status1.trim(),
        "M dir/a.txt",
        "status must report exactly the edited file, got:\n{status1}"
    );
    gitw(&["add", "dir/a.txt"]);
    let staged = gitw(&["diff", "--cached", "--name-only"]);
    assert_eq!(staged.trim(), "dir/a.txt", "edited file must stage");

    // ---- 3. create a new file inside the mount ----
    std::fs::write(mnt.join("dir/new.txt"), b"fresh\n").expect("create new file");
    let listing = std::fs::read_dir(mnt.join("dir"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        listing.contains("new.txt") && listing.contains("a.txt") && listing.contains("b.txt"),
        "readdir must merge created + lower entries, got {listing:?}"
    );
    let status2 = gitw(&["status", "--porcelain"]);
    assert!(
        status2.contains("?? dir/new.txt"),
        "new file must show untracked, got:\n{status2}"
    );
    gitw(&["add", "dir/new.txt"]);
    let staged2 = gitw(&["diff", "--cached", "--name-only"]);
    assert!(
        staged2.contains("dir/new.txt"),
        "created file must stage, got:\n{staged2}"
    );

    // ---- 4. untouched file is still served from the lower projection ----
    let bravo = std::fs::read_to_string(mnt.join("dir/b.txt")).expect("read untouched");
    assert_eq!(bravo, "bravo\n", "untouched file stays virtual + correct");
}
