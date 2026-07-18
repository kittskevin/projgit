//! Integration tests for `crate::dotgit::a1_plus_overlay`.
//!
//! These build a small real git repository on disk, point a
//! [`projgit_core::ObjectStore`] at it, run `a1_plus_overlay`, parse the
//! resulting `.git/index` bytes via [`gix::index::File::at`], and check
//! invariants the A1+ design doc commits to (entry presence, `ASSUME_VALID`
//! on every entry, byte-determinism across builds).
//!
//! These tests need `git` on PATH to build the fixture; they panic if
//! `git` is missing rather than producing misleading "command not found"
//! errors.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use bstr::{BString, ByteSlice};
use projgit_core::dotgit;
use projgit_core::overlay::SyntheticEntry;
use projgit_core::ObjectStore;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Per-test-binary monotonically-increasing counter used to make
/// every temp path inside this file unique even when `cargo test`
/// runs the tests in parallel.
///
/// The previous scheme used `Instant::now().elapsed().as_nanos()`,
/// which returns ~0 ns because the instant is immediately polled —
/// parallel test threads would hit the same `(pid, nanos)` pair and
/// race-delete each other's fixtures (typically surfaced as "git
/// failed" or `read_overlay_file` assertion failures on ~1/5 runs).
fn next_unique_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// -----------------------------------------------------------------------------
// Fixture helpers (mirror `tests/projection_fs.rs` so each test binary
// stays self-contained; a shared common module would add more ceremony
// than it saves for a single module).
// -----------------------------------------------------------------------------

fn git(repo: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git available on PATH");
    if !out.status.success() {
        panic!(
            "git {args:?} failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.stdout
}

/// Same shape as `tests/projection_fs.rs::build_fixture`: 6 entries total
/// (README.md, run.sh-executable, src/main.c, src/util/helper.c,
/// src/util/helper.h, link-to-readme-symlink) so we exercise every entry
/// kind A1+ needs to encode.
fn build_fixture(name: &str) -> (PathBuf, gix::ObjectId) {
    let base = std::env::temp_dir().join(format!(
        "projgit-dotgit-{}-{}-{}",
        name,
        std::process::id(),
        next_unique_id(),
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    git(&base, &["config", "core.fileMode", "true"]);
    git(&base, &["config", "user.email", "test@example.invalid"]);
    git(&base, &["config", "user.name", "Test"]);

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

    // Add a symlink directly to the index (works the same on every OS).
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

/// Drop guard so test failures don't leave temp dirs behind.
struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Walk a path inside a `RootOverlay` and read the bytes of the file
/// at the end. Panics on any mis-step so the tests stay terse.
fn read_overlay_file(overlay: &projgit_core::RootOverlay, path: &str) -> Vec<u8> {
    let mut parts = path.split('/');
    let first = parts.next().unwrap();
    let mut current = overlay
        .get(first.as_bytes())
        .unwrap_or_else(|| panic!("missing top-level `{first}`"));
    for component in parts {
        current = match current {
            SyntheticEntry::Directory { children } => children
                .get(bstr::BStr::new(component.as_bytes()))
                .unwrap_or_else(|| panic!("missing `{component}` while walking `{path}`")),
            _ => panic!("non-directory ancestor while walking `{path}`"),
        };
    }
    match current {
        SyntheticEntry::File { content, .. } => content.clone(),
        other => panic!("expected file at `{path}`, got {other:?}"),
    }
}

/// Parse `.git/index` bytes by writing them to a temp file and asking
/// `gix::index::File::at` to read it. This is the canonical "git would
/// accept these bytes" check.
fn parse_index(index_bytes: &[u8]) -> gix::index::File {
    let tmp = std::env::temp_dir().join(format!(
        "projgit-dotgit-index-parse-{}-{}",
        std::process::id(),
        next_unique_id(),
    ));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, index_bytes).unwrap();
    let file = gix::index::File::at(&tmp, gix::hash::Kind::Sha1, false, Default::default())
        .expect("synthesized index bytes must round-trip through gix::index::File::at");
    let _ = std::fs::remove_file(&tmp);
    file
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn a1_plus_overlay_adds_dotgit_index_file() {
    let (repo, head) = build_fixture("adds-index");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let objects_dir = repo.join(".git/objects");

    let overlay = dotgit::a1_plus_overlay(&store, head, &objects_dir).unwrap();

    // .git/HEAD and .git/objects/info/alternates from A1 are still present.
    assert_eq!(
        read_overlay_file(&overlay, ".git/HEAD"),
        format!("{head}\n").into_bytes(),
    );
    let alt = read_overlay_file(&overlay, ".git/objects/info/alternates");
    assert_eq!(alt, format!("{}\n", objects_dir.display()).into_bytes());

    // A1+ adds .git/index — the new bit. Bytes round-trip through gix.
    let index_bytes = read_overlay_file(&overlay, ".git/index");
    let file = parse_index(&index_bytes);
    assert!(
        !file.entries().is_empty(),
        ".git/index must contain entries (the fixture has 6 paths)"
    );
}

#[test]
fn synthesized_index_carries_every_fixture_path() {
    let (repo, head) = build_fixture("entry-paths");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let objects_dir = repo.join(".git/objects");

    let overlay = dotgit::a1_plus_overlay(&store, head, &objects_dir).unwrap();
    let file = parse_index(&read_overlay_file(&overlay, ".git/index"));

    let paths: std::collections::BTreeSet<BString> = file
        .entries()
        .iter()
        .map(|e| e.path(&file).to_owned())
        .collect();
    let expected: std::collections::BTreeSet<BString> = [
        BString::from("README.md"),
        BString::from("link-to-readme"),
        BString::from("run.sh"),
        BString::from("src/main.c"),
        BString::from("src/util/helper.c"),
        BString::from("src/util/helper.h"),
    ]
    .into_iter()
    .collect();
    assert_eq!(paths, expected);
}

#[test]
fn every_synthesized_entry_has_assume_valid_set() {
    let (repo, head) = build_fixture("assume-valid");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let objects_dir = repo.join(".git/objects");

    let overlay = dotgit::a1_plus_overlay(&store, head, &objects_dir).unwrap();
    let file = parse_index(&read_overlay_file(&overlay, ".git/index"));

    assert!(!file.entries().is_empty());
    for entry in file.entries() {
        assert!(
            entry.flags.contains(gix::index::entry::Flags::ASSUME_VALID),
            "entry `{}` is missing ASSUME_VALID (`flags = {:?}`); without\n\
             this flag git would re-hash the worktree on every status check",
            entry.path(&file).as_bstr(),
            entry.flags,
        );
    }
}

#[test]
fn executable_and_symlink_modes_are_preserved() {
    let (repo, head) = build_fixture("modes");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let objects_dir = repo.join(".git/objects");

    let overlay = dotgit::a1_plus_overlay(&store, head, &objects_dir).unwrap();
    let file = parse_index(&read_overlay_file(&overlay, ".git/index"));

    let mut saw_executable = false;
    let mut saw_symlink = false;
    let mut saw_regular = false;
    for entry in file.entries() {
        let path = entry.path(&file);
        match entry.mode {
            gix::index::entry::Mode::FILE_EXECUTABLE => {
                assert_eq!(path, "run.sh");
                saw_executable = true;
            }
            gix::index::entry::Mode::SYMLINK => {
                assert_eq!(path, "link-to-readme");
                saw_symlink = true;
            }
            gix::index::entry::Mode::FILE => {
                saw_regular = true;
            }
            other => panic!("unexpected mode {other:?} for entry `{}`", path.as_bstr()),
        }
    }
    assert!(saw_executable, "run.sh must be 100755 in the index");
    assert!(saw_symlink, "link-to-readme must be 120000 in the index");
    assert!(saw_regular, "at least one regular file must be 100644");
}

#[test]
fn build_is_byte_deterministic() {
    // The serialized index format only records (path, mode, OID,
    // zero-stat) per entry — no wall-clock-derived fields — so two
    // builds against the same commit produce byte-identical bytes.
    // A future cross-mount cache can key on `commit_oid` alone.
    let (repo, head) = build_fixture("determinism");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let objects_dir = repo.join(".git/objects");

    let bytes_a = read_overlay_file(
        &dotgit::a1_plus_overlay(&store, head, &objects_dir).unwrap(),
        ".git/index",
    );
    let bytes_b = read_overlay_file(
        &dotgit::a1_plus_overlay(&store, head, &objects_dir).unwrap(),
        ".git/index",
    );
    assert_eq!(
        bytes_a, bytes_b,
        "same commit_oid must produce byte-identical index bytes"
    );
}

// -----------------------------------------------------------------------------
// Writable-mode index (R1) — `dotgit::build_writable_index_bytes`
// -----------------------------------------------------------------------------

/// Commit time of HEAD in unix seconds, via git.
fn commit_time_secs(repo: &Path) -> u64 {
    let out = String::from_utf8(git(repo, &["show", "-s", "--format=%ct", "HEAD"])).unwrap();
    out.trim().parse().unwrap()
}

#[test]
fn writable_index_round_trips_with_all_paths() {
    let (repo, head) = build_fixture("writable-paths");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());

    let bytes = dotgit::build_writable_index_bytes(&store, head).unwrap();
    let file = parse_index(&bytes);

    let paths: std::collections::BTreeSet<BString> = file
        .entries()
        .iter()
        .map(|e| e.path(&file).to_owned())
        .collect();
    let expected: std::collections::BTreeSet<BString> = [
        BString::from("README.md"),
        BString::from("link-to-readme"),
        BString::from("run.sh"),
        BString::from("src/main.c"),
        BString::from("src/util/helper.c"),
        BString::from("src/util/helper.h"),
    ]
    .into_iter()
    .collect();
    assert_eq!(paths, expected);
}

#[test]
fn writable_index_has_no_assume_valid() {
    // The defining difference from the read-only A1+ index: a writable
    // mount must let git notice edits, so ASSUME_VALID must NOT be set.
    let (repo, head) = build_fixture("writable-no-av");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());

    let file = parse_index(&dotgit::build_writable_index_bytes(&store, head).unwrap());
    assert!(!file.entries().is_empty());
    for entry in file.entries() {
        assert!(
            !entry.flags.contains(gix::index::entry::Flags::ASSUME_VALID),
            "writable index entry `{}` must NOT have ASSUME_VALID set",
            entry.path(&file).as_bstr(),
        );
    }
}

#[test]
fn writable_index_carries_real_size_and_mtime() {
    let (repo, head) = build_fixture("writable-stat");
    let _guard = DirGuard(repo.clone());
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let ct = commit_time_secs(&repo) as u32;

    let file = parse_index(&dotgit::build_writable_index_bytes(&store, head).unwrap());

    for entry in file.entries() {
        let path = entry.path(&file);
        // Real size from the blob header (no content read) — every
        // fixture file is non-empty.
        assert!(
            entry.stat.size > 0,
            "entry `{}` must carry a real (non-zero) size, got 0",
            path.as_bstr(),
        );
        // Stable mtime = the projection's commit time, so an unmodified
        // file stat-matches the index under checkStat=minimal.
        assert_eq!(
            entry.stat.mtime.secs, ct,
            "entry `{}` mtime.secs must equal HEAD commit time",
            path.as_bstr(),
        );
        if path == "README.md" {
            assert_eq!(
                entry.stat.size, 15,
                "README.md is `# fixture repo\\n` = 15 bytes",
            );
        }
    }
}

#[test]
fn sparse_index_sets_skip_worktree_outside_cone() {
    // build a repo with two top-level dirs + a root file
    let base = std::env::temp_dir().join(format!(
        "projgit-dotgit-sparse-{}-{}",
        std::process::id(),
        next_unique_id(),
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _guard = DirGuard(base.clone());
    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "user.email", "t@t.invalid"]);
    git(&base, &["config", "user.name", "t"]);
    std::fs::write(base.join("README.md"), b"root\n").unwrap();
    std::fs::create_dir_all(base.join("dirA")).unwrap();
    std::fs::write(base.join("dirA/a.txt"), b"alpha\n").unwrap();
    std::fs::create_dir_all(base.join("dirB")).unwrap();
    std::fs::write(base.join("dirB/b.txt"), b"bravo\n").unwrap();
    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "init"]);
    let head =
        gix::ObjectId::from_hex(String::from_utf8(git(&base, &["rev-parse", "HEAD"])).unwrap().trim().as_bytes())
            .unwrap();

    let store = Arc::new(ObjectStore::open(&base).unwrap());
    let bytes =
        dotgit::build_writable_index_bytes_sparse(&store, head, &["dirA".to_string()]).unwrap();
    let file = parse_index(&bytes);

    let skip = gix::index::entry::Flags::SKIP_WORKTREE;
    for entry in file.entries() {
        let path = entry.path(&file).to_string();
        let has_skip = entry.flags.contains(skip);
        if path.starts_with("dirB/") {
            assert!(has_skip, "out-of-cone `{path}` must have SKIP_WORKTREE");
        } else {
            assert!(
                !has_skip,
                "in-cone `{path}` (root file or cone dir) must NOT have SKIP_WORKTREE",
            );
        }
    }
}

