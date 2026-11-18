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
    let base = std::env::temp_dir().join(format!("projgit-pfs-{}-{}", name, std::process::id()));
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

/// Build a `ProjectionFsProvider` for `Projection::Commit(head)`
/// over a `NoopFetcher`-backed hydrating store. Empty overlay.
fn provider_for(repo: &Path, head: gix::ObjectId) -> ProjectionFsProvider<NoopFetcher> {
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

    assert_eq!(
        p.lookup(ROOT_INODE, b"does-not-exist"),
        Err(FsError::NotFound)
    );
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
fn readdir_t1_prefetch_warms_header_cache() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("t1_prefetch");
    let p = provider_for(&repo, head);

    // Cold: header cache should be empty.
    let h0 = p.store().store().header_cache_stats();
    assert_eq!(h0.inserts, 0, "header cache starts empty");
    assert_eq!(h0.len, 0);

    // Trigger root readdir; this posts blob/symlink OIDs to the
    // T1 prefetch worker. With NoopFetcher, prefetch_headers takes
    // the cache-miss branch, then the fetcher returns
    // NotHydratable (no upstream) -- but importantly,
    // HydratingObjectStore's pre-pass `store.header()` call **does**
    // populate the header cache for OIDs that are locally present
    // (which they all are in this fixture). So the cache warms even
    // with NoopFetcher.
    let _ = p.readdir(ROOT_INODE, 0).unwrap();

    // The worker runs on a background thread; give it a brief
    // window to drain. We poll the prefetch counters rather than
    // sleeping a fixed duration to keep the test responsive on a
    // loaded host.
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(2) {
        let s = p.prefetch_stats();
        if s.batches_sent > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let pf = p.prefetch_stats();
    assert_eq!(pf.posted, 1, "exactly one task posted from root readdir");
    assert!(
        pf.batches_sent >= 1,
        "worker should have processed at least one batch: {pf:?}"
    );
    // The fixture root has README.md, link-to-readme, run.sh -- 3
    // blobs/symlinks. (`src` is a directory; harvested OIDs skip it.)
    assert_eq!(
        pf.oids_resolved, 3,
        "expected one resolved probe per blob/symlink in root: {pf:?}"
    );

    // Cache should now have those 3 entries (populated via the
    // pre-pass `store.header()` call inside HydratingObjectStore).
    let h1 = p.store().store().header_cache_stats();
    assert_eq!(
        h1.len, 3,
        "header cache should contain the 3 root-level blob/symlink headers: {h1:?}"
    );
    assert!(h1.inserts >= 3);
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

    let base = std::env::temp_dir().join(format!("projgit-pfs-gitlink-{}", std::process::id()));
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
    let p = ProjectionFsProvider::new(Projection::Commit(head), hydrating, RootOverlay::new(), 3)
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

#[test]
fn backing_file_returns_uncompressed_host_file_for_tree_files() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("backing_file");

    let mut overlay = RootOverlay::new();
    overlay.insert(
        BString::from(".synth-file"),
        SyntheticEntry::file(b"in-memory overlay\n".to_vec()),
    );

    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), NoopFetcher));
    let p = ProjectionFsProvider::new(Projection::Commit(head), hydrating, overlay, 1).unwrap();

    // 1. Projected regular file -> returns Some(Arc<File>) backed by
    //    <git-dir>/blobs/<xx>/<oid> for virtio-fs DAX / passthrough.
    let readme = lookup(&p, ROOT_INODE, b"README.md");
    let backing = p
        .backing_file(readme.inode)
        .unwrap()
        .expect("tree file must return a host backing File");
    let mut file_bytes = Vec::new();
    {
        use std::io::Read;
        (&*backing).read_to_end(&mut file_bytes).unwrap();
    }
    assert_eq!(&file_bytes, b"# fixture repo\n");
    assert!(store.disk_blob_cache_stats().inserts >= 1);

    // Second backing_file call hits the already-materialized blob.
    let hits_before = store.disk_blob_cache_stats().hits;
    let _backing2 = p.backing_file(readme.inode).unwrap().unwrap();
    assert!(store.disk_blob_cache_stats().hits > hits_before);

    // 2. Synthetic overlay file -> returns Ok(None) (in-memory buffer).
    let synth = lookup(&p, ROOT_INODE, b".synth-file");
    assert!(p.backing_file(synth.inode).unwrap().is_none());

    // 3. Directory and symlink inodes -> Err(FsError::NotAFile).
    assert_eq!(p.backing_file(ROOT_INODE).unwrap_err(), FsError::NotAFile);
    let src_dir = lookup(&p, ROOT_INODE, b"src");
    assert_eq!(
        p.backing_file(src_dir.inode).unwrap_err(),
        FsError::NotAFile
    );
    let symlink = lookup(&p, ROOT_INODE, b"link-to-readme");
    assert_eq!(
        p.backing_file(symlink.inode).unwrap_err(),
        FsError::NotAFile
    );

    // 4. Unknown inode -> Err(FsError::NotFound).
    assert_eq!(p.backing_file(0xDEAD_BEEF).unwrap_err(), FsError::NotFound);
}

#[test]
fn isolated_overlay_host_files_support_positioned_reads_and_backing_file() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("isolated_host_files");
    git(&repo, &["repack", "-a", "-d", "-q"]);
    git(&repo, &["commit-graph", "write", "--reachable"]);

    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let overlay = projgit_core::dotgit::a1_plus_isolated_overlay(&store, head).unwrap();
    let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), NoopFetcher));
    let p = ProjectionFsProvider::new(Projection::Commit(head), hydrating, overlay, 7).unwrap();

    let dotgit = lookup(&p, ROOT_INODE, b".git");
    let objects = lookup(&p, dotgit.inode, b"objects");
    let info = lookup(&p, objects.inode, b"info");
    let pack_dir = lookup(&p, objects.inode, b"pack");

    // 1. .git/objects/info/alternates is strictly absent.
    assert_eq!(
        p.lookup(info.inode, b"alternates").unwrap_err(),
        FsError::NotFound
    );

    // 2. .git/objects/info/commit-graph is present with mode 0o444 and
    //    exact host byte length.
    let cg_attr = lookup(&p, info.inode, b"commit-graph");
    let host_cg_bytes = std::fs::read(store.git_dir().join("objects/info/commit-graph")).unwrap();
    assert_eq!(cg_attr.kind, FileType::RegularFile);
    assert_eq!(cg_attr.mode, 0o444);
    assert_eq!(cg_attr.size, host_cg_bytes.len() as u64);
    assert_eq!(cg_attr.mtime, p.commit_time());

    // 3. .git/objects/pack lists pack-*.pack and pack-*.idx as regular files.
    let pack_entries = p.readdir(pack_dir.inode, 0).unwrap();
    let pack_entry = pack_entries
        .iter()
        .find(|e| e.name.ends_with(b".pack"))
        .expect("pack-*.pack entry in .git/objects/pack");
    assert_eq!(pack_entry.kind, FileType::RegularFile);

    let pack_attr = lookup(&p, pack_dir.inode, pack_entry.name.as_ref());
    let host_pack_path = store
        .git_dir()
        .join("objects/pack")
        .join(std::str::from_utf8(pack_entry.name.as_ref()).unwrap());
    let host_pack_bytes = std::fs::read(&host_pack_path).unwrap();
    assert_eq!(pack_attr.kind, FileType::RegularFile);
    assert_eq!(pack_attr.mode, 0o444);
    assert_eq!(pack_attr.size, host_pack_bytes.len() as u64);
    assert_eq!(pack_attr.mtime, p.commit_time());

    // 4. Positioned reads (`ProjectionFsProvider::read`) return exact slices
    //    from the packfile and handle EOF clamping / past-EOF offsets.
    let header_slice = p.read(pack_attr.inode, 0, 12).unwrap();
    assert_eq!(&header_slice[..4], b"PACK");
    assert_eq!(&header_slice, &host_pack_bytes[..12]);

    let mid_offset = (host_pack_bytes.len() / 3) as u64;
    let mid_slice = p.read(pack_attr.inode, mid_offset, 32).unwrap();
    let expected_end = (mid_offset as usize + 32).min(host_pack_bytes.len());
    assert_eq!(
        &mid_slice,
        &host_pack_bytes[mid_offset as usize..expected_end]
    );

    // Clamping across EOF and reading at/past EOF.
    let tail_offset = host_pack_bytes.len().saturating_sub(10) as u64;
    let tail_slice = p.read(pack_attr.inode, tail_offset, 64).unwrap();
    assert_eq!(&tail_slice, &host_pack_bytes[tail_offset as usize..]);
    assert!(p
        .read(pack_attr.inode, host_pack_bytes.len() as u64 + 16, 32)
        .unwrap()
        .is_empty());

    // 5. `backing_file` returns Ok(Some(Arc<File>)) for projected HostFile
    //    inodes so virtio-fs DAX / FUSE passthrough can map packfiles directly.
    let backing = p
        .backing_file(pack_attr.inode)
        .unwrap()
        .expect("HostFile must return Some(Arc<File>)");
    let mut backing_bytes = Vec::new();
    {
        use std::io::Read;
        (&*backing).read_to_end(&mut backing_bytes).unwrap();
    }
    assert_eq!(backing_bytes, host_pack_bytes);
}

#[test]
fn readdirplus_returns_child_attrs_and_warms_attr_snapshots() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let (repo, head) = build_fixture("pfs_readdirplus");
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let overlay = projgit_core::dotgit::a1_plus_isolated_overlay(&store, head).unwrap();
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let p = ProjectionFsProvider::new(Projection::Commit(head), hydrating, overlay, 1).unwrap();

    // Call `readdirplus` on ROOT_INODE *before* calling `lookup` on any child.
    let entries = p.readdirplus(ROOT_INODE, 0).unwrap();
    assert!(
        entries.len() >= 4,
        "expected .git, README.md, link-to-readme, run.sh, src"
    );
    let dotgit = entries
        .iter()
        .find(|e| e.name.as_slice() == b".git")
        .expect(".git in readdirplus");
    assert_eq!(dotgit.attr.kind, FileType::Directory);

    let readme = entries
        .iter()
        .find(|e| e.name.as_slice() == b"README.md")
        .expect("README.md in readdirplus");
    assert_eq!(readme.attr.kind, FileType::RegularFile);
    assert!(readme.attr.size > 0);
    assert_eq!(readme.attr.inode, readme.inode);

    // Because `readdirplus` published the child's AttrSnapshot in a single pass,
    // `getattr(readme.inode)` and `read(readme.inode, ...)` succeed immediately
    // even though `lookup(ROOT_INODE, "README.md")` was never called!
    let direct_attr = p.getattr(readme.inode).expect("getattr after readdirplus");
    assert_eq!(direct_attr, readme.attr);
    let data = p
        .read(readme.inode, 0, 1024)
        .expect("read after readdirplus");
    assert_eq!(data.len() as u64, readme.attr.size);

    // Pagination via offset works identically to `readdir`.
    let tail = p.readdirplus(ROOT_INODE, 2).unwrap();
    assert_eq!(tail.len(), entries.len() - 2);
    assert_eq!(tail[0], entries[2]);
}
