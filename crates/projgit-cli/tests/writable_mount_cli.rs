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
