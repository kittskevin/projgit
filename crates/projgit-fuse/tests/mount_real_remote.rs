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
    dotgit, GitCliFetcher, HydratingObjectStore, ObjectStore, Projection, ProjectionFsProvider,
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

/// Companion test: same partial-clone path, but with an A1 `.git/`
/// overlay spliced at the projection root. Proves that
/// `dotgit::a1_overlay` actually lets `git` operate inside the mount.
///
/// This is what closes problem-statement §7 criterion #4 ("git log
/// <path> works inside the mount") from "Deferred" to "Met".
#[test]
#[ignore = "requires FUSE and network; opt in with PROJGIT_NETWORK_TESTS=1"]
fn mount_real_remote_with_dotgit_supports_git_log() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    if !network_enabled() {
        eprintln!("SKIP: set PROJGIT_NETWORK_TESTS=1 to enable network tests");
        return;
    }

    let (cache_dir, _cache_guard) = make_temp_dir("dotgit-cache");
    let opts = CloneOptions::new(TARGET_URL.to_owned(), cache_dir.clone());
    partial_clone(&opts).expect("partial_clone of TARGET_URL");

    let git_dir = git_dir_for(&cache_dir);
    let store = Arc::new(ObjectStore::open(&git_dir).expect("ObjectStore::open"));

    // Resolve the projection to a commit OID for both `HEAD`
    // synthesis and the assertion below.
    let projection = Projection::Ref(TARGET_REF.to_owned());
    let commit_oid = projection.resolve_commit(&store).expect("resolve_commit");

    // Build the A1 overlay pointing at the shared objects directory.
    let objects_dir = std::fs::canonicalize(git_dir.join("objects")).expect("canonicalize objects");
    let overlay = dotgit::a1_overlay(commit_oid, &objects_dir);

    let fetcher = GitCliFetcher::open(store.clone()).expect("GitCliFetcher::open");
    let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(projection, hydrating, overlay, /* projection_id */ 2)
            .expect("ProjectionFsProvider::new"),
    );

    let (mountpoint, _mountpoint_guard) = make_temp_dir("dotgit-mp");
    let _session =
        mount_background(provider, &mountpoint, &MountConfig::default()).expect("mount_background");

    assert!(
        wait_for_mount(&mountpoint, Duration::from_secs(10)),
        "mountpoint never became a FUSE mount within 10s"
    );

    // `.git/HEAD` is visible and contains the detached commit OID.
    let head = std::fs::read_to_string(mountpoint.join(".git").join("HEAD"))
        .expect("read synthesized .git/HEAD");
    assert_eq!(
        head.trim(),
        commit_oid.to_string(),
        "synthesized HEAD must equal the projection's commit OID"
    );

    // `.git/objects/info/alternates` is visible and contains the
    // shared objects directory.
    let alt = std::fs::read_to_string(mountpoint.join(".git/objects/info/alternates"))
        .expect("read synthesized alternates");
    assert_eq!(
        alt.trim(),
        objects_dir.to_string_lossy(),
        "alternates must point at the shared objects directory"
    );

    // `git -C <mount> rev-parse HEAD` returns the projection's commit.
    // This is the "no user setup required" path — the uid/gid echo in
    // the FUSE adapter is what stops `safe.directory` from blocking us.
    let rev_parse = Command::new("git")
        .arg("-C")
        .arg(&mountpoint)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("spawn git rev-parse");
    assert!(
        rev_parse.status.success(),
        "git rev-parse HEAD failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&rev_parse.stdout),
        String::from_utf8_lossy(&rev_parse.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&rev_parse.stdout).trim(),
        commit_oid.to_string(),
        "git rev-parse HEAD must return the projection's commit OID",
    );

    // `git -C <mount> log -1 --format=%H` returns the same OID and
    // exercises commit parsing through the alternates objects dir.
    let log_head = Command::new("git")
        .arg("-C")
        .arg(&mountpoint)
        .args(["log", "-1", "--format=%H"])
        .output()
        .expect("spawn git log");
    assert!(
        log_head.status.success(),
        "git log -1 failed: stderr={:?}",
        String::from_utf8_lossy(&log_head.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&log_head.stdout).trim(),
        commit_oid.to_string(),
    );

    // `git -C <mount> log -1 -- src/lib.rs` returns *some* commit
    // touching src/lib.rs. We don't assert which one (history is
    // free to change upstream) — only that the command succeeds and
    // produces a single OID-shaped line.
    let log_path = Command::new("git")
        .arg("-C")
        .arg(&mountpoint)
        .args(["log", "-1", "--format=%H", "--", "src/lib.rs"])
        .output()
        .expect("spawn git log -- src/lib.rs");
    assert!(
        log_path.status.success(),
        "git log -- src/lib.rs failed: stderr={:?}",
        String::from_utf8_lossy(&log_path.stderr),
    );
    let hex = String::from_utf8_lossy(&log_path.stdout).trim().to_owned();
    assert_eq!(hex.len(), 40, "expected a 40-char OID, got {hex:?}");
    assert!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "expected hex OID, got {hex:?}",
    );

    drop(_session);
}

/// A1+ companion: same setup, but the overlay is built with
/// `dotgit::a1_plus_overlay` (A1 + `.git/index` matching HEAD, every
/// entry `ASSUME_VALID`). Asserts the things the new index unlocks:
/// `git status` reports a clean working tree, `git diff` and
/// `git diff --cached` are both empty, and `git ls-files` returns the
/// real file list. See `docs/design/dotgit-index.md`.
#[test]
#[ignore = "requires FUSE and network; opt in with PROJGIT_NETWORK_TESTS=1"]
fn mount_real_remote_with_dotgit_a1_plus_shows_clean_status() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    if !network_enabled() {
        eprintln!("SKIP: set PROJGIT_NETWORK_TESTS=1 to enable network tests");
        return;
    }

    let (cache_dir, _cache_guard) = make_temp_dir("a1plus-cache");
    let opts = CloneOptions::new(TARGET_URL.to_owned(), cache_dir.clone());
    partial_clone(&opts).expect("partial_clone of TARGET_URL");

    let git_dir = git_dir_for(&cache_dir);
    let store = Arc::new(ObjectStore::open(&git_dir).expect("ObjectStore::open"));

    let projection = Projection::Ref(TARGET_REF.to_owned());
    let commit_oid = projection.resolve_commit(&store).expect("resolve_commit");
    let objects_dir = std::fs::canonicalize(git_dir.join("objects")).expect("canonicalize objects");

    let overlay = dotgit::a1_plus_overlay(&store, commit_oid, &objects_dir)
        .expect("a1_plus_overlay builds successfully");

    let fetcher = GitCliFetcher::open(store.clone()).expect("GitCliFetcher::open");
    let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(projection, hydrating, overlay, /* projection_id */ 3)
            .expect("ProjectionFsProvider::new"),
    );

    let (mountpoint, _mountpoint_guard) = make_temp_dir("a1plus-mp");
    let _session =
        mount_background(provider, &mountpoint, &MountConfig::default()).expect("mount_background");

    assert!(
        wait_for_mount(&mountpoint, Duration::from_secs(10)),
        "mountpoint never became a FUSE mount within 10s"
    );

    // `git status --porcelain` should produce zero lines.
    let status = Command::new("git")
        .arg("-C")
        .arg(&mountpoint)
        .args(["status", "--porcelain"])
        .output()
        .expect("spawn git status --porcelain");
    assert!(
        status.status.success(),
        "git status --porcelain failed: stderr={:?}",
        String::from_utf8_lossy(&status.stderr),
    );
    let porcelain = String::from_utf8_lossy(&status.stdout);
    assert!(
        porcelain.is_empty(),
        "git status --porcelain must be empty (working tree clean); got:\n{porcelain}",
    );

    // Full `git status` output should declare the working tree clean.
    let status_full = Command::new("git")
        .arg("-C")
        .arg(&mountpoint)
        .arg("status")
        .output()
        .expect("spawn git status");
    let out = String::from_utf8_lossy(&status_full.stdout);
    assert!(
        out.contains("nothing to commit, working tree clean"),
        "expected 'nothing to commit, working tree clean' in git status; got:\n{out}",
    );

    // `git diff` and `git diff --cached` both empty.
    for args in [&["diff"][..], &["diff", "--cached"][..]] {
        let diff = Command::new("git")
            .arg("-C")
            .arg(&mountpoint)
            .args(args)
            .output()
            .expect("spawn git diff");
        assert!(
            diff.status.success(),
            "git {args:?} failed: stderr={:?}",
            String::from_utf8_lossy(&diff.stderr),
        );
        assert!(
            diff.stdout.is_empty(),
            "git {args:?} must be empty; got {} bytes",
            diff.stdout.len(),
        );
    }

    // `git ls-files` must return the real file list.
    let ls_files = Command::new("git")
        .arg("-C")
        .arg(&mountpoint)
        .arg("ls-files")
        .output()
        .expect("spawn git ls-files");
    assert!(ls_files.status.success());
    let listing = String::from_utf8_lossy(&ls_files.stdout);
    assert!(
        !listing.is_empty(),
        "git ls-files must return entries when index is populated"
    );
    assert!(
        listing.lines().any(|l| l == "Cargo.toml"),
        "expected Cargo.toml in git ls-files output; got:\n{listing}",
    );

    // A1 invariants still hold (A1+ is a strict superset).
    let rev_parse = Command::new("git")
        .arg("-C")
        .arg(&mountpoint)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("spawn git rev-parse");
    assert_eq!(
        String::from_utf8_lossy(&rev_parse.stdout).trim(),
        commit_oid.to_string(),
    );

    drop(_session);
}
