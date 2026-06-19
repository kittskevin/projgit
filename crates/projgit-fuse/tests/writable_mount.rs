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
use projgit_fuse::{mount_writable_background, mount_writable_background_with_handle, MountConfig};
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

    // ---- 3b. commit the staged changes and verify (Stage 6) ----
    gitw(&["commit", "-q", "-m", "spike: edit + new file"]);
    let head_a = gitw(&["cat-file", "-p", "HEAD:dir/a.txt"]);
    assert!(
        head_a.contains("EDIT"),
        "committed dir/a.txt must carry the edit, got:\n{head_a}"
    );
    let head_new = gitw(&["cat-file", "-p", "HEAD:dir/new.txt"]);
    assert_eq!(
        head_new.trim(),
        "fresh",
        "committed dir/new.txt must have the created content"
    );
    let post_commit = gitw(&["status", "--porcelain"]);
    assert!(
        post_commit.trim().is_empty(),
        "after commit the worktree is clean again, got:\n{post_commit}"
    );

    // ---- 4. untouched file is still served from the lower projection ----
    let bravo = std::fs::read_to_string(mnt.join("dir/b.txt")).expect("read untouched");
    assert_eq!(bravo, "bravo\n", "untouched file stays virtual + correct");

    // ---- 5. same-SIZE in-place edit: detection depends on the Stage 3
    //         FUSE cache invalidation (size is unchanged, so only the
    //         mtime change distinguishes it, and the kernel would
    //         otherwise serve a cached getattr within the TTL). ----
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(mnt.join("dir/b.txt"))
            .expect("open dir/b.txt for in-place write");
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"BRAVO\n").expect("same-size overwrite"); // 6 bytes == "bravo\n"
    }
    // Give the off-thread invalidator a beat to reach the kernel.
    std::thread::sleep(Duration::from_millis(100));
    let status3 = gitw(&["status", "--porcelain"]);
    assert!(
        status3.contains(" M dir/b.txt"),
        "same-size edit must be detected (Stage 3 invalidation), got:\n{status3}"
    );
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn writable_mount_fsmonitor_write_log() {
    // Stage 4 (R3): the overlay's write-log answers a core.fsmonitor hook
    // so git can skip scanning, and an edit is still reported via the log.
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let (served, head) = build_served("fsmonitor");
    let base = served.parent().unwrap().to_path_buf();
    let _guard = DirGuard(base.clone());
    let git_dir = served.join(".git");
    let fsm = base.join("fsm-log");
    let hook = base.join("fsmonitor-hook.sh");

    let store = Arc::new(ObjectStore::open(&served).expect("open store"));
    let index_bytes = dotgit::build_writable_index_bytes(&store, head).expect("writable index");
    std::fs::write(git_dir.join("index"), &index_bytes).expect("seed index");

    // A core.fsmonitor hook (query protocol v2) that streams the overlay
    // write-log verbatim. In production projgit installs this; the daemon
    // answers from its authoritative write log.
    std::fs::write(
        &hook,
        "#!/bin/sh\nif [ -n \"$VWORKTREE_FSM\" ] && [ -s \"$VWORKTREE_FSM\" ]; then cat \"$VWORKTREE_FSM\"; else printf '%s\\0' 0; fi\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(Projection::Commit(head), hydrating, RootOverlay::new(), 1)
            .expect("ProjectionFsProvider::new"),
    );

    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let cfg = MountConfig {
        fsmonitor_file: Some(fsm.clone()),
        ..MountConfig::default()
    };
    let _session =
        mount_writable_background(provider, &mnt, &cfg).expect("mount_writable_background");
    assert!(wait_for_mount(&mnt, Duration::from_secs(5)), "never mounted");

    let mnt_s = mnt.to_str().unwrap();
    let gd_s = git_dir.to_str().unwrap();
    let fsm_s = fsm.to_str().unwrap();
    let hook_s = hook.to_str().unwrap();
    // git invocation that exports VWORKTREE_FSM so the hook can read the log.
    let run_git = |args: &[&str]| -> String {
        let mut full = vec!["--git-dir", gd_s, "--work-tree", mnt_s, "-c", "safe.directory=*"];
        full.extend_from_slice(args);
        let out = Command::new("git")
            .args(&full)
            .current_dir(&base)
            .env("VWORKTREE_FSM", fsm_s)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    run_git(&["config", "core.checkStat", "minimal"]);
    run_git(&["config", "core.fsmonitorHookVersion", "2"]);
    run_git(&["config", "core.fsmonitor", hook_s]);
    // Persist an fsmonitor baseline into the index, then settle.
    run_git(&["update-index", "--refresh"]);
    run_git(&["status", "--porcelain"]);

    // Clean mount + fsmonitor => clean status (no false positives).
    let clean = run_git(&["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "fsmonitor clean status must be empty, got:\n{clean}"
    );

    // Edit a file: the overlay records it in the write-log; the hook
    // reports it; git detects it (one settle query absorbs the documented
    // post-change lag).
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/a.txt"))
            .unwrap();
        f.write_all(b"VIA-FSMONITOR\n").unwrap();
    }
    std::thread::sleep(Duration::from_millis(100));
    let log = std::fs::read(&fsm).unwrap();
    let log_str = String::from_utf8_lossy(&log);
    assert!(
        log_str.contains("dir/a.txt"),
        "write-log must list the edited path, got: {log_str:?}"
    );
    run_git(&["status", "--porcelain"]); // settle
    let detected = run_git(&["status", "--porcelain"]);
    assert!(
        detected.contains("M dir/a.txt"),
        "fsmonitor must surface the edit, got:\n{detected}"
    );
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn writable_mount_sparse_cone_hides_out_of_cone() {
    // Stage 5 (R2): with a sparse cone configured, the projection must not
    // surface out-of-cone paths (which is what keeps git's sparse-index
    // collapsed instead of expanding to a full index — see the spike).
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let base = std::env::temp_dir().join(format!("projgit-wr-cone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let served = base.join("served");
    std::fs::create_dir_all(&src).unwrap();
    let _guard = DirGuard(base.clone());

    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::write(src.join("README.md"), b"# root\n").unwrap();
    std::fs::create_dir_all(src.join("dirA")).unwrap();
    std::fs::write(src.join("dirA/a.txt"), b"alpha\n").unwrap();
    std::fs::create_dir_all(src.join("dirB/sub")).unwrap();
    std::fs::write(src.join("dirB/b.txt"), b"bravo\n").unwrap();
    std::fs::write(src.join("dirB/sub/c.txt"), b"charlie\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);
    let url = format!("file://{}", src.display());
    git_ok(
        &base,
        &["clone", "-q", "--no-checkout", &url, served.to_str().unwrap()],
    );
    let head_hex = git_ok(&served, &["rev-parse", "HEAD"]);
    let head = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).unwrap();

    let store = Arc::new(ObjectStore::open(&served).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(Projection::Commit(head), hydrating, RootOverlay::new(), 1)
            .expect("ProjectionFsProvider::new"),
    );

    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let cfg = MountConfig {
        sparse_cone: vec!["dirA".to_string()],
        ..MountConfig::default()
    };
    let _session =
        mount_writable_background(provider, &mnt, &cfg).expect("mount_writable_background");
    assert!(wait_for_mount(&mnt, Duration::from_secs(5)), "never mounted");

    // Root shows README.md + the cone dir, but NOT the out-of-cone dirB.
    let root: std::collections::BTreeSet<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(root.contains("README.md"), "root files stay visible: {root:?}");
    assert!(root.contains("dirA"), "cone dir is visible: {root:?}");
    assert!(
        !root.contains("dirB"),
        "out-of-cone dir must be hidden, got {root:?}"
    );

    // The cone dir's contents are fully visible.
    let a: std::collections::BTreeSet<String> = std::fs::read_dir(mnt.join("dirA"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(a.contains("a.txt"), "cone dir contents visible: {a:?}");

    // Out-of-cone paths are not reachable at all.
    assert!(
        !mnt.join("dirB").exists(),
        "out-of-cone dir must not be stat-able"
    );
    assert!(
        std::fs::read_dir(mnt.join("dirB")).is_err(),
        "out-of-cone dir must not be listable"
    );
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn writable_mount_swap_baseline_serves_new_commit() {
    // Stage 7: swap the LOWER baseline under a live mount (a checkout of a
    // different commit) and prove unmodified files re-virtualize to the
    // new baseline, with the kernel caches invalidated.
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let base = std::env::temp_dir().join(format!("projgit-wr-swap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let served = base.join("served");
    std::fs::create_dir_all(&src).unwrap();
    let _guard = DirGuard(base.clone());

    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    // commit A
    std::fs::write(src.join("README.md"), b"A\n").unwrap();
    std::fs::write(src.join("dir/x.txt"), b"one\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "A"]);
    // commit B: change both files + add a new one
    std::fs::write(src.join("README.md"), b"B\n").unwrap();
    std::fs::write(src.join("dir/x.txt"), b"two\n").unwrap();
    std::fs::write(src.join("dir/y.txt"), b"new\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "B"]);

    let url = format!("file://{}", src.display());
    git_ok(
        &base,
        &["clone", "-q", "--no-checkout", &url, served.to_str().unwrap()],
    );
    let head_b = gix::ObjectId::from_hex(git_ok(&served, &["rev-parse", "HEAD"]).trim().as_bytes())
        .unwrap();
    let head_a =
        gix::ObjectId::from_hex(git_ok(&served, &["rev-parse", "HEAD~1"]).trim().as_bytes())
            .unwrap();

    let make_provider = |commit: gix::ObjectId, id: u64| {
        let store = Arc::new(ObjectStore::open(&served).unwrap());
        let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
        Arc::new(
            ProjectionFsProvider::new(Projection::Commit(commit), hydrating, RootOverlay::new(), id)
                .expect("ProjectionFsProvider::new"),
        )
    };

    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let (_session, handle) =
        mount_writable_background_with_handle(make_provider(head_a, 1), &mnt, &MountConfig::default())
            .expect("mount_writable_background_with_handle");
    assert!(wait_for_mount(&mnt, Duration::from_secs(5)), "never mounted");

    // Baseline A.
    assert_eq!(
        std::fs::read_to_string(mnt.join("README.md")).unwrap(),
        "A\n"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("dir/x.txt")).unwrap(),
        "one\n"
    );
    assert!(!mnt.join("dir/y.txt").exists(), "y.txt is not in commit A");

    // Swap to commit B under the live mount.
    handle
        .swap_baseline(make_provider(head_b, 2))
        .expect("clean swap should succeed");
    // Let the off-thread invalidator reach the kernel.
    std::thread::sleep(Duration::from_millis(250));

    // The mount now re-virtualizes to baseline B.
    assert_eq!(
        std::fs::read_to_string(mnt.join("README.md")).unwrap(),
        "B\n",
        "swapped baseline must serve commit B's README"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("dir/x.txt")).unwrap(),
        "two\n",
        "swapped baseline must serve commit B's dir/x.txt"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("dir/y.txt")).unwrap(),
        "new\n",
        "file added in commit B must appear after the swap"
    );

    // A swap with outstanding edits is refused (EdenFS-style edit-survival
    // across checkout is a documented Stage 7 follow-up).
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("README.md"))
            .unwrap();
        f.write_all(b"local edit\n").unwrap();
    }
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        handle.swap_baseline(make_provider(head_a, 3)).is_err(),
        "swap must be refused while the overlay has outstanding edits"
    );
}
