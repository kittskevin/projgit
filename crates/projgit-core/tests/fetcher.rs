//! Phase 2 integration tests.
//!
//! Most of these are local (no network): they exercise the
//! [`HydratingObjectStore`] composition with a [`NoopFetcher`] or a
//! deterministic fake fetcher.
//!
//! The two tests that *do* need network are gated behind the
//! `PROJGIT_NETWORK_TESTS=1` environment variable so CI / cold checkout
//! never hits the network unexpectedly. When enabled, they reproduce
//! Phase 0a's spike via the real `GixFetcher` against
//! `https://github.com/rust-lang/log`.

use bstr::ByteSlice;
use projgit_core::{
    Fetcher, FetcherError, GixFetcher, HydrateError, HydratingObjectStore, NoopFetcher, ObjectStore,
};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

// -----------------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "projgit-test")
        .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
        .env("GIT_COMMITTER_NAME", "projgit-test")
        .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a tiny single-commit repo and return `(repo_dir, head_oid)`.
fn build_local_repo(name: &str) -> (PathBuf, gix::ObjectId) {
    let base = std::env::temp_dir().join(format!("projgit-p2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    run_git(&base, &["init", "-q", "-b", "main"]);
    std::fs::write(base.join("hello.txt"), b"hello phase 2\n").unwrap();
    run_git(&base, &["add", "."]);
    run_git(&base, &["commit", "-q", "-m", "init"]);
    let head_hex = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&base)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let oid = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).unwrap();
    (base, oid)
}

// -----------------------------------------------------------------------------
// HydratingObjectStore + NoopFetcher (no network)
// -----------------------------------------------------------------------------

#[test]
fn hydrating_store_passes_through_present_blobs() {
    if !git_available() {
        eprintln!("SKIP: no git CLI");
        return;
    }
    let (repo_dir, _head) = build_local_repo("present");
    let store = Arc::new(ObjectStore::open(&repo_dir).unwrap());
    let proj_root = store
        .commit_tree(_head)
        .unwrap();
    let entries = store.read_tree(proj_root).unwrap();
    let hello_oid = entries
        .iter()
        .find(|e| e.name == b"hello.txt".as_bstr())
        .unwrap()
        .oid;

    // NoopFetcher must not be called for objects that are already present.
    let h = HydratingObjectStore::new(store.clone(), NoopFetcher::new());
    let bytes = h.read_blob(hello_oid).unwrap();
    assert_eq!(&bytes, b"hello phase 2\n");
}

#[test]
fn hydrating_store_surfaces_noop_fetcher_failure_for_missing() {
    if !git_available() {
        eprintln!("SKIP: no git CLI");
        return;
    }
    let (repo_dir, _) = build_local_repo("missing");
    let store = Arc::new(ObjectStore::open(&repo_dir).unwrap());
    let h = HydratingObjectStore::new(store.clone(), NoopFetcher::new());

    let bogus = gix::ObjectId::from_hex(b"0000000000000000000000000000000000000001").unwrap();
    let err = h.read_blob(bogus).unwrap_err();
    match err {
        HydrateError::Fetcher(FetcherError::NotHydratable(o)) => assert_eq!(o, bogus),
        other => panic!("expected NotHydratable, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------
// Single-flight via Coalescer + a fake counting Fetcher
// -----------------------------------------------------------------------------

struct CountingFetcher {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl Fetcher for CountingFetcher {
    fn fetch_object(&self, oid: gix::ObjectId) -> Result<(), FetcherError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Pretend we couldn't hydrate so the test never expects post-fetch
        // success without writing to the real store.
        Err(FetcherError::NotHydratable(oid))
    }
}

#[test]
fn hydrating_store_calls_fetcher_for_each_miss() {
    if !git_available() {
        eprintln!("SKIP: no git CLI");
        return;
    }
    let (repo_dir, _) = build_local_repo("fetch_once");
    let store = Arc::new(ObjectStore::open(&repo_dir).unwrap());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = HydratingObjectStore::new(
        store.clone(),
        CountingFetcher {
            calls: calls.clone(),
        },
    );

    let bogus = gix::ObjectId::from_hex(b"0000000000000000000000000000000000000002").unwrap();
    let _ = h.read_blob(bogus); // 1
    let _ = h.read_blob(bogus); // 2 (no memoization of failed hydrations)

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "two misses should each call the fetcher"
    );
}

// -----------------------------------------------------------------------------
// Network: real GixFetcher against a public partial clone
// -----------------------------------------------------------------------------

fn network_enabled() -> bool {
    std::env::var("PROJGIT_NETWORK_TESTS").as_deref() == Ok("1")
}

#[test]
fn gix_fetcher_hydrates_missing_blob_from_remote() {
    if !git_available() {
        eprintln!("SKIP: no git CLI");
        return;
    }
    if !network_enabled() {
        eprintln!("SKIP: set PROJGIT_NETWORK_TESTS=1 to enable network tests");
        return;
    }

    // Create a fresh blobless clone of a small public repo.
    let dest = std::env::temp_dir().join(format!("projgit-p2-net-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let opts = projgit_core::clone::CloneOptions::new(
        "https://github.com/rust-lang/log".to_owned(),
        dest.clone(),
    );
    projgit_core::clone::partial_clone(&opts).expect("partial_clone");

    let git_dir = projgit_core::clone::git_dir_for(&dest);
    let store = Arc::new(ObjectStore::open(&git_dir).unwrap());

    // Pick a blob OID from HEAD's top-level tree -- the same approach the
    // Phase 0a spike used.
    let head = store.resolve_ref("HEAD").unwrap();
    let tree = store.commit_tree(head).unwrap();
    let entries = store.read_tree(tree).unwrap();
    let blob_oid = entries
        .iter()
        .find(|e| e.mode_raw == 0o100644)
        .expect("at least one blob in root tree")
        .oid;

    // Confirm the blob is NOT present locally before we fetch.
    assert!(
        !store.contains(blob_oid),
        "expected blob {blob_oid} to be absent in a blobless clone"
    );

    // Now fetch it via the GixFetcher.
    let fetcher = GixFetcher::open(store.clone(), "origin").expect("open GixFetcher");
    fetcher.fetch_object(blob_oid).expect("fetch_object");

    // After hydration the blob is locally readable.
    assert!(store.contains(blob_oid));
    let bytes = store.read_blob(blob_oid).unwrap();
    assert!(!bytes.is_empty(), "fetched blob should have bytes");

    // HydratingObjectStore composes the same way; reading a *different*
    // missing blob hydrates transparently.
    let h = HydratingObjectStore::new(store.clone(), fetcher);
    // Find a blob deeper in the tree that probably wasn't pulled by the first
    // fetch.
    let deep_blob = entries
        .iter()
        .find(|e| e.mode_raw == 0o100644 && e.oid != blob_oid)
        .map(|e| e.oid);
    if let Some(oid) = deep_blob {
        // First read may or may not need fetching depending on how the
        // pack arrived; either way it should succeed.
        let bytes = h.read_blob(oid).expect("hydrate-on-miss read_blob");
        assert!(!bytes.is_empty());
    }
}
