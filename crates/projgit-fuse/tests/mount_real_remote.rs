//! Network-gated end-to-end test: partial-clone a real public repo,
//! mount it through `projgit-fuse`, and walk it via the kernel.
//!
//! This is the test that proves projgit's public README claim ("you
//! can mount a public repo and walk it") is actually true today,
//! against a live remote and the same `GitCliFetcher` path the CLI
//! uses for URL mounts.
//!
//! Why it's gated:
//!
//! - It needs `/dev/fuse` (same constraint as `mount_smoke`).
//! - It hits `github.com` over HTTPS, which is unacceptable for the
//!   default test run / cold checkouts / offline CI.
//!
//! Run inside the devcontainer with both gates set:
//!
//! ```sh
//! PROJGIT_NETWORK_TESTS=1 \
//!   cargo test -p projgit-fuse --test mount_real_remote \
//!   -- --ignored --nocapture
//! ```
//!
//! Cfg-gated to Linux + macOS, like `mount_smoke`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_core::{
    clone::{git_dir_for, partial_clone, CloneOptions},
    GitCliFetcher, HydratingObjectStore, ObjectStore, Projection, ProjectionFsProvider,
    RootOverlay,
};
use projgit_fuse::{mount_background, MountConfig};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// Skip helpers
// -----------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn network_enabled() -> bool {
    std::env::var("PROJGIT_NETWORK_TESTS").as_deref() == Ok("1")
}

// -----------------------------------------------------------------------------
// Filesystem helpers (mirror `mount_smoke.rs` so each test binary stays
// self-contained; a shared common module would add more ceremony than it
// saves for two tests).
// -----------------------------------------------------------------------------

/// Drop guard that removes a directory tree on scope exit.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_temp_dir(label: &str) -> (PathBuf, DirGuard) {
    let p = std::env::temp_dir().join(format!(
        "projgit-fuse-{}-{}-{}",
        label,
        std::process::id(),
        // Cheap per-call uniqueness so cache + mountpoint never collide.
        Instant::now().elapsed().as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    let guard = DirGuard(p.clone());
    (p, guard)
}

/// Wait until `mountpoint` reports a different st_dev from its parent
/// (i.e. the FUSE mount actually came up), or `timeout` elapses.
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

const TARGET_URL: &str = "https://github.com/rust-lang/log";
const TARGET_REF: &str = "master";

#[test]
#[ignore = "requires FUSE and network; opt in with PROJGIT_NETWORK_TESTS=1"]
fn mount_real_remote_serves_public_repo() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    if !network_enabled() {
        eprintln!("SKIP: set PROJGIT_NETWORK_TESTS=1 to enable network tests");
        return;
    }

    // 1. Fresh cache dir for the partial clone.
    let (cache_dir, _cache_guard) = make_temp_dir("real-cache");

    // 2. Partial-clone the target repo into it. This is what
    //    `projgit mount <url>` does for a URL source.
    let opts = CloneOptions::new(TARGET_URL.to_owned(), cache_dir.clone());
    partial_clone(&opts).expect("partial_clone of TARGET_URL");

    // 3. Build the same provider stack the CLI builds for URL mounts:
    //    ObjectStore + GitCliFetcher + HydratingObjectStore + projection.
    let store = Arc::new(
        ObjectStore::open(git_dir_for(&cache_dir)).expect("ObjectStore::open of partial clone"),
    );
    let fetcher = GitCliFetcher::open(store.clone()).expect("GitCliFetcher::open");
    let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(
            Projection::Ref(TARGET_REF.to_owned()),
            hydrating,
            RootOverlay::new(),
            /* projection_id */ 1,
        )
        .expect("ProjectionFsProvider::new"),
    );

    // 4. Mount in the background. Drop order at function exit:
    //    `_session` (unmounts), then `_mountpoint_guard` (rmdir),
    //    then `_cache_guard` (rmdir cache).
    let (mountpoint, _mountpoint_guard) = make_temp_dir("real-mp");
    let _session =
        mount_background(provider, &mountpoint, &MountConfig::default()).expect("mount_background");

    assert!(
        wait_for_mount(&mountpoint, Duration::from_secs(10)),
        "mountpoint never became a FUSE mount within 10s"
    );

    // 5. Assertions. Pick stable, top-of-repo files known to exist
    //    in `rust-lang/log` for years so this test does not break on
    //    routine upstream churn.

    // Root readdir contains the well-known top-level entries.
    let root_names: std::collections::BTreeSet<String> = std::fs::read_dir(&mountpoint)
        .expect("read_dir mountpoint")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for expected in ["Cargo.toml", "LICENSE-APACHE", "src"] {
        assert!(
            root_names.contains(expected),
            "root listing missing {expected}: got {root_names:?}",
        );
    }

    // Cargo.toml is a real Cargo manifest for the `log` crate.
    let cargo_toml =
        std::fs::read_to_string(mountpoint.join("Cargo.toml")).expect("read Cargo.toml");
    assert!(
        cargo_toml.contains("name = \"log\""),
        "Cargo.toml does not contain `name = \"log\"`:\n{cargo_toml}",
    );

    // src/ has at least a few files; lib.rs in particular has been
    // present for the entire history of the crate.
    let src_dir = mountpoint.join("src");
    let src_names: std::collections::BTreeSet<String> = std::fs::read_dir(&src_dir)
        .expect("read_dir src")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        src_names.contains("lib.rs"),
        "src/ missing lib.rs: got {src_names:?}",
    );
    assert!(
        src_names.len() >= 3,
        "src/ has fewer entries than expected: {src_names:?}",
    );

    // src/lib.rs reads back as non-empty Rust source.
    let lib_rs = std::fs::read_to_string(src_dir.join("lib.rs")).expect("read src/lib.rs");
    assert!(
        !lib_rs.is_empty(),
        "src/lib.rs read back empty; lazy hydration failed?",
    );

    // Done. Drop guards in the documented order.
    drop(_session);
}
