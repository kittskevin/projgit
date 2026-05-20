//! Runtime test for Stage 1 of the projgitd plan
//! (`docs/implementation/projgitd-plan.md`): prove that one projgit
//! process can host multiple `ProjectionFsProvider`s sharing one
//! `Arc<HydratingObjectStore>`, with two real FUSE mounts at distinct
//! mountpoints, and that the shared `ObjectStore`'s in-memory caches
//! are actually shared (read via mount A then mount B → second read
//! is a cache hit).
//!
//! Run inside the devcontainer:
//!
//! ```sh
//! cargo test -p projgit-fuse --test mount_multi -- --ignored --nocapture
//! ```
//!
//! Cfg-gated to Linux + macOS (FUSE).

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
// Fixture / mount helpers (duplicated from mount_smoke.rs / projection_fs.rs
// per the existing test convention — each test binary is its own crate and a
// shared common module would add more ceremony than it saves).
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

/// Build a fixture repo with two refs, `main` and `branchB`, each
/// holding a distinct `README.md` (so we can verify isolation) plus
/// an identical `shared.txt` blob (so we can verify cache sharing —
/// git deduplicates the blob, so reading it via both mounts hits the
/// same OID and exercises the shared in-process blob cache).
fn build_two_ref_fixture() -> PathBuf {
    let base = std::env::temp_dir().join(format!("projgit-fuse-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);

    // main: README says "from main", shared.txt is the common blob.
    std::fs::write(base.join("README.md"), b"from main\n").unwrap();
    std::fs::write(base.join("shared.txt"), b"identical bytes\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "main initial"]);

    // branchB: README says "from branch B", shared.txt unchanged
    // (so git stores one blob, accessible from both refs by the same
    // OID).
    git(&base, &["checkout", "-q", "-b", "branchB"]);
    std::fs::write(base.join("README.md"), b"from branch B\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "branchB diverges"]);

    // Leave HEAD on main so a default checkout would see main; we
    // don't actually check anything out, but a consistent state
    // helps post-mortem.
    git(&base, &["checkout", "-q", "main"]);

    base
}

struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_mountpoint(name: &str) -> (PathBuf, DirGuard) {
    let mp = std::env::temp_dir().join(format!(
        "projgit-fuse-mp-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&mp);
    std::fs::create_dir_all(&mp).unwrap();
    let guard = DirGuard(mp.clone());
    (mp, guard)
}

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
// The test
// -----------------------------------------------------------------------------

#[test]
#[ignore = "requires FUSE; run inside the devcontainer"]
fn multi_projection_shares_object_store_and_isolates_contents() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let repo = build_two_ref_fixture();

    // One ObjectStore + one HydratingObjectStore, shared by both
    // providers. This is the Stage 1 contract: same store, distinct
    // projections, in-memory caches that one provider populates are
    // visible to the other.
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), NoopFetcher));

    let provider_a = Arc::new(
        ProjectionFsProvider::new(
            Projection::Ref("main".to_owned()),
            hydrating.clone(),
            RootOverlay::new(), // skip dotgit for the test — we
            // only care about file content
            /* projection_id */ 1,
        )
        .expect("ProjectionFsProvider main"),
    );
    let provider_b = Arc::new(
        ProjectionFsProvider::new(
            Projection::Ref("branchB".to_owned()),
            hydrating.clone(),
            RootOverlay::new(),
            /* projection_id */ 2,
        )
        .expect("ProjectionFsProvider branchB"),
    );

    let (mp_a, _g_a) = make_mountpoint("multi-a");
    let (mp_b, _g_b) = make_mountpoint("multi-b");

    let session_a = mount_background(provider_a, &mp_a, &MountConfig::default())
        .expect("mount_background main");
    let session_b = mount_background(provider_b, &mp_b, &MountConfig::default())
        .expect("mount_background branchB");

    assert!(
        wait_for_mount(&mp_a, Duration::from_secs(5)),
        "mount A never came up"
    );
    assert!(
        wait_for_mount(&mp_b, Duration::from_secs(5)),
        "mount B never came up"
    );

    // ---- isolation: each mount sees its own README ----

    let readme_a = std::fs::read_to_string(mp_a.join("README.md")).expect("read mount A README");
    let readme_b = std::fs::read_to_string(mp_b.join("README.md")).expect("read mount B README");
    assert_eq!(readme_a, "from main\n", "mount A should see main's README");
    assert_eq!(
        readme_b, "from branch B\n",
        "mount B should see branchB's README"
    );

    // ---- shared cache: read shared.txt via A then B; assert B is a hit ----

    // Snapshot blob_cache stats before exercising shared.txt.
    let pre = store.blob_cache_stats();

    let shared_a = std::fs::read_to_string(mp_a.join("shared.txt")).expect("read mount A shared");
    assert_eq!(shared_a, "identical bytes\n");

    let after_a = store.blob_cache_stats();
    // Reading via A must have triggered at least one new lookup
    // (miss + insert, or hit if a prior read warmed it). Either way
    // the cache state advanced.
    assert!(
        after_a.inserts > pre.inserts || after_a.hits > pre.hits,
        "blob_cache should record A's read of shared.txt (pre={pre:?} after_a={after_a:?})",
    );

    let shared_b = std::fs::read_to_string(mp_b.join("shared.txt")).expect("read mount B shared");
    assert_eq!(shared_b, "identical bytes\n");

    let after_b = store.blob_cache_stats();
    // shared.txt is byte-identical on both branches, so git stores
    // one blob with one OID. After A read it, B's read of the same
    // OID must hit the shared blob_cache (proves the cache is
    // shared, not per-provider).
    assert!(
        after_b.hits > after_a.hits,
        "blob_cache hits must increase between A's read and B's read of the same OID; \
         after_a={after_a:?} after_b={after_b:?}",
    );

    // Drop both sessions; fuser unmounts each on Drop. Drop order
    // doesn't matter — they're independent kernel mounts.
    drop(session_b);
    drop(session_a);
}
