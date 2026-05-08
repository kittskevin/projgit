//! Integration tests for `projgit-core` against a real fixture repo
//! built at runtime via the system `git` CLI.
//!
//! Why we shell out to `git` instead of constructing objects via gix:
//! it's the closest possible match to how real-world repos are
//! created, and it keeps the test deps to one well-known external
//! tool. If `git` is not on PATH the tests are skipped with a clear
//! message.

use bstr::ByteSlice;
use projgit_core::{
    EntryMode, ObjectKind, ObjectStore, Projection, ResolvedEntry, RootOverlay, SyntheticEntry,
    TreeNavigator,
};
use std::path::{Path, PathBuf};
use std::process::Command;

// -----------------------------------------------------------------------------
// Fixture-repo helpers
// -----------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `git` with `args` inside `cwd`, returning stdout.
/// Panics on non-zero exit.
fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "projgit-test")
        .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
        .env("GIT_COMMITTER_NAME", "projgit-test")
        .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null") // ignore user global config
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

/// Build a small repo under a fresh temp dir, return its path.
///
/// Layout (post-commit):
/// ```text
/// README.md          (regular file, 100644)
/// run.sh             (executable file, 100755 on POSIX; 100644 on Windows)
/// src/
///   main.c           (regular file)
///   util/
///     helper.c       (regular file)
///     helper.h       (regular file)
/// ```
///
/// Returns `(repo_dir, head_commit_oid_hex)`.
fn build_fixture(name: &str) -> (PathBuf, String) {
    let base = std::env::temp_dir().join(format!(
        "projgit-test-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);
    git(&base, &["config", "core.fileMode", "true"]);

    // Files
    std::fs::write(base.join("README.md"), b"# fixture repo\n").unwrap();

    std::fs::write(base.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();

    let src_dir = base.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("main.c"), b"int main(void){return 0;}\n").unwrap();

    let util_dir = src_dir.join("util");
    std::fs::create_dir_all(&util_dir).unwrap();
    std::fs::write(util_dir.join("helper.c"), b"void helper(){}\n").unwrap();
    std::fs::write(util_dir.join("helper.h"), b"void helper(void);\n").unwrap();

    // Stage everything; mark run.sh executable in the index.
    git(&base, &["add", "-A"]);
    git(&base, &["update-index", "--chmod=+x", "run.sh"]);

    git(&base, &["commit", "-q", "-m", "initial"]);

    let head_hex = String::from_utf8(git(&base, &["rev-parse", "HEAD"])).unwrap();
    let head_hex = head_hex.trim().to_owned();
    (base, head_hex)
}

fn parse_oid(hex: &str) -> gix::ObjectId {
    gix::ObjectId::from_hex(hex.as_bytes()).expect("valid hex OID")
}

// -----------------------------------------------------------------------------
// ObjectStore tests
// -----------------------------------------------------------------------------

#[test]
fn object_store_open_resolves_head() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("os_open");
    let store = ObjectStore::open(&repo).unwrap();

    // resolve_ref on HEAD points at the commit.
    let head = store.resolve_ref("HEAD").unwrap();
    assert_eq!(head.to_hex().to_string(), head_hex);

    // Same via the branch name.
    let main_ref = store.resolve_ref("refs/heads/main").unwrap();
    assert_eq!(main_ref, head);

    // The commit is locally present.
    assert!(store.contains(head));
}

#[test]
fn object_store_header_classifies_kinds() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("os_header");
    let store = ObjectStore::open(&repo).unwrap();

    let head = parse_oid(&head_hex);
    let (kind, _) = store.header(head).unwrap();
    assert_eq!(kind, ObjectKind::Commit);

    let tree_oid = store.commit_tree(head).unwrap();
    let (kind, _) = store.header(tree_oid).unwrap();
    assert_eq!(kind, ObjectKind::Tree);
}

#[test]
fn object_store_missing_object_error() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, _) = build_fixture("os_missing");
    let store = ObjectStore::open(&repo).unwrap();

    let bogus = parse_oid("0000000000000000000000000000000000000001");
    assert!(!store.contains(bogus));
    let err = store.header(bogus).unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ObjectStoreError::MissingObject(o) if o == bogus
    ));

    let err = store.read_blob(bogus).unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ObjectStoreError::MissingObject(_)
    ));
}

#[test]
fn object_store_unexpected_kind() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("os_kind");
    let store = ObjectStore::open(&repo).unwrap();

    let commit = parse_oid(&head_hex);
    // Asking for a blob when we have a commit should produce UnexpectedKind.
    let err = store.read_blob(commit).unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ObjectStoreError::UnexpectedKind {
            expected: ObjectKind::Blob,
            actual: ObjectKind::Commit,
            ..
        }
    ));
}

#[test]
fn object_store_read_blob_returns_content() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("os_read_blob");
    let store = ObjectStore::open(&repo).unwrap();

    let head = parse_oid(&head_hex);
    let tree = store.commit_tree(head).unwrap();
    let entries = store.read_tree(tree).unwrap();

    let readme = entries
        .iter()
        .find(|e| e.name == b"README.md".as_bstr())
        .expect("README.md");
    let bytes = store.read_blob(readme.oid).unwrap();
    assert_eq!(&bytes, b"# fixture repo\n");
}

// -----------------------------------------------------------------------------
// TreeNavigator tests
// -----------------------------------------------------------------------------

#[test]
fn tree_navigator_lists_root() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("tn_list");
    let store = ObjectStore::open(&repo).unwrap();
    let head = parse_oid(&head_hex);
    let tree = store.commit_tree(head).unwrap();

    let nav = TreeNavigator::new(&store);
    let names: Vec<String> = nav
        .list(tree)
        .unwrap()
        .iter()
        .map(|e| e.name.to_str_lossy().into_owned())
        .collect();
    // Git sorts trees byte-wise with directories suffixed by '/'.
    assert!(names.contains(&"README.md".to_owned()));
    assert!(names.contains(&"run.sh".to_owned()));
    assert!(names.contains(&"src".to_owned()));
}

#[test]
fn tree_navigator_lookup_nested() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("tn_nested");
    let store = ObjectStore::open(&repo).unwrap();
    let head = parse_oid(&head_hex);
    let tree = store.commit_tree(head).unwrap();
    let nav = TreeNavigator::new(&store);

    // Top-level file.
    let entry = nav.lookup(tree, "README.md").unwrap();
    assert_eq!(entry.mode, EntryMode::RegularFile);

    // Nested directory.
    let entry = nav.lookup(tree, "src/util").unwrap();
    assert_eq!(entry.mode, EntryMode::Directory);

    // Nested file.
    let entry = nav.lookup(tree, "src/util/helper.h").unwrap();
    assert_eq!(entry.mode, EntryMode::RegularFile);

    // Empty path == projection root tree.
    let entry = nav.lookup(tree, "").unwrap();
    assert_eq!(entry.mode, EntryMode::Directory);
    assert_eq!(entry.oid, tree);
}

#[test]
fn tree_navigator_lookup_errors() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("tn_err");
    let store = ObjectStore::open(&repo).unwrap();
    let head = parse_oid(&head_hex);
    let tree = store.commit_tree(head).unwrap();
    let nav = TreeNavigator::new(&store);

    // NotFound at top level.
    let err = nav.lookup(tree, "nope").unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ProjectionError::NotFound { .. }
    ));

    // NotADirectory: try to descend into a file.
    let err = nav.lookup(tree, "README.md/inner").unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ProjectionError::NotADirectory { .. }
    ));

    // InvalidPath: '..' rejected.
    let err = nav.lookup(tree, "../etc").unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ProjectionError::InvalidPath { .. }
    ));
}

#[test]
fn tree_navigator_run_sh_is_executable() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("tn_exec");
    let store = ObjectStore::open(&repo).unwrap();
    let head = parse_oid(&head_hex);
    let tree = store.commit_tree(head).unwrap();
    let nav = TreeNavigator::new(&store);

    // We used `git update-index --chmod=+x` so the index records 100755.
    let entry = nav.lookup(tree, "run.sh").unwrap();
    assert_eq!(entry.mode, EntryMode::ExecutableFile);
    assert_eq!(entry.mode_raw, 0o100755);
}

// -----------------------------------------------------------------------------
// Projection tests (Ref / Commit / Subtree)
// -----------------------------------------------------------------------------

#[test]
fn projection_ref_root_tree() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("proj_ref");
    let store = ObjectStore::open(&repo).unwrap();
    let proj = Projection::Ref("refs/heads/main".to_owned());
    let root_via_ref = proj.root_tree(&store).unwrap();
    let root_via_commit = store.commit_tree(parse_oid(&head_hex)).unwrap();
    assert_eq!(root_via_ref, root_via_commit);
}

#[test]
fn projection_commit_lookup_file() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("proj_commit");
    let store = ObjectStore::open(&repo).unwrap();
    let overlay = RootOverlay::new();
    let proj = Projection::Commit(parse_oid(&head_hex));

    let resolved = proj.lookup(&store, &overlay, "src/main.c").unwrap();
    match resolved {
        ResolvedEntry::Tree(t) => {
            assert_eq!(t.mode, EntryMode::RegularFile);
            let bytes = store.read_blob(t.oid).unwrap();
            assert_eq!(&bytes, b"int main(void){return 0;}\n");
        }
        other => panic!("expected Tree entry, got {other:?}"),
    }
}

#[test]
fn projection_subtree_strips_prefix() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("proj_subtree");
    let store = ObjectStore::open(&repo).unwrap();
    let overlay = RootOverlay::new();
    let proj = Projection::Subtree {
        commit: parse_oid(&head_hex),
        path: "src".to_owned(),
    };

    // Inside a Subtree projection, 'main.c' should resolve directly.
    let resolved = proj.lookup(&store, &overlay, "main.c").unwrap();
    assert!(matches!(resolved, ResolvedEntry::Tree(t) if t.mode == EntryMode::RegularFile));

    // 'util/helper.h' too.
    let resolved = proj.lookup(&store, &overlay, "util/helper.h").unwrap();
    assert!(matches!(resolved, ResolvedEntry::Tree(_)));

    // 'README.md' is OUTSIDE the subtree, so it should be NotFound.
    let err = proj.lookup(&store, &overlay, "README.md").unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ProjectionError::NotFound { .. }
    ));
}

#[test]
fn projection_subtree_must_point_at_directory() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("proj_subtree_file");
    let store = ObjectStore::open(&repo).unwrap();
    let proj = Projection::Subtree {
        commit: parse_oid(&head_hex),
        path: "README.md".to_owned(),
    };
    let err = proj.root_tree(&store).unwrap_err();
    assert!(matches!(
        err,
        projgit_core::ProjectionError::NotADirectory { .. }
    ));
}

// -----------------------------------------------------------------------------
// RootOverlay tests (mechanism only — empty by default)
// -----------------------------------------------------------------------------

#[test]
fn empty_overlay_is_pass_through_for_lookup() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("ov_passthrough");
    let store = ObjectStore::open(&repo).unwrap();
    let overlay = RootOverlay::new();
    let proj = Projection::Commit(parse_oid(&head_hex));

    // Empty overlay => same lookup path as if no overlay existed.
    let resolved = proj.lookup(&store, &overlay, "README.md").unwrap();
    assert!(matches!(resolved, ResolvedEntry::Tree(_)));
}

#[test]
fn empty_overlay_read_root_matches_real_tree() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("ov_root");
    let store = ObjectStore::open(&repo).unwrap();
    let overlay = RootOverlay::new();
    let proj = Projection::Commit(parse_oid(&head_hex));

    let entries = proj.read_root(&store, &overlay).unwrap();
    let names: Vec<String> = entries
        .iter()
        .map(|(n, _)| n.to_str_lossy().into_owned())
        .collect();
    assert!(names.contains(&"README.md".to_owned()));
    assert!(names.contains(&"src".to_owned()));
    // Every entry is a Tree entry; nothing synthetic.
    assert!(entries
        .iter()
        .all(|(_, e)| matches!(e, ResolvedEntry::Tree(_))));
}

#[test]
fn overlay_synthetic_entry_shadows_real() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("ov_shadow");
    let store = ObjectStore::open(&repo).unwrap();
    let proj = Projection::Commit(parse_oid(&head_hex));

    // Overlay puts a synthetic file at "README.md" -- shadowing the real one.
    let mut overlay = RootOverlay::new();
    overlay.insert(
        bstr::BString::from("README.md"),
        SyntheticEntry::file(b"OVERLAYED".to_vec()),
    );

    // Lookup returns the synthetic.
    let resolved = proj.lookup(&store, &overlay, "README.md").unwrap();
    match resolved {
        ResolvedEntry::Synthetic { name, entry } => {
            assert_eq!(name, bstr::BString::from("README.md"));
            assert!(matches!(
                entry,
                SyntheticEntry::File { content, .. } if content == b"OVERLAYED"
            ));
        }
        other => panic!("expected Synthetic entry, got {other:?}"),
    }

    // read_root: the real README.md is hidden; the synthetic one appears.
    let entries = proj.read_root(&store, &overlay).unwrap();
    let readmes: Vec<&ResolvedEntry> = entries
        .iter()
        .filter(|(n, _)| n == "README.md")
        .map(|(_, e)| e)
        .collect();
    assert_eq!(readmes.len(), 1, "real README.md should be shadowed");
    assert!(matches!(readmes[0], ResolvedEntry::Synthetic { .. }));
}

#[test]
fn overlay_synthetic_directory_walks() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head_hex) = build_fixture("ov_dir");
    let store = ObjectStore::open(&repo).unwrap();
    let proj = Projection::Commit(parse_oid(&head_hex));

    let mut overlay = RootOverlay::new();
    let mut projgit_dir = SyntheticEntry::directory();
    projgit_dir.insert_child(
        bstr::BString::from("info.json"),
        SyntheticEntry::file(b"{\"v\":1}".to_vec()),
    );
    overlay.insert(bstr::BString::from(".projgit"), projgit_dir);

    // Walking into the synthetic directory works.
    let resolved = proj.lookup(&store, &overlay, ".projgit/info.json").unwrap();
    match resolved {
        ResolvedEntry::Synthetic { entry, .. } => {
            assert!(matches!(
                entry,
                SyntheticEntry::File { content, .. } if content == b"{\"v\":1}"
            ));
        }
        other => panic!("expected Synthetic file, got {other:?}"),
    }

    // The directory itself resolves to the directory entry.
    let resolved = proj.lookup(&store, &overlay, ".projgit").unwrap();
    assert!(matches!(
        resolved,
        ResolvedEntry::Synthetic {
            entry: SyntheticEntry::Directory { .. },
            ..
        }
    ));
}
