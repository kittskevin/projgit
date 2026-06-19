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
    let mut child = Command::new(PROJGIT_BIN)
        .args(["mount", "--writable"])
        .arg(&src)
        .arg(&mnt)
        .spawn()
        .expect("spawn projgit mount --writable");
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
