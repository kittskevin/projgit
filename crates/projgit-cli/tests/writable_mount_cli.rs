//! End-to-end test for `projgit mount --writable` (Phase 2): spawn the
//! real CLI binary, mount a local repo read-write, and drive STOCK git
//! inside the mount — clean `status`, edit + new file, `add`, `commit`,
//! and verify the committed content.
//!
//! `#[ignore]`-gated: requires `/dev/fuse` (run inside the devcontainer):
//!
//! ```sh
//! cargo test -p projgit-cli --test writable_mount_cli -- --ignored --nocapture
//! ```

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const PROJGIT_BIN: &str = env!("CARGO_BIN_EXE_projgit");

fn git(cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let (ok, stdout) = git(cwd, args);
    assert!(ok, "git {args:?} failed");
    stdout
}

/// Count objects missing from `git_dir`'s odb (promised by the promisor)
/// — i.e. blobs not yet hydrated in a partial clone.
fn count_missing_objects(git_dir: &Path) -> usize {
    let (_ok, out) = git(
        git_dir,
        &["rev-list", "--objects", "--all", "--missing=print"],
    );
    out.lines().filter(|l| l.starts_with('?')).count()
}

/// The projgit cache's clone directory (the one entry that isn't the
/// `worktrees/` scratch dir).
fn find_clone_dir(cache: &Path) -> PathBuf {
    std::fs::read_dir(cache)
        .expect("read cache dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()) != Some("worktrees"))
        .expect("clone dir under cache")
}

/// The upper journal's content-addressed `blobs/` directory (under the
/// reused scratch git dir), if it exists.
fn find_upper_blobs(cache: &Path) -> Option<PathBuf> {
    for e in std::fs::read_dir(cache.join("worktrees")).ok()?.flatten() {
        let blobs = e.path().join("projgit-upper").join("blobs");
        if blobs.is_dir() {
            return Some(blobs);
        }
    }
    None
}

/// Count non-temp files in `dir`.
fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with(".tmp-"))
                .count()
        })
        .unwrap_or(0)
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
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Wait until `mp` is no longer a FUSE mount (back on its parent fs, or
/// gone). Used between an unmount and a remount.
fn wait_for_unmount(mp: &Path, timeout: Duration) -> bool {
    let parent_dev = mp
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(0);
    let start = Instant::now();
    while start.elapsed() < timeout {
        match std::fs::metadata(mp) {
            Ok(m) if m.dev() == parent_dev => return true,
            Err(_) => return true,
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Spawn `projgit mount --writable --cache-dir <cache> <src> <mnt>`.
/// The explicit cache dir keeps the persistent worktree git dir under
/// the test's own tree (hermetic + cleaned by `DirGuard`).
fn spawn_writable(src: &Path, mnt: &Path, cache: &Path) -> std::process::Child {
    Command::new(PROJGIT_BIN)
        .args(["mount", "--writable", "--cache-dir"])
        .arg(cache)
        .arg(src)
        .arg(mnt)
        .spawn()
        .expect("spawn projgit mount --writable")
}

/// Poll `path` until its contents equal `expected` (or timeout).
fn wait_for_content(path: &Path, expected: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(c) = std::fs::read_to_string(path) {
            if c == expected {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Poll `cond` until it is true (or timeout).
fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_mount_edit_add_commit() {
    let base = std::env::temp_dir().join(format!("projgit-cli-wr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["--version"]); // git present?
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/f.txt"), b"hello\nworld\n").unwrap();
    std::fs::write(src.join("README.md"), b"readme\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);

    // Spawn the real CLI: `projgit mount --writable <src> <mnt>`.
    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "writable mount never came up"
    );

    // Fresh writable mount is clean.
    let s0 = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(s0.trim().is_empty(), "fresh writable mount must be clean:\n{s0}");

    // Edit a tracked file + create a new one inside the mount.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/f.txt"))
            .unwrap();
        f.write_all(b"EDIT\n").unwrap();
    }
    std::fs::write(mnt.join("dir/new.txt"), b"fresh\n").unwrap();

    let s1 = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(s1.contains("M dir/f.txt"), "edit must show modified:\n{s1}");
    assert!(s1.contains("?? dir/new.txt"), "new file must show untracked:\n{s1}");

    // Stage + commit through stock git inside the mount.
    git_ok(&mnt, &["add", "-A"]);
    let (ok, _) = git(&mnt, &["commit", "-m", "edit via writable mount"]);
    assert!(ok, "commit must succeed");

    let committed = git_ok(&mnt, &["cat-file", "-p", "HEAD:dir/f.txt"]);
    assert_eq!(committed, "hello\nworld\nEDIT\n", "committed content must carry the edit");
    let new_committed = git_ok(&mnt, &["cat-file", "-p", "HEAD:dir/new.txt"]);
    assert_eq!(new_committed, "fresh\n", "new file must be committed");

    // Tear down the mount.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_mount_commit_and_push_to_branch() {
    // The full dev loop: mount a source that has a remote, edit + commit
    // on its branch inside the mount, then `git push` to the remote.
    let base = std::env::temp_dir().join(format!("projgit-cli-push-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let remote = base.join("remote.git");
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    // Bare remote.
    std::fs::create_dir_all(&remote).unwrap();
    let (ok, _) = git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    assert!(ok, "git init --bare");

    // Source repo with `origin` -> the bare remote; initial commit pushed.
    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/f.txt"), b"hello\nworld\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);
    git_ok(&src, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git_ok(&src, &["push", "-q", "-u", "origin", "main"]);

    // Mount the source writable.
    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "writable mount never came up"
    );

    // The mount is on the branch 'main' with 'origin' configured.
    let branch = git_ok(&mnt, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(branch.trim(), "main", "writable mount must be on branch main");
    let origin = git_ok(&mnt, &["remote", "get-url", "origin"]);
    assert_eq!(
        origin.trim(),
        remote.to_str().unwrap(),
        "origin must point at the source's remote"
    );

    // Edit + commit on the branch inside the mount.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/f.txt"))
            .unwrap();
        f.write_all(b"PUSHED\n").unwrap();
    }
    git_ok(&mnt, &["add", "-A"]);
    let (ok, _) = git(&mnt, &["commit", "-m", "edit + push via writable mount"]);
    assert!(ok, "commit must succeed");

    // Push to the remote.
    let (ok, push_out) = git(&mnt, &["push", "-q", "origin", "main"]);
    assert!(ok, "git push must succeed:\n{push_out}");

    // The bare remote now has the new commit + content.
    let (ok, remote_content) = git(&remote, &["cat-file", "-p", "main:dir/f.txt"]);
    assert!(ok, "reading pushed file from remote");
    assert_eq!(
        remote_content, "hello\nworld\nPUSHED\n",
        "the remote branch must carry the edit pushed from the writable mount"
    );
    let (_ok, remote_log) = git(&remote, &["log", "--oneline"]);
    assert!(
        remote_log.contains("edit + push via writable mount"),
        "remote branch must have the commit:\n{remote_log}"
    );

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_mount_persists_commit_across_remount() {
    // Committed work survives an unmount: commit inside a writable mount,
    // unmount, then remount the SAME mountpoint and find HEAD, the index,
    // and the committed content all restored (the scratch git dir + its
    // objects — shared into the CAS — are reused, not recreated).
    let base = std::env::temp_dir().join(format!("projgit-cli-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    // Source repo with one commit.
    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/f.txt"), b"hello\nworld\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);

    // --- First mount: edit a tracked file, add a new one, commit. ---
    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount 1 never came up"
    );
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/f.txt"))
            .unwrap();
        f.write_all(b"PERSIST\n").unwrap();
    }
    std::fs::write(mnt.join("dir/new.txt"), b"brand new\n").unwrap();
    git_ok(&mnt, &["add", "-A"]);
    let (ok, _) = git(&mnt, &["commit", "-m", "committed work"]);
    assert!(ok, "commit must succeed");
    let head1 = git_ok(&mnt, &["rev-parse", "HEAD"]).trim().to_string();

    // Unmount and wait for the mountpoint to drop back to its parent fs.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        wait_for_unmount(&mnt, Duration::from_secs(10)),
        "mount 1 never unmounted"
    );

    // --- Second mount at the SAME mountpoint + cache: work restored. ---
    let mut child2 = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount 2 never came up"
    );

    // HEAD is the commit made in the first session.
    let head2 = git_ok(&mnt, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(head2, head1, "remount must restore the committed HEAD");

    // Clean status, and the committed content is present in the worktree.
    let s = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(
        s.trim().is_empty(),
        "remount of committed work must be clean:\n{s}"
    );
    let f = std::fs::read_to_string(mnt.join("dir/f.txt")).unwrap();
    assert_eq!(f, "hello\nworld\nPERSIST\n", "edited file must survive remount");
    let n = std::fs::read_to_string(mnt.join("dir/new.txt")).unwrap();
    assert_eq!(n, "brand new\n", "new file must survive remount");
    let committed = git_ok(&mnt, &["cat-file", "-p", "HEAD:dir/f.txt"]);
    assert_eq!(committed, "hello\nworld\nPERSIST\n");

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child2.kill();
    let _ = child2.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_mount_persists_uncommitted_edits_across_remount() {
    // Uncommitted work survives an unmount too: edit + create WITHOUT
    // committing, unmount, then remount and find the materialized upper
    // (edited bytes + new file) restored and `status` still dirty — the
    // upper crash journal is replayed and reconciled against the baseline.
    let base = std::env::temp_dir().join(format!("projgit-cli-draft-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    // Source repo with one commit.
    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/f.txt"), b"hello\nworld\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);

    // --- First mount: edit + create, but DO NOT commit. ---
    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount 1 never came up"
    );
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/f.txt"))
            .unwrap();
        f.write_all(b"DRAFT\n").unwrap();
    }
    std::fs::write(mnt.join("dir/draft.txt"), b"wip\n").unwrap();
    let head1 = git_ok(&mnt, &["rev-parse", "HEAD"]).trim().to_string();
    let s1 = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(s1.contains("M dir/f.txt"), "pre-unmount edit must show:\n{s1}");
    assert!(
        s1.contains("?? dir/draft.txt"),
        "pre-unmount new file must show:\n{s1}"
    );

    // Unmount without committing.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        wait_for_unmount(&mnt, Duration::from_secs(10)),
        "mount 1 never unmounted"
    );

    // --- Second mount: uncommitted edits restored from the journal. ---
    let mut child2 = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount 2 never came up"
    );

    // No commit happened, so HEAD is unchanged.
    let head2 = git_ok(&mnt, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(head2, head1, "HEAD must not have moved (no commit)");

    // The materialized bytes are back in the worktree...
    let f = std::fs::read_to_string(mnt.join("dir/f.txt")).unwrap();
    assert_eq!(f, "hello\nworld\nDRAFT\n", "uncommitted edit must be restored");
    let d = std::fs::read_to_string(mnt.join("dir/draft.txt")).unwrap();
    assert_eq!(d, "wip\n", "uncommitted new file must be restored");

    // ...and git still sees them as pending changes.
    let s2 = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(
        s2.contains("M dir/f.txt"),
        "restored edit must still be modified:\n{s2}"
    );
    assert!(
        s2.contains("?? dir/draft.txt"),
        "restored new file must still be untracked:\n{s2}"
    );

    // And they can now be committed normally.
    git_ok(&mnt, &["add", "-A"]);
    let (ok, _) = git(&mnt, &["commit", "-m", "commit the restored draft"]);
    assert!(ok, "restored edits must be committable");
    let committed = git_ok(&mnt, &["cat-file", "-p", "HEAD:dir/f.txt"]);
    assert_eq!(committed, "hello\nworld\nDRAFT\n");

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child2.kill();
    let _ = child2.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_checkout_reprojects_under_live_mount() {
    // `projgit checkout` switches a live writable mount to another branch
    // without rewriting the worktree: the mount's HEAD watcher swaps the
    // LOWER baseline, so the worktree re-virtualizes to the new commit and
    // local edits survive (path-keyed upper).
    let base = std::env::temp_dir().join(format!("projgit-cli-co-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    // Two branches that differ: `main` and `feature`.
    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/f.txt"), b"main-one\n").unwrap();
    std::fs::write(src.join("main-only.txt"), b"m\n").unwrap();
    std::fs::write(src.join("shared.txt"), b"shared\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "main"]);

    git_ok(&src, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("dir/f.txt"), b"feature-two\n").unwrap();
    std::fs::remove_file(src.join("main-only.txt")).unwrap();
    std::fs::write(src.join("feat-only.txt"), b"f\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "feature"]);
    git_ok(&src, &["checkout", "-q", "main"]);

    // Mount writable on `main`.
    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "writable mount never came up"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("dir/f.txt")).unwrap(),
        "main-one\n"
    );
    assert!(mnt.join("main-only.txt").exists());
    assert!(!mnt.join("feat-only.txt").exists());

    // Materialize a local edit to a file that is identical in both
    // branches — it must survive the checkout.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("shared.txt"))
            .unwrap();
        f.write_all(b"EDIT\n").unwrap();
    }

    // Switch to `feature` via projgit (no worktree rewrite).
    let out = Command::new(PROJGIT_BIN)
        .args(["checkout", "-C"])
        .arg(&mnt)
        .arg("feature")
        .output()
        .expect("spawn projgit checkout");
    assert!(
        out.status.success(),
        "projgit checkout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The HEAD watcher re-projects the worktree to `feature`.
    assert!(
        wait_for_content(&mnt.join("dir/f.txt"), "feature-two\n", Duration::from_secs(10)),
        "worktree never re-projected to feature"
    );
    assert!(
        mnt.join("feat-only.txt").exists(),
        "feature-only file must appear after checkout"
    );
    assert!(
        !mnt.join("main-only.txt").exists(),
        "main-only file must disappear after checkout"
    );

    // The local edit survived the checkout (shadows feature's shared.txt).
    assert_eq!(
        std::fs::read_to_string(mnt.join("shared.txt")).unwrap(),
        "shared\nEDIT\n",
        "local edit must survive the checkout"
    );

    // git agrees: HEAD is on feature, and only shared.txt is modified.
    assert_eq!(
        git_ok(&mnt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feature"
    );
    let s = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(
        s.contains("M shared.txt"),
        "only the surviving edit should be dirty:\n{s}"
    );
    assert!(
        !s.contains("dir/f.txt"),
        "re-projected files must be clean:\n{s}"
    );

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_reconcile_drops_edit_the_baseline_carries() {
    // reconcile-on-swap: an edit whose content matches the branch you
    // check out is dropped from the upper, so it does NOT shadow a later
    // checkout of a branch that lacks it. (A change that becomes part of
    // the baseline must not behave like a phantom local edit.)
    let base = std::env::temp_dir().join(format!("projgit-cli-reconcile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    // `main`: x.txt = "base"; `feature`: x.txt = "edited" + a marker file.
    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::write(src.join("x.txt"), b"base\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "main"]);
    git_ok(&src, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("x.txt"), b"edited\n").unwrap();
    std::fs::write(src.join("marker.txt"), b"F\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "feature"]);
    git_ok(&src, &["checkout", "-q", "main"]);

    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount never came up"
    );
    assert_eq!(std::fs::read_to_string(mnt.join("x.txt")).unwrap(), "base\n");
    assert!(!mnt.join("marker.txt").exists());

    // Materialize an edit to x.txt that happens to equal feature's version.
    std::fs::write(mnt.join("x.txt"), b"edited\n").unwrap();

    // Check out feature; wait until the swap is observed (marker appears).
    // reconcile then drops the x.txt edit because it equals feature's x.txt.
    let out = Command::new(PROJGIT_BIN)
        .args(["checkout", "-C"])
        .arg(&mnt)
        .arg("feature")
        .output()
        .expect("spawn projgit checkout feature");
    assert!(out.status.success(), "checkout feature failed");
    assert!(
        wait_until(|| mnt.join("marker.txt").exists(), Duration::from_secs(10)),
        "never re-projected to feature"
    );

    // Check out main again; wait until observed (marker gone).
    let out = Command::new(PROJGIT_BIN)
        .args(["checkout", "-C"])
        .arg(&mnt)
        .arg("main")
        .output()
        .expect("spawn projgit checkout main");
    assert!(out.status.success(), "checkout main failed");
    assert!(
        wait_until(|| !mnt.join("marker.txt").exists(), Duration::from_secs(10)),
        "never re-projected back to main"
    );

    // x.txt is main's content: the edit was reconciled away when it became
    // part of the feature baseline, so it does NOT shadow main. (Without
    // reconcile-on-swap it would still read "edited\n" and status dirty.)
    assert!(
        wait_for_content(&mnt.join("x.txt"), "base\n", Duration::from_secs(10)),
        "committed/redundant edit must not shadow the checkout: {:?}",
        std::fs::read_to_string(mnt.join("x.txt"))
    );
    let s = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(s.trim().is_empty(), "status must be clean after checkout:\n{s}");

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_checkout_synchronous_over_control_socket() {
    // `projgit checkout` drives the swap over the mount's control socket
    // and returns only once the mount has applied it — the command's own
    // output reports "mount re-projected" (the synchronous ack), rather
    // than "will re-project" (the async poll-watcher fallback).
    let base = std::env::temp_dir().join(format!("projgit-cli-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::write(src.join("x.txt"), b"main\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "main"]);
    git_ok(&src, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("x.txt"), b"feature\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "feature"]);
    git_ok(&src, &["checkout", "-q", "main"]);

    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount never came up"
    );
    assert_eq!(std::fs::read_to_string(mnt.join("x.txt")).unwrap(), "main\n");

    // `projgit checkout feature` — the mount must acknowledge synchronously.
    let out = Command::new(PROJGIT_BIN)
        .args(["checkout", "-C"])
        .arg(&mnt)
        .arg("feature")
        .output()
        .expect("spawn projgit checkout");
    assert!(out.status.success(), "checkout failed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("mount re-projected"),
        "checkout must be acked synchronously over the control socket, got:\n{err}"
    );

    // And the worktree reflects feature.
    assert!(
        wait_for_content(&mnt.join("x.txt"), "feature\n", Duration::from_secs(5)),
        "worktree must reflect feature after a synchronous checkout"
    );

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_stock_commit_reconciles_via_hook() {
    // STOCK git operations we don't control are observed via the scratch
    // git dir's `reference-transaction` hook (not the poll watcher): a
    // stock `git commit` fires the hook, which swaps the mount to the new
    // commit and reconciles the now-committed edit out of the upper — so
    // it no longer shadows a later checkout of the parent as a phantom.
    let base = std::env::temp_dir().join(format!("projgit-cli-hook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::write(src.join("x.txt"), b"base\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "c1"]);
    let c1 = git_ok(&src, &["rev-parse", "HEAD"]).trim().to_string();

    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount never came up"
    );
    assert_eq!(std::fs::read_to_string(mnt.join("x.txt")).unwrap(), "base\n");

    // Materialize an edit, then commit it with STOCK git (fires the hook).
    std::fs::write(mnt.join("x.txt"), b"edited\n").unwrap();
    git_ok(&mnt, &["commit", "-q", "-am", "c2"]);

    // The hook must have swapped + reconciled the edit out of the upper.
    // Checking out c1 (parent) must therefore show base — if the stock
    // commit had NOT been observed, the "edited" upper entry would shadow
    // c1 and this would read "edited".
    let out = Command::new(PROJGIT_BIN)
        .args(["checkout", "-C"])
        .arg(&mnt)
        .arg(&c1)
        .output()
        .expect("spawn projgit checkout <c1>");
    assert!(
        out.status.success(),
        "checkout c1 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_for_content(&mnt.join("x.txt"), "base\n", Duration::from_secs(10)),
        "stock commit must be observed via the hook so its edit doesn't shadow the parent: {:?}",
        std::fs::read_to_string(mnt.join("x.txt"))
    );
    let s = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(s.trim().is_empty(), "status must be clean:\n{s}");

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_fsmonitor_over_socket() {
    // `--fsmonitor` installs a core.fsmonitor hook that streams the
    // mount's modified-path set over the control socket. Verify the
    // config is wired, the query returns the edited path, and status is
    // still correct.
    let base = std::env::temp_dir().join(format!("projgit-cli-fsm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/f.txt"), b"hello\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "c1"]);

    // Mount with --fsmonitor.
    let mut child = Command::new(PROJGIT_BIN)
        .args(["mount", "--writable", "--fsmonitor", "--cache-dir"])
        .arg(&cache)
        .arg(&src)
        .arg(&mnt)
        .spawn()
        .expect("spawn projgit mount --writable --fsmonitor");
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(10)),
        "mount never came up"
    );

    // core.fsmonitor is wired to the mount's hook.
    let cfg = git_ok(&mnt, &["config", "core.fsmonitor"]);
    assert!(
        cfg.contains("projgit-fsmonitor"),
        "core.fsmonitor must point at the mount hook, got: {cfg:?}"
    );

    // Clean mount: no false positives.
    let s0 = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(s0.trim().is_empty(), "fresh mount must be clean:\n{s0}");

    // Edit a file, then query fsmonitor directly (what git's hook runs).
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/f.txt"))
            .unwrap();
        f.write_all(b"EDIT\n").unwrap();
    }
    let out = Command::new(PROJGIT_BIN)
        .args(["__fsmonitor", "2", "0"])
        .current_dir(&mnt)
        .output()
        .expect("spawn projgit __fsmonitor");
    assert!(out.status.success(), "fsmonitor query must succeed");
    let resp = String::from_utf8_lossy(&out.stdout);
    assert!(
        resp.contains("dir/f.txt"),
        "fsmonitor response must list the edited path, got: {resp:?}"
    );

    // And git status still surfaces the edit with fsmonitor enabled.
    let s1 = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(
        s1.contains("M dir/f.txt"),
        "status must surface the edit under fsmonitor:\n{s1}"
    );

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_partial_clone_edit_commit() {
    // A writable mount over a PARTIAL (blob:none) clone: projgit
    // partial-clones the `file://` source, so blobs are absent locally.
    // Exercises the two partial-clone fixes the live GitHub validation
    // surfaced — the writable index synthesis (size falls back to 0 when
    // a blob header is absent) and the promisor config propagation (so
    // git's write-tree/commit treat absent blobs as fetchable, not
    // fatal). Without either fix the mount crashes or the commit fails.
    let base = std::env::temp_dir().join(format!("projgit-cli-partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/a.txt"), b"alpha\n").unwrap();
    std::fs::write(src.join("dir/b.txt"), b"beta\n").unwrap();
    std::fs::write(src.join("README.md"), b"readme\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);

    // Mount writable over a `file://` URL — that path goes through
    // projgit's `--filter=blob:none` partial clone (a plain path would
    // hardlink and skip the filter).
    let src_abs = std::fs::canonicalize(&src).unwrap();
    let url = format!("file://{}", src_abs.display());
    let mut child = Command::new(PROJGIT_BIN)
        .args(["mount", "--writable", "--cache-dir"])
        .arg(&cache)
        .arg(&url)
        .arg(&mnt)
        .spawn()
        .expect("spawn projgit mount --writable file://");
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(15)),
        "writable partial-clone mount never came up"
    );

    // The mount inherited the source's promisor config (the fix): git in
    // the mount treats absent blobs as fetchable, not fatal.
    let promisor = git_ok(&mnt, &["config", "--get", "remote.origin.promisor"]);
    assert_eq!(
        promisor.trim(),
        "true",
        "writable mount must inherit the partial-clone promisor config"
    );

    // Edit a tracked file + create a new one, then commit with stock git.
    // (Blobs hydrate from the local file:// promisor — fast.)
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(mnt.join("dir/a.txt"))
            .unwrap();
        f.write_all(b"EDIT\n").unwrap();
    }
    std::fs::write(mnt.join("dir/new.txt"), b"fresh\n").unwrap();
    git_ok(&mnt, &["add", "-A"]);
    let (ok, _) = git(&mnt, &["commit", "-m", "edit over a partial clone"]);
    assert!(ok, "commit over a partial clone must succeed");
    let committed = git_ok(&mnt, &["cat-file", "-p", "HEAD:dir/a.txt"]);
    assert_eq!(committed, "alpha\nEDIT\n", "committed edit must be readable");
    let newc = git_ok(&mnt, &["cat-file", "-p", "HEAD:dir/new.txt"]);
    assert_eq!(newc, "fresh\n", "new file must be committed");

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_fsmonitor_avoids_partial_clone_hydration() {
    // Over a real partial (blob:none) clone, `--fsmonitor` seeds a
    // pre-populated FSMN index extension so git's first `status` trusts
    // every unmodified entry and does NOT content-check them — no mass
    // blob hydration. (Without the seed, the same first status hydrates
    // the whole tree; confirmed by the plain partial-clone path.)
    let base = std::env::temp_dir().join(format!("projgit-cli-fsmnhydr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    // Let the local `file://` upload-pack honor `--filter` so projgit's
    // clone is a REAL partial clone (blobs absent).
    git_ok(&src, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    for i in 0..8 {
        std::fs::write(src.join(format!("dir/f{i}.txt")), format!("content number {i}\n")).unwrap();
    }
    std::fs::write(src.join("README.md"), b"readme\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);

    let src_abs = std::fs::canonicalize(&src).unwrap();
    let url = format!("file://{}", src_abs.display());
    let mut child = Command::new(PROJGIT_BIN)
        .args(["mount", "--writable", "--fsmonitor", "--cache-dir"])
        .arg(&cache)
        .arg(&url)
        .arg(&mnt)
        .spawn()
        .expect("spawn projgit mount --writable --fsmonitor file://");
    assert!(
        wait_for_mount(&mnt, Duration::from_secs(15)),
        "mount never came up"
    );

    // Sanity: this is a real partial clone (blobs absent).
    let clone = find_clone_dir(&cache);
    let before = count_missing_objects(&clone);
    assert!(
        before > 0,
        "test must exercise a real partial clone (missing blobs); got {before} \
         (is uploadpack.allowFilter honored?)"
    );

    // First status is clean AND does not hydrate any blob.
    let s = git_ok(&mnt, &["status", "--porcelain"]);
    assert!(s.trim().is_empty(), "fresh mount must be clean:\n{s}");
    let after = count_missing_objects(&clone);
    assert_eq!(
        after, before,
        "the FSMN seed must let git trust unmodified entries — no hydration on \
         first status (before={before}, after={after})"
    );

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn cli_writable_journal_gcs_stale_blobs() {
    // The upper journal's content-addressed blob store accumulates a blob
    // per distinct file version during editing; compaction (on remount or
    // checkout) garbage-collects the ones no longer referenced, so the
    // store doesn't grow unbounded.
    let base = std::env::temp_dir().join(format!("projgit-cli-gc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("src");
    let mnt = base.join("mnt");
    let cache = base.join("cache");
    std::fs::create_dir_all(&mnt).unwrap();
    let _guard = DirGuard(base.clone());

    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-q", "-b", "main"]);
    git_ok(&src, &["config", "user.email", "t@t.invalid"]);
    git_ok(&src, &["config", "user.name", "t"]);
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/f.txt"), b"original\n").unwrap();
    git_ok(&src, &["add", "-A"]);
    git_ok(&src, &["commit", "-q", "-m", "init"]);

    // --- Mount 1: rewrite the same file with distinct contents. ---
    let mut child = spawn_writable(&src, &mnt, &cache);
    assert!(wait_for_mount(&mnt, Duration::from_secs(10)), "mount 1 up");
    std::fs::write(mnt.join("dir/f.txt"), b"content-one\n").unwrap();
    std::fs::write(mnt.join("dir/f.txt"), b"content-two\n").unwrap();
    std::fs::write(mnt.join("dir/f.txt"), b"content-three\n").unwrap();

    // The blob store accumulated multiple versions (no compaction mid-session).
    let blobs = find_upper_blobs(&cache).expect("upper blobs dir");
    let during = count_files(&blobs);
    assert!(
        during >= 2,
        "blob store should accumulate file versions during editing, got {during}"
    );

    // Unmount + remount → replay + reconcile + compaction, which GCs stale blobs.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child.kill();
    let _ = child.wait();
    assert!(wait_for_unmount(&mnt, Duration::from_secs(10)), "unmounted");

    let mut child2 = spawn_writable(&src, &mnt, &cache);
    assert!(wait_for_mount(&mnt, Duration::from_secs(10)), "mount 2 up");

    // The latest edit survived...
    assert_eq!(
        std::fs::read_to_string(mnt.join("dir/f.txt")).unwrap(),
        "content-three\n"
    );
    // ...and only the one live blob remains (the stale versions are gone).
    let after = count_files(&blobs);
    assert_eq!(
        after, 1,
        "compaction must GC stale blobs, keeping only the live one (was {during}, now {after})"
    );

    // Tear down.
    let _ = Command::new("fusermount").args(["-u"]).arg(&mnt).status();
    let _ = child2.kill();
    let _ = child2.wait();
}
