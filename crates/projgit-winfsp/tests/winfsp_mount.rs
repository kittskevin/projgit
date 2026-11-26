//! Live Windows integration tests for `projgit-winfsp` using our custom
//! permissive (`MIT OR Apache-2.0`) WinFsp FFI bindings.

#![cfg(target_os = "windows")]

use std::collections::BTreeSet;
use std::fs;
use std::process::Command;
use std::sync::{Arc, Mutex};

use projgit_core::dotgit::a1_plus_isolated_overlay;
use projgit_core::{
    HydratingObjectStore, InMemoryFsProvider, NoopFetcher, ObjectStore, Projection,
    ProjectionFsProvider, ROOT_INODE,
};
use projgit_winfsp::{
    is_winfsp_available, mount_background, query_winfsp_version, MountConfig, SymlinkMode,
};
use tempfile::TempDir;

/// Serialize live WinFsp mount/unmount operations across test threads.
static MOUNT_LOCK: Mutex<()> = Mutex::new(());

fn ensure_winfsp_or_skip() -> bool {
    if is_winfsp_available() {
        true
    } else if std::env::var_os("PROJGIT_REQUIRE_WINFSP").is_some() {
        panic!("winfsp-x64.dll is required (PROJGIT_REQUIRE_WINFSP is set) but not found");
    } else {
        eprintln!("skipping live WinFsp test because winfsp-x64.dll is not installed");
        false
    }
}

#[test]
fn winfsp_dll_loads_and_reports_version() {
    if !ensure_winfsp_or_skip() {
        return;
    }
    let (major, minor) = query_winfsp_version().expect("query WinFsp version");
    assert!(major >= 1, "unexpected WinFsp version: {}.{}", major, minor);
}

#[test]
fn in_memory_mount_serves_files_dirs_and_native_symlinks() {
    if !ensure_winfsp_or_skip() {
        return;
    }
    let _guard = MOUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let provider = InMemoryFsProvider::new();
    provider.add_file(
        ROOT_INODE,
        b"hello.txt",
        b"hello from projgit-winfsp!\n".to_vec(),
    );
    let src_dir = provider.add_dir(ROOT_INODE, b"src");
    provider.add_file(
        src_dir,
        b"lib.rs",
        b"pub fn answer() -> u32 {\n    42\n}\n".to_vec(),
    );
    let docs_dir = provider.add_dir(ROOT_INODE, b"docs");
    provider.add_file(docs_dir, b"guide.md", b"# Guide\nDetails here.\n".to_vec());

    // Relative file symlink (`latest.rs` -> `src/lib.rs`) and relative directory
    // symlink (`code` -> `src`).
    provider.add_symlink(ROOT_INODE, b"latest.rs", b"src/lib.rs");
    provider.add_symlink(ROOT_INODE, b"code", b"src");

    let parent_temp = TempDir::new().expect("create temp parent");
    // WinFsp requires the directory mountpoint path not to exist yet (it creates
    // the mount reparse point directory itself).
    let mount_path = parent_temp.path().join("mnt");

    let config = MountConfig {
        name: "projgit".to_string(),
        volume_label: "projgit-test".to_string(),
        symlink_mode: SymlinkMode::NativeReparse,
        n_threads: 0,
    };

    let session =
        mount_background(Arc::new(provider), &mount_path, &config).expect("mount WinFsp volume");
    assert_eq!(session.mountpoint(), mount_path.as_path());

    // 1. Read root and nested files.
    let hello = fs::read_to_string(mount_path.join("hello.txt")).expect("read hello.txt");
    assert_eq!(hello, "hello from projgit-winfsp!\n");

    let lib_rs =
        fs::read_to_string(mount_path.join("src").join("lib.rs")).expect("read src/lib.rs");
    assert_eq!(lib_rs, "pub fn answer() -> u32 {\n    42\n}\n");

    // 2. Directory enumeration at root and in nested directory.
    let root_entries: BTreeSet<String> = fs::read_dir(&mount_path)
        .expect("read_dir root")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        root_entries,
        BTreeSet::from([
            "code".to_string(),
            "docs".to_string(),
            "hello.txt".to_string(),
            "latest.rs".to_string(),
            "src".to_string(),
        ])
    );

    let src_entries: Vec<String> = fs::read_dir(mount_path.join("src"))
        .expect("read_dir src")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(src_entries, vec!["lib.rs".to_string()]);

    // 3. Read-only enforcement: writing or creating a file must fail.
    let write_err = fs::write(mount_path.join("new.txt"), b"nope").expect_err("write must fail");
    assert!(
        write_err.kind() == std::io::ErrorKind::PermissionDenied
            || write_err.raw_os_error().is_some(),
        "unexpected error writing to read-only mount: {write_err:?}"
    );

    // 4. Native symlink reparse point inspection and resolution.
    let sym_meta =
        fs::symlink_metadata(mount_path.join("latest.rs")).expect("symlink_metadata latest.rs");
    assert!(
        sym_meta.file_type().is_symlink(),
        "latest.rs should be reported as a native symlink reparse point"
    );
    let link_target = fs::read_link(mount_path.join("latest.rs")).expect("read_link latest.rs");
    assert_eq!(link_target.to_string_lossy(), "src\\lib.rs");

    // Reading through the file symlink (`latest.rs` -> `src\lib.rs`).
    let via_file_symlink =
        fs::read_to_string(mount_path.join("latest.rs")).expect("read through file symlink");
    assert_eq!(via_file_symlink, lib_rs);

    // Reading through the directory symlink (`code\lib.rs` -> `src\lib.rs`).
    let dir_sym_meta =
        fs::symlink_metadata(mount_path.join("code")).expect("symlink_metadata code");
    assert!(
        dir_sym_meta.file_type().is_symlink(),
        "code should be reported as a directory symlink reparse point"
    );
    let via_dir_symlink = fs::read_to_string(mount_path.join("code").join("lib.rs"))
        .expect("read through directory symlink");
    assert_eq!(via_dir_symlink, lib_rs);

    assert!(session.read_stats().provider_reads > 0);
    session.unmount();
}

#[test]
fn plain_file_symlink_mode_serves_symlink_as_regular_file() {
    if !ensure_winfsp_or_skip() {
        return;
    }
    let _guard = MOUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let provider = InMemoryFsProvider::new();
    provider.add_file(ROOT_INODE, b"target.txt", b"real target\n".to_vec());
    provider.add_symlink(ROOT_INODE, b"link.txt", b"target.txt");

    let parent_temp = TempDir::new().expect("create temp parent");
    let mount_path = parent_temp.path().join("mnt-plain");

    let config = MountConfig {
        name: "projgit".to_string(),
        volume_label: "projgit-plain".to_string(),
        symlink_mode: SymlinkMode::PlainFile,
        n_threads: 0,
    };

    let session = mount_background(Arc::new(provider), &mount_path, &config)
        .expect("mount plain symlink mode");
    let meta = fs::symlink_metadata(mount_path.join("link.txt")).expect("metadata link.txt");
    assert!(
        meta.file_type().is_file(),
        "PlainFile mode should expose symlink as a regular file"
    );
    let content = fs::read_to_string(mount_path.join("link.txt")).expect("read link.txt");
    assert_eq!(content, "target.txt");
    session.unmount();
}

#[test]
fn mount_onto_existing_empty_directory_succeeds_and_restores_dir_on_unmount() {
    if !ensure_winfsp_or_skip() {
        return;
    }
    let _guard = MOUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let provider = InMemoryFsProvider::new();
    provider.add_file(
        ROOT_INODE,
        b"inside.txt",
        b"mounted on existing empty dir".to_vec(),
    );

    let parent_temp = TempDir::new().expect("create temp parent");
    let mount_path = parent_temp.path().join("existing-empty-dir");
    fs::create_dir(&mount_path).expect("pre-create empty mountpoint directory");
    assert!(mount_path.is_dir());

    let session = mount_background(Arc::new(provider), &mount_path, &MountConfig::default())
        .expect("mount onto existing empty directory");
    let content = fs::read_to_string(mount_path.join("inside.txt")).expect("read inside.txt");
    assert_eq!(content, "mounted on existing empty dir");

    session.unmount();
    assert!(
        mount_path.is_dir(),
        "empty mountpoint directory should be restored on unmount"
    );
}

#[test]
fn real_git_projection_mounts_serves_l2_host_files_and_passes_git_cli() {
    if !ensure_winfsp_or_skip() {
        return;
    }
    let _guard = MOUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 1. Create a real temporary Git repository using `git` CLI.
    let repo_temp = TempDir::new().expect("create temp repo");
    let repo_dir = repo_temp.path().join("origin-repo");
    fs::create_dir_all(repo_dir.join("src")).expect("create src dir");
    fs::write(repo_dir.join("README.md"), b"# ProjGit WinFsp Test\n").expect("write README.md");
    fs::write(
        repo_dir.join("src").join("main.rs"),
        b"fn main() {\n    println!(\"WinFsp projection works!\");\n}\n",
    )
    .expect("write src/main.rs");

    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo_dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    run_git(&["init", "--initial-branch=main"]);
    run_git(&["config", "user.name", "ProjGit Test"]);
    run_git(&["config", "user.email", "test@projgit.local"]);
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "Initial commit"]);
    run_git(&["gc", "--quiet"]);
    let head_hex = run_git(&["rev-parse", "HEAD"]);

    // 2. Open `ObjectStore` + `ProjectionFsProvider` with isolated `.git/` overlay.
    let git_dir = repo_dir.join(".git");
    let store = Arc::new(ObjectStore::open(&git_dir).expect("open ObjectStore"));
    let head_oid = gix::ObjectId::from_hex(head_hex.as_bytes()).expect("parse HEAD oid");
    let projection = Projection::Commit(head_oid);
    let overlay = a1_plus_isolated_overlay(&store, head_oid).expect("overlay");

    let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(projection, hydrating, overlay, 1)
            .expect("build ProjectionFsProvider"),
    );

    let mount_parent = TempDir::new().expect("create mount parent");
    let mount_path = mount_parent.path().join("worktree");

    let session = mount_background(provider, &mount_path, &MountConfig::default())
        .expect("mount ProjectionFsProvider on WinFsp");

    // First read of `README.md` populates the uncompressed L2 blob cache on disk;
    // second open + read attaches `FsProvider::backing_file` (`Arc<std::fs::File>`)
    // and reads directly via `FileExt::seek_read`.
    let readme_1 = fs::read_to_string(mount_path.join("README.md")).expect("first read README.md");
    assert_eq!(readme_1, "# ProjGit WinFsp Test\n");
    let readme_2 = fs::read_to_string(mount_path.join("README.md")).expect("second read README.md");
    assert_eq!(readme_2, "# ProjGit WinFsp Test\n");

    let main_rs =
        fs::read_to_string(mount_path.join("src").join("main.rs")).expect("read src/main.rs");
    assert!(main_rs.contains("WinFsp projection works!"));

    // Verify that `host_file_reads` incremented from the L2 uncompressed blob cache fast path!
    let stats_after_readme = session.read_stats();
    assert!(
        stats_after_readme.host_file_reads >= 1,
        "expected direct host_file_reads >= 1 on second open of L2 cached blob, got {:?}",
        stats_after_readme
    );

    // 3. Run real `git` CLI commands inside the mounted WinFsp projection!
    // Notably, we do NOT pass `safe.directory=*`: our current-user SDDL security
    // descriptor ensures Git for Windows recognizes the mounted directory as
    // owned by the current user.
    let rev_parse_out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&mount_path)
        .output()
        .expect("run git rev-parse HEAD in mount");
    assert!(
        rev_parse_out.status.success(),
        "git rev-parse HEAD failed inside WinFsp mount: {}",
        String::from_utf8_lossy(&rev_parse_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&rev_parse_out.stdout).trim(),
        head_hex
    );

    let log_out = Command::new("git")
        .args(["log", "-n", "1", "--oneline"])
        .current_dir(&mount_path)
        .output()
        .expect("run git log in mount");
    assert!(
        log_out.status.success(),
        "git log failed inside WinFsp mount: {}",
        String::from_utf8_lossy(&log_out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&log_out.stdout).contains("Initial commit"),
        "unexpected git log output: {}",
        String::from_utf8_lossy(&log_out.stdout)
    );

    let status_out = Command::new("git")
        .args(["--no-optional-locks", "status", "--short"])
        .current_dir(&mount_path)
        .output()
        .expect("run git status in mount");
    assert!(
        status_out.status.success(),
        "git status failed inside WinFsp mount: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&status_out.stdout).trim(),
        "",
        "expected clean `git status --short` inside mounted projection"
    );

    session.unmount();
}
