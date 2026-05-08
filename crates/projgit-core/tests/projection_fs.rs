//! Integration tests for [`projgit_core::ProjectionFsProvider`].
//!
//! Builds small fixture repos via the system `git` CLI (mirrors
//! `tests/integration.rs`) and exercises the FsProvider methods
//! against a `Projection` over a `HydratingObjectStore<NoopFetcher>`.

use bstr::{BString, ByteSlice};
use projgit_core::{
    Attr, DirEntry, FileType, FsError, FsProvider, HydratingObjectStore, NoopFetcher, ObjectStore,
    Projection, ProjectionFsProvider, RootOverlay, SyntheticEntry, ROOT_INODE,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

// -----------------------------------------------------------------------------
// Fixture-repo helpers (duplicated from tests/integration.rs by design — each
// test binary is its own crate, and cross-binary helpers would require a
// `tests/common/mod.rs` which adds more ceremony than it saves).
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

/// Build a small repo with the standard fixture layout.
///
/// ```text
/// README.md
/// run.sh             (executable in the index, 100755)
/// src/
///   main.c
///   util/
///     helper.c
///     helper.h
/// link-to-readme     (symlink → README.md, 120000)
/// ```
fn build_fixture(name: &str) -> (PathBuf, gix::ObjectId) {
    let base = std::env::temp_dir().join(format!(
        "projgit-pfs-{}-{}",
        name,
        std::process::id()
    ));
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

    // Add a symlink directly to the index by writing a blob whose
    // content is the link target string and registering it under
    // mode 120000. Works the same on Windows and POSIX.
    let target = b"README.md";
    let target_path = base.join(".symlink-target.tmp");
    std::fs::write(&target_path, target).unwrap();
    let blob_hex = String::from_utf8(git(
        &base,
        &[
            "hash-object",
            "-w",
            target_path.to_str().unwrap(),
        ],
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

/// Build a `ProjectionFsProvider` for `Projection::Commit(head)`
/// over a `NoopFetcher`-backed hydrating store. Empty overlay.
fn provider_for(
    repo: &Path,
    head: gix::ObjectId,
) -> ProjectionFsProvider<NoopFetcher> {
    let store = Arc::new(ObjectStore::open(repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    ProjectionFsProvider::new(
        Projection::Commit(head),
        hydrating,
        RootOverlay::new(),
        /* projection_id */ 1,
    )
    .expect("provider construction")
}

fn entry_names(entries: &[DirEntry]) -> Vec<&[u8]> {
    entries.iter().map(|e| e.name.as_slice()).collect()
}

fn lookup(provider: &ProjectionFsProvider<NoopFetcher>, parent: u64, name: &[u8]) -> Attr {
    provider
        .lookup(parent, name)
        .unwrap_or_else(|e| panic!("lookup {} failed: {e:?}", name.as_bstr()))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn root_readdir_lists_real_entries_sorted() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("root_readdir");
    let p = provider_for(&repo, head);

    let entries = p.readdir(ROOT_INODE, 0).unwrap();
    let names = entry_names(&entries);
    // Git stores tree entries in byte-wise sorted order with directories
    // suffixed by '/'. Expected: README.md, link-to-readme, run.sh, src.
    assert_eq!(
        names,
        vec![
            b"README.md".as_ref(),
            b"link-to-readme".as_ref(),
            b"run.sh".as_ref(),
            b"src".as_ref(),
        ]
    );

    // Pagination contract: offset > 0 → empty.
    let again = p.readdir(ROOT_INODE, 1).unwrap();
    assert!(again.is_empty());
}

#[test]
fn lookup_regular_file_returns_blob_size_and_mode() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("lookup_file");
    let p = provider_for(&repo, head);

    let attr = lookup(&p, ROOT_INODE, b"README.md");
    assert_eq!(attr.kind, FileType::RegularFile);
    assert_eq!(attr.size, b"# fixture repo\n".len() as u64);
    assert_eq!(attr.mode, 0o644);
    // mtime stamped from commit time, not UNIX_EPOCH.
    assert_ne!(attr.mtime, std::time::SystemTime::UNIX_EPOCH);
    assert_eq!(attr.mtime, p.commit_time());
}

#[test]
fn lookup_executable_file_reports_0o755() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("lookup_exec");
    let p = provider_for(&repo, head);

    let attr = lookup(&p, ROOT_INODE, b"run.sh");
    assert_eq!(attr.kind, FileType::RegularFile);
    assert_eq!(attr.mode, 0o755);
}

#[test]
fn lookup_nested_path_via_intermediate_lookups() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("lookup_nested");
    let p = provider_for(&repo, head);

    let src = lookup(&p, ROOT_INODE, b"src");
    assert_eq!(src.kind, FileType::Directory);

    let util = lookup(&p, src.inode, b"util");
    assert_eq!(util.kind, FileType::Directory);

    let header = lookup(&p, util.inode, b"helper.h");
    assert_eq!(header.kind, FileType::RegularFile);
    assert_eq!(header.size, b"void helper(void);\n".len() as u64);
}

#[test]
fn lookup_missing_entry_returns_not_found() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("lookup_missing");
    let p = provider_for(&repo, head);

    assert_eq!(p.lookup(ROOT_INODE, b"does-not-exist"), Err(FsError::NotFound));
}

#[test]
fn read_returns_blob_bytes_with_offset_and_eof_clamp() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("read_blob");
    let p = provider_for(&repo, head);

    let attr = lookup(&p, ROOT_INODE, b"README.md");
    let full = p.read(attr.inode, 0, 1024).unwrap();
    assert_eq!(&full, b"# fixture repo\n");

    // Offset slice.
    let tail = p.read(attr.inode, 2, 1024).unwrap();
    assert_eq!(&tail, b"fixture repo\n");

    // Read past EOF returns empty.
    let none = p.read(attr.inode, 1024, 1024).unwrap();
    assert!(none.is_empty());

    // Bounded size returns exactly that many bytes.
    let head_only = p.read(attr.inode, 0, 5).unwrap();
    assert_eq!(&head_only, b"# fix");
}

#[test]
fn read_on_directory_inode_returns_not_a_file() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("read_on_dir");
    let p = provider_for(&repo, head);

    let dir = lookup(&p, ROOT_INODE, b"src");
    assert_eq!(p.read(dir.inode, 0, 16).unwrap_err(), FsError::NotAFile);
}

#[test]
fn getattr_unknown_inode_returns_not_found() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("getattr_unknown");
    let p = provider_for(&repo, head);

    assert_eq!(p.getattr(0xDEAD_BEEF), Err(FsError::NotFound));
}

#[test]
fn getattr_root_returns_directory_with_commit_mtime() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("getattr_root");
    let p = provider_for(&repo, head);

    let root = p.getattr(ROOT_INODE).unwrap();
    assert_eq!(root.inode, ROOT_INODE);
    assert_eq!(root.kind, FileType::Directory);
    assert_eq!(root.mtime, p.commit_time());
}

#[test]
fn readdir_into_subdirectory_lists_children() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("readdir_sub");
    let p = provider_for(&repo, head);

    // Must lookup() first to populate the cache for the non-root inode.
    let src = lookup(&p, ROOT_INODE, b"src");
    let entries = p.readdir(src.inode, 0).unwrap();
    let names = entry_names(&entries);
    assert_eq!(names, vec![b"main.c".as_ref(), b"util".as_ref()]);

    // Drill one more level.
    let util = lookup(&p, src.inode, b"util");
    let entries = p.readdir(util.inode, 0).unwrap();
    let names = entry_names(&entries);
    assert_eq!(names, vec![b"helper.c".as_ref(), b"helper.h".as_ref()]);
}

#[test]
fn symlink_attr_and_readlink() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("symlink");
    let p = provider_for(&repo, head);

    let link = lookup(&p, ROOT_INODE, b"link-to-readme");
    assert_eq!(link.kind, FileType::Symlink);
    assert_eq!(link.mode, 0o777);
    assert_eq!(link.size, b"README.md".len() as u64);

    let target = p.readlink(link.inode).unwrap();
    assert_eq!(target, BString::from("README.md"));

    // readlink on a non-symlink (regular file) errors.
    let readme = lookup(&p, ROOT_INODE, b"README.md");
    assert_eq!(p.readlink(readme.inode).unwrap_err(), FsError::NotASymlink);
}

#[test]
fn root_overlay_shadows_real_entries_and_supports_nested_dirs() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("overlay_shadow");

    // Construct the provider with a non-empty overlay.
    let mut overlay = RootOverlay::new();
    // Synthetic file shadows the real README.md.
    overlay.insert(
        BString::from("README.md"),
        SyntheticEntry::file(b"# synthetic\n".to_vec()),
    );
    // Synthetic top-level directory with a child file.
    let mut dir = SyntheticEntry::directory();
    dir.insert_child(
        BString::from("info.json"),
        SyntheticEntry::file(b"{\"projection\":\"test\"}".to_vec()),
    );
    overlay.insert(BString::from(".projgit"), dir);

    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let p = ProjectionFsProvider::new(
        Projection::Commit(head),
        hydrating,
        overlay,
        /* projection_id */ 2,
    )
    .unwrap();

    // Root listing: synthetic entries first (sorted), then real
    // entries minus the shadowed README.md.
    let entries = p.readdir(ROOT_INODE, 0).unwrap();
    let names = entry_names(&entries);
    assert_eq!(
        names,
        vec![
            b".projgit".as_ref(),
            b"README.md".as_ref(),
            b"link-to-readme".as_ref(),
            b"run.sh".as_ref(),
            b"src".as_ref(),
        ]
    );

    // The README.md entry is the synthetic one.
    let readme_attr = lookup(&p, ROOT_INODE, b"README.md");
    let bytes = p.read(readme_attr.inode, 0, 1024).unwrap();
    assert_eq!(&bytes, b"# synthetic\n");

    // Synthetic directory is descendable.
    let synth_dir = lookup(&p, ROOT_INODE, b".projgit");
    assert_eq!(synth_dir.kind, FileType::Directory);
    let inner = p.readdir(synth_dir.inode, 0).unwrap();
    let names = entry_names(&inner);
    assert_eq!(names, vec![b"info.json".as_ref()]);

    let info = lookup(&p, synth_dir.inode, b"info.json");
    let body = p.read(info.inode, 0, 1024).unwrap();
    assert_eq!(&body, b"{\"projection\":\"test\"}");
}

#[test]
fn gitlink_renders_as_empty_directory() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let base = std::env::temp_dir().join(format!(
        "projgit-pfs-gitlink-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "core.autocrlf", "false"]);

    std::fs::write(base.join("README.md"), b"top\n").unwrap();
    git(&base, &["add", "-A"]);

    // Inject a gitlink entry. The OID need not exist as an object
    // (git itself doesn't dereference it during commit-tree).
    let bogus_commit = "1111111111111111111111111111111111111111";
    git(
        &base,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{bogus_commit},submodule"),
        ],
    );
    git(&base, &["commit", "-q", "-m", "with gitlink"]);
    let head_hex = String::from_utf8(git(&base, &["rev-parse", "HEAD"])).unwrap();
    let head = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).unwrap();

    let store = Arc::new(ObjectStore::open(&base).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let p = ProjectionFsProvider::new(
        Projection::Commit(head),
        hydrating,
        RootOverlay::new(),
        3,
    )
    .unwrap();

    // The gitlink shows up at the projection root as a directory.
    let entries = p.readdir(ROOT_INODE, 0).unwrap();
    let names = entry_names(&entries);
    assert!(names.contains(&b"submodule".as_ref()));

    let sub = lookup(&p, ROOT_INODE, b"submodule");
    assert_eq!(sub.kind, FileType::Directory);

    // Reading it lists no children.
    let inner = p.readdir(sub.inode, 0).unwrap();
    assert!(inner.is_empty());
}
