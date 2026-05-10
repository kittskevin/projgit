//! Runtime smoke test for `projgit-fuse`: actually mount a
//! `ProjectionFsProvider` over a fixture git repo via FUSE and prove
//! that real I/O (`read_dir`, `read_to_string`, `read_link`) reaches
//! our `FsProvider` callbacks.
//!
//! Until this test exists, `projgit-fuse` is only compile-checked.
//! The test is `#[ignore]`-gated because FUSE isn't available
//! everywhere (Windows host, CI runners without `/dev/fuse`, etc.).
//!
//! Run inside the devcontainer:
//!
//! ```sh
//! cargo test -p projgit-fuse --test mount_smoke -- --ignored --nocapture
//! ```
//!
//! Cfg-gated to Linux + macOS (matches the `projgit-fuse` support
//! matrix); compiles to nothing on Windows.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_core::{
    HydratingObjectStore, NoopFetcher, ObjectStore, Projection, ProjectionFsProvider, RootOverlay,
};
use projgit_fuse::{mount_background, MountConfig};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Fixture-repo helpers (mirror tests/integration.rs / tests/projection_fs.rs in
// projgit-core). Duplicated rather than factored out — each test binary is its
// own crate and a shared common module would add more ceremony than it saves.
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

/// Build the standard fixture repo (mirrors
/// `crates/projgit-core/tests/projection_fs.rs::build_fixture`).
fn build_fixture(name: &str) -> (PathBuf, gix::ObjectId) {
    let base = std::env::temp_dir().join(format!("projgit-fuse-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    git(&base, &["config", "core.fileMode", "true"]);

    std::fs::write(base.join("README.md"), b"# fixture repo\n").unwrap();
    std::fs::write(base.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();

    let src_dir = base.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("main.c"), b"int main(void){return 0;}\n").unwrap();

    let util_dir = src_dir.join("util");
    std::fs::create_dir_all(&util_dir).unwrap();
    std::fs::write(util_dir.join("helper.c"), b"void helper(){}\n").unwrap();
    std::fs::write(util_dir.join("helper.h"), b"void helper(void);\n").unwrap();

    git(&base, &["add", "-A"]);
    git(&base, &["update-index", "--chmod=+x", "run.sh"]);

    // Symlink as a 120000 index entry whose blob bytes are the
    // target string. Works the same on Windows and POSIX.
    let target = b"README.md";
    let target_path = base.join(".symlink-target.tmp");
    std::fs::write(&target_path, target).unwrap();
    let blob_hex = String::from_utf8(git(
        &base,
        &["hash-object", "-w", target_path.to_str().unwrap()],
    ))
    .unwrap();
    let blob_hex = blob_hex.trim();
    std::fs::remove_file(&target_path).unwrap();
    git(
        &base,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{blob_hex},link-to-readme"),
        ],
    );

    git(&base, &["commit", "-q", "-m", "initial"]);
    let head_hex = String::from_utf8(git(&base, &["rev-parse", "HEAD"])).unwrap();
    let head = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).expect("valid hex");
    (base, head)
}

// -----------------------------------------------------------------------------
// Mount + cleanup helpers
// -----------------------------------------------------------------------------

/// Drop guard that removes a directory tree on scope exit. Used so a
/// panicking assertion still cleans up the mountpoint (the
/// `BackgroundSession` itself unmounts on Drop, then this clears the
/// now-empty directory).
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Create a fresh empty directory under temp_dir and return a guard
/// that cleans it up.
fn make_mountpoint(name: &str) -> (PathBuf, DirGuard) {
    let mp = std::env::temp_dir().join(format!("projgit-fuse-mp-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&mp);
    std::fs::create_dir_all(&mp).unwrap();
    let guard = DirGuard(mp.clone());
    (mp, guard)
}

/// Wait until `mountpoint` has a different st_dev from its parent
/// (i.e. has been mounted), or `timeout` elapses. Avoids fixed-sleep
/// flakiness on slow / loaded systems.
fn wait_for_mount(mountpoint: &Path, timeout: Duration) -> bool {
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
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

// -----------------------------------------------------------------------------
// The smoke test
// -----------------------------------------------------------------------------

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn fuse_mount_serves_real_projection_data() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let (repo, head) = build_fixture("smoke");
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(
            Projection::Commit(head),
            hydrating,
            RootOverlay::new(),
            /* projection_id */ 1,
        )
        .expect("ProjectionFsProvider::new"),
    );

    let (mountpoint, _mp_guard) = make_mountpoint("smoke");

    // Mount in the background. `_session` is dropped at end of scope,
    // which triggers a clean unmount before the directory guard runs.
    let _session =
        mount_background(provider, &mountpoint, &MountConfig::default()).expect("mount_background");

    // Wait for the kernel to actually attach our FS to the
    // mountpoint. If this times out, dispatch never came up.
    assert!(
        wait_for_mount(&mountpoint, Duration::from_secs(5)),
        "mountpoint never became a FUSE mount within 5s"
    );

    // ---- root readdir ----

    let mut names: Vec<String> = std::fs::read_dir(&mountpoint)
        .expect("read_dir mountpoint")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "README.md".to_owned(),
            "link-to-readme".to_owned(),
            "run.sh".to_owned(),
            "src".to_owned(),
        ]
    );

    // ---- read a regular file ----

    let readme = std::fs::read_to_string(mountpoint.join("README.md")).expect("read README.md");
    assert_eq!(readme, "# fixture repo\n");

    // ---- executable bit on run.sh ----

    let run_meta = std::fs::metadata(mountpoint.join("run.sh")).expect("metadata run.sh");
    use std::os::unix::fs::PermissionsExt;
    let mode = run_meta.permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "run.sh should have executable bits set, mode = {mode:o}"
    );

    // ---- symlink ----

    let link_meta = std::fs::symlink_metadata(mountpoint.join("link-to-readme"))
        .expect("symlink_metadata link-to-readme");
    assert!(
        link_meta.file_type().is_symlink(),
        "link-to-readme is not a symlink"
    );
    let target =
        std::fs::read_link(mountpoint.join("link-to-readme")).expect("read_link link-to-readme");
    assert_eq!(target, std::path::PathBuf::from("README.md"));

    // ---- nested directory listing ----

    let mut util_names: Vec<String> = std::fs::read_dir(mountpoint.join("src/util"))
        .expect("read_dir src/util")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    util_names.sort();
    assert_eq!(
        util_names,
        vec!["helper.c".to_owned(), "helper.h".to_owned()]
    );

    // ---- read deeper into a subtree ----

    let main_c = std::fs::read_to_string(mountpoint.join("src/main.c")).expect("read src/main.c");
    assert_eq!(main_c, "int main(void){return 0;}\n");

    // Drop order: `_session` first (BackgroundSession unmounts),
    // then `_mp_guard` (removes the now-empty mountpoint dir).
    // Repo dir leaks on purpose — same as the projgit-core fixture
    // tests, and it makes failure post-mortem easier.
    drop(_session);
}
