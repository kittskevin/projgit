//! End-to-end integration tests for `projgit-virtiofs` against a real
//! `ProjectionFsProvider` with `dotgit::a1_plus_isolated_overlay`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use projgit_core::{
    dotgit, HydratingObjectStore, NoopFetcher, ObjectStore, Projection, ProjectionFsProvider,
    ROOT_INODE,
};
use projgit_virtiofs::{
    dax::MemoryDaxWindow,
    server::serve_stream,
    wire::{
        build_getattr_request, build_init_request, build_lookup_request, build_open_request,
        build_opendir_request, build_read_request, build_readdir_request,
        build_removemapping_request, build_request, build_setupmapping_request, errno, FuseAttrOut,
        FuseDirent, FuseEntryOut, FuseInitOut, FuseOpcode, FuseOpenOut, FuseOutHeader,
        FuseRemoveMappingOne, DEFAULT_DAX_MAP_ALIGNMENT, FUSE_ASYNC_READ,
        FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_MAP_ALIGNMENT, FUSE_OUT_HEADER_LEN,
        FUSE_SETUPMAPPING_FLAG_READ, FUSE_SETUPMAPPING_FLAG_WRITE,
    },
    ProjgitVirtioFs,
};

type TestProvider = ProjectionFsProvider<NoopFetcher>;

struct TestRepo {
    _dir: tempfile::TempDir,
    git_dir: PathBuf,
    commit_oid: gix::ObjectId,
    readme_oid_hex: String,
    pack_name: String,
    pack_bytes: Vec<u8>,
}

impl TestRepo {
    fn create() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();

        let run_git = |args: &[&str]| -> Vec<u8> {
            let out = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "projgit-test")
                .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
                .env("GIT_COMMITTER_NAME", "projgit-test")
                .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };

        run_git(&["init", "-q", "-b", "main"]);
        std::fs::write(
            root.join("README.md"),
            b"# projgit-virtiofs\nZero-copy DAX projection test payload.\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn answer() -> u32 { 42 }\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "initial commit"]);
        // Pack all loose objects into `.git/objects/pack/pack-*.pack` so the
        // isolated overlay projects real HostFile pack entries and blob reads
        // materialize the L2 `<git-dir>/blobs/<xx>/<oid>` cache from packfiles.
        run_git(&["repack", "-a", "-d", "-q"]);

        let head_hex = String::from_utf8(run_git(&["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        let commit_oid = gix::ObjectId::from_hex(head_hex.as_bytes()).unwrap();

        let readme_oid_hex = String::from_utf8(run_git(&["rev-parse", "HEAD:README.md"]))
            .unwrap()
            .trim()
            .to_owned();

        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        let mut pack_name = None;
        let mut pack_bytes = Vec::new();
        for entry in std::fs::read_dir(&pack_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("pack-") && name.ends_with(".pack") {
                pack_bytes = std::fs::read(entry.path()).unwrap();
                pack_name = Some(name);
                break;
            }
        }

        Self {
            _dir: dir,
            git_dir,
            commit_oid,
            readme_oid_hex,
            pack_name: pack_name.expect("expected a pack-*.pack after git repack"),
            pack_bytes,
        }
    }

    fn build_virtiofs(&self) -> (ProjgitVirtioFs<TestProvider, MemoryDaxWindow>, PathBuf) {
        let store = Arc::new(ObjectStore::open(&self.git_dir).expect("open ObjectStore"));
        let hydrating = Arc::new(HydratingObjectStore::new(Arc::clone(&store), NoopFetcher));
        let projection = Projection::Commit(self.commit_oid);
        let overlay = dotgit::a1_plus_isolated_overlay(&store, self.commit_oid)
            .expect("build a1_plus_isolated_overlay");
        let provider =
            ProjectionFsProvider::new(projection, hydrating, overlay, 1).expect("build provider");
        (ProjgitVirtioFs::new(provider), self.git_dir.clone())
    }

    fn blob_cache_path(&self, oid_hex: &str) -> PathBuf {
        let (prefix, _) = oid_hex.split_at(2);
        self.git_dir.join("blobs").join(prefix).join(oid_hex)
    }
}

fn lookup_ino(fs: &ProjgitVirtioFs<TestProvider>, parent: u64, name: &[u8]) -> FuseEntryOut {
    let resp = fs.dispatch(&build_lookup_request(1, parent, name));
    let hdr = FuseOutHeader::decode(&resp).expect("decode FuseOutHeader");
    assert_eq!(
        hdr.error,
        0,
        "lookup {:?} under parent {parent} failed with {}",
        String::from_utf8_lossy(name),
        hdr.error
    );
    FuseEntryOut::decode(&resp[FUSE_OUT_HEADER_LEN..]).expect("decode FuseEntryOut")
}

fn lookup_path(fs: &ProjgitVirtioFs<TestProvider>, path: &str) -> FuseEntryOut {
    let mut current_ino = ROOT_INODE;
    let mut last_entry = None;
    for component in Path::new(path).components() {
        let name = component.as_os_str().to_string_lossy();
        let entry = lookup_ino(fs, current_ino, name.as_bytes());
        current_ino = entry.nodeid;
        last_entry = Some(entry);
    }
    last_entry.expect("non-empty path")
}

#[test]
fn fuse_init_negotiates_protocol_and_dax_map_alignment() {
    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();

    let req = build_init_request(
        42,
        FUSE_KERNEL_VERSION,
        FUSE_KERNEL_MINOR_VERSION,
        FUSE_ASYNC_READ | FUSE_MAP_ALIGNMENT,
    );
    let resp = fs.dispatch(&req);
    let hdr = FuseOutHeader::decode(&resp).unwrap();
    assert_eq!(hdr.unique, 42);
    assert_eq!(hdr.error, 0);

    let init_out = FuseInitOut::decode(&resp[FUSE_OUT_HEADER_LEN..]).unwrap();
    assert_eq!(init_out.major, FUSE_KERNEL_VERSION);
    assert_eq!(init_out.minor, FUSE_KERNEL_MINOR_VERSION);
    assert_ne!(init_out.flags & FUSE_MAP_ALIGNMENT, 0);
    assert_ne!(init_out.flags & FUSE_ASYNC_READ, 0);
    assert_eq!(init_out.map_alignment, DEFAULT_DAX_MAP_ALIGNMENT);
}

#[test]
fn fuse_lookup_getattr_open_read_and_readdir_on_projected_tree() {
    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();

    // 1. Root readdir includes `.git`, `README.md`, and `src`.
    let open_dir_resp = fs.dispatch(&build_opendir_request(10, ROOT_INODE, 0));
    let open_dir_hdr = FuseOutHeader::decode(&open_dir_resp).unwrap();
    assert_eq!(open_dir_hdr.error, 0);

    let readdir_resp = fs.dispatch(&build_readdir_request(11, ROOT_INODE, 0, 0, 4096));
    let readdir_hdr = FuseOutHeader::decode(&readdir_resp).unwrap();
    assert_eq!(readdir_hdr.error, 0);
    let dirents = FuseDirent::decode_all(&readdir_resp[FUSE_OUT_HEADER_LEN..]).unwrap();
    let names: Vec<String> = dirents
        .iter()
        .map(|d| String::from_utf8_lossy(&d.name).into_owned())
        .collect();
    assert!(names.contains(&".git".to_owned()));
    assert!(names.contains(&"README.md".to_owned()));
    assert!(names.contains(&"src".to_owned()));

    // 2. Lookup + Getattr + Open + Read on `src/lib.rs`.
    let lib_entry = lookup_path(&fs, "src/lib.rs");
    let expected = b"pub fn answer() -> u32 { 42 }\n";
    assert_eq!(lib_entry.attr.size, expected.len() as u64);

    let getattr_resp = fs.dispatch(&build_getattr_request(12, lib_entry.nodeid));
    let getattr_hdr = FuseOutHeader::decode(&getattr_resp).unwrap();
    assert_eq!(getattr_hdr.error, 0);
    let attr_out = FuseAttrOut::decode(&getattr_resp[FUSE_OUT_HEADER_LEN..]).unwrap();
    assert_eq!(attr_out.attr.ino, lib_entry.nodeid);
    assert_eq!(attr_out.attr.size, expected.len() as u64);

    let open_resp = fs.dispatch(&build_open_request(13, lib_entry.nodeid, 0));
    let open_hdr = FuseOutHeader::decode(&open_resp).unwrap();
    assert_eq!(open_hdr.error, 0);
    let open_out = FuseOpenOut::decode(&open_resp[FUSE_OUT_HEADER_LEN..]).unwrap();
    assert_eq!(open_out.fh, 0);

    let read_resp = fs.dispatch(&build_read_request(14, lib_entry.nodeid, 0, 0, 4096));
    let read_hdr = FuseOutHeader::decode(&read_resp).unwrap();
    assert_eq!(read_hdr.error, 0);
    assert_eq!(&read_resp[FUSE_OUT_HEADER_LEN..], expected);
}

#[test]
fn fuse_setupmapping_on_tree_blob_materializes_l2_cache_maps_dax_and_removemapping_unmaps() {
    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();

    let readme_cache = repo.blob_cache_path(&repo.readme_oid_hex);
    assert!(
        !readme_cache.exists(),
        "L2 uncompressed blob cache should not exist before first FUSE_SETUPMAPPING"
    );

    let readme_entry = lookup_path(&fs, "README.md");
    let expected = b"# projgit-virtiofs\nZero-copy DAX projection test payload.\n";
    assert_eq!(readme_entry.attr.size, expected.len() as u64);

    // Issue FUSE_SETUPMAPPING (opcode 48) at DAX window moffset 0x10_0000.
    let moffset: u64 = 0x0010_0000;
    let map_len: u64 = 4096;
    let setup_req = build_setupmapping_request(
        20,
        readme_entry.nodeid,
        0,
        0,
        map_len,
        FUSE_SETUPMAPPING_FLAG_READ,
        moffset,
    );
    let setup_resp = fs.dispatch(&setup_req);
    let setup_hdr = FuseOutHeader::decode(&setup_resp).unwrap();
    assert_eq!(
        setup_hdr.error, 0,
        "FUSE_SETUPMAPPING on tree blob must succeed"
    );

    // 1. `<git-dir>/blobs/<xx>/<oid>` was materialized on disk.
    assert!(
        readme_cache.exists(),
        "FUSE_SETUPMAPPING must materialize the L2 uncompressed blob at {}",
        readme_cache.display()
    );
    assert_eq!(std::fs::read(&readme_cache).unwrap(), expected);

    // 2. DAX window has the active mapping and reads directly from the mapped file.
    assert_eq!(fs.dax_mapper().mapping_count(), 1);
    let mapping = fs
        .dax_mapper()
        .mapping_at(moffset)
        .expect("mapping at moffset");
    assert_eq!(mapping.ino, readme_entry.nodeid);
    assert_eq!(mapping.foffset, 0);
    assert_eq!(mapping.len, map_len);

    let window_bytes = fs
        .dax_mapper()
        .read_window(moffset, expected.len())
        .expect("read_window from DAX mapping");
    assert_eq!(window_bytes, expected);

    // Sub-range read inside the DAX window (e.g. offset +19 for "Zero-copy DAX").
    let sub_bytes = fs
        .dax_mapper()
        .read_window(moffset + 19, 13)
        .expect("sub-range read_window");
    assert_eq!(sub_bytes, b"Zero-copy DAX");

    // 3. Issue FUSE_REMOVEMAPPING (opcode 49) and verify the DAX mapping is removed.
    let rem_req = build_removemapping_request(
        21,
        readme_entry.nodeid,
        &[FuseRemoveMappingOne {
            moffset,
            len: map_len,
        }],
    );
    let rem_resp = fs.dispatch(&rem_req);
    let rem_hdr = FuseOutHeader::decode(&rem_resp).unwrap();
    assert_eq!(rem_hdr.error, 0, "FUSE_REMOVEMAPPING must succeed");
    assert_eq!(fs.dax_mapper().mapping_count(), 0);
    assert!(fs.dax_mapper().read_window(moffset, 4).is_err());
}

#[test]
fn fuse_setupmapping_on_isolated_pack_hostfile_succeeds_without_alternates() {
    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();

    // 1. Verify `.git/objects/info/alternates` does NOT exist in isolated mode.
    let info_entry = lookup_path(&fs, ".git/objects/info");
    let alt_resp = fs.dispatch(&build_lookup_request(30, info_entry.nodeid, b"alternates"));
    let alt_hdr = FuseOutHeader::decode(&alt_resp).unwrap();
    assert_eq!(
        alt_hdr.error,
        -errno::ENOENT,
        ".git/objects/info/alternates must not exist in isolated mode"
    );

    // 2. Lookup `.git/objects/pack/<pack_name>` (`SyntheticEntry::HostFile`).
    let pack_rel_path = format!(".git/objects/pack/{}", repo.pack_name);
    let pack_entry = lookup_path(&fs, &pack_rel_path);
    assert_eq!(pack_entry.attr.size, repo.pack_bytes.len() as u64);

    // 3. Map the host packfile into the DAX window via FUSE_SETUPMAPPING.
    let moffset: u64 = 0x0040_0000;
    let map_len = (repo.pack_bytes.len() as u64).max(4096);
    let setup_resp = fs.dispatch(&build_setupmapping_request(
        31,
        pack_entry.nodeid,
        0,
        0,
        map_len,
        FUSE_SETUPMAPPING_FLAG_READ,
        moffset,
    ));
    let setup_hdr = FuseOutHeader::decode(&setup_resp).unwrap();
    assert_eq!(
        setup_hdr.error, 0,
        "FUSE_SETUPMAPPING on isolated HostFile pack must succeed"
    );

    // 4. Verify reading through the DAX window returns the exact packfile bytes
    //    (starting with the 4-byte `PACK` magic signature).
    let mapped_pack = fs
        .dax_mapper()
        .read_window(moffset, repo.pack_bytes.len())
        .expect("read packfile through DAX window");
    assert_eq!(&mapped_pack[..4], b"PACK");
    assert_eq!(mapped_pack, repo.pack_bytes);
}

#[test]
fn fuse_setupmapping_on_in_memory_synthetic_files_returns_enosys_and_fuse_read_succeeds() {
    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();

    // 1. `.git/HEAD` is an in-memory SyntheticEntry::File.
    let head_entry = lookup_path(&fs, ".git/HEAD");
    let setup_resp = fs.dispatch(&build_setupmapping_request(
        40,
        head_entry.nodeid,
        0,
        0,
        4096,
        FUSE_SETUPMAPPING_FLAG_READ,
        0x0080_0000,
    ));
    let setup_hdr = FuseOutHeader::decode(&setup_resp).unwrap();
    assert_eq!(
        setup_hdr.error,
        -errno::ENOSYS,
        "in-memory synthetic .git/HEAD must return -ENOSYS on FUSE_SETUPMAPPING"
    );

    // Fallback `FUSE_READ` on `.git/HEAD` returns `<commit_oid>\n`.
    let read_resp = fs.dispatch(&build_read_request(41, head_entry.nodeid, 0, 0, 128));
    let read_hdr = FuseOutHeader::decode(&read_resp).unwrap();
    assert_eq!(read_hdr.error, 0);
    let expected_head = format!("{}\n", repo.commit_oid);
    assert_eq!(&read_resp[FUSE_OUT_HEADER_LEN..], expected_head.as_bytes());

    // 2. `.git/index` is also an in-memory SyntheticEntry::File: returns -ENOSYS
    //    on FUSE_SETUPMAPPING and `DIRC` v2 header on FUSE_READ.
    let index_entry = lookup_path(&fs, ".git/index");
    let idx_setup_resp = fs.dispatch(&build_setupmapping_request(
        42,
        index_entry.nodeid,
        0,
        0,
        4096,
        FUSE_SETUPMAPPING_FLAG_READ,
        0x0090_0000,
    ));
    let idx_setup_hdr = FuseOutHeader::decode(&idx_setup_resp).unwrap();
    assert_eq!(idx_setup_hdr.error, -errno::ENOSYS);

    let idx_read_resp = fs.dispatch(&build_read_request(43, index_entry.nodeid, 0, 0, 12));
    let idx_read_hdr = FuseOutHeader::decode(&idx_read_resp).unwrap();
    assert_eq!(idx_read_hdr.error, 0);
    assert_eq!(
        &idx_read_resp[FUSE_OUT_HEADER_LEN..FUSE_OUT_HEADER_LEN + 4],
        b"DIRC"
    );
}

#[test]
fn write_opcodes_and_writable_dax_mappings_return_erofs() {
    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();

    let write_opcodes = [
        FuseOpcode::Setattr,
        FuseOpcode::Symlink,
        FuseOpcode::Mknod,
        FuseOpcode::Mkdir,
        FuseOpcode::Unlink,
        FuseOpcode::Rmdir,
        FuseOpcode::Rename,
        FuseOpcode::Link,
        FuseOpcode::Write,
        FuseOpcode::Create,
    ];

    for (idx, opcode) in write_opcodes.into_iter().enumerate() {
        let unique = 100 + idx as u64;
        let req = build_request(unique, opcode, ROOT_INODE, &[0u8; 24]);
        let resp = fs.dispatch(&req);
        let hdr = FuseOutHeader::decode(&resp).unwrap();
        assert_eq!(hdr.unique, unique);
        assert_eq!(
            hdr.error,
            -errno::EROFS,
            "write opcode {opcode:?} must return -EROFS"
        );
    }

    // Writable FUSE_SETUPMAPPING must also return -EROFS.
    let readme_entry = lookup_path(&fs, "README.md");
    let writable_map = build_setupmapping_request(
        200,
        readme_entry.nodeid,
        0,
        0,
        4096,
        FUSE_SETUPMAPPING_FLAG_READ | FUSE_SETUPMAPPING_FLAG_WRITE,
        0x0100_0000,
    );
    let resp = fs.dispatch(&writable_map);
    let hdr = FuseOutHeader::decode(&resp).unwrap();
    assert_eq!(hdr.error, -errno::EROFS);
}

#[test]
fn framed_stream_transport_roundtrip() {
    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();

    let mut request_stream = Vec::new();
    projgit_virtiofs::server::write_frame(
        &mut request_stream,
        &build_init_request(1, FUSE_KERNEL_VERSION, FUSE_KERNEL_MINOR_VERSION, 0),
    )
    .unwrap();
    projgit_virtiofs::server::write_frame(
        &mut request_stream,
        &build_lookup_request(2, ROOT_INODE, b"README.md"),
    )
    .unwrap();

    let mut reader = std::io::Cursor::new(request_stream);
    let mut response_stream = Vec::new();
    let processed = serve_stream(&fs, &mut reader, &mut response_stream).unwrap();
    assert_eq!(processed, 2);

    let mut resp_reader = std::io::Cursor::new(response_stream);
    let frame1 = projgit_virtiofs::server::read_frame(&mut resp_reader)
        .unwrap()
        .unwrap();
    let hdr1 = FuseOutHeader::decode(&frame1).unwrap();
    assert_eq!(hdr1.unique, 1);
    assert_eq!(hdr1.error, 0);

    let frame2 = projgit_virtiofs::server::read_frame(&mut resp_reader)
        .unwrap()
        .unwrap();
    let hdr2 = FuseOutHeader::decode(&frame2).unwrap();
    assert_eq!(hdr2.unique, 2);
    assert_eq!(hdr2.error, 0);
    let entry = FuseEntryOut::decode(&frame2[FUSE_OUT_HEADER_LEN..]).unwrap();
    assert!(entry.nodeid != ROOT_INODE);

    // Clean EOF after the 2 frames.
    assert!(projgit_virtiofs::server::read_frame(&mut resp_reader)
        .unwrap()
        .is_none());
}

#[cfg(unix)]
#[test]
fn unix_socket_server_client_roundtrip() {
    use projgit_virtiofs::server::{exchange_frame, spawn_unix_socket_server};
    use std::os::unix::net::UnixStream;

    let repo = TestRepo::create();
    let (fs, _) = repo.build_virtiofs();
    let fs = Arc::new(fs);

    let sock_path = repo._dir.path().join("virtiofs.sock");
    let server = spawn_unix_socket_server(&sock_path, Arc::clone(&fs)).expect("spawn unix server");

    let mut client = UnixStream::connect(server.socket_path()).expect("connect unix socket");

    // 1. FUSE_INIT over Unix domain socket
    let init_resp = exchange_frame(
        &mut client,
        &build_init_request(1, FUSE_KERNEL_VERSION, FUSE_KERNEL_MINOR_VERSION, 0),
    )
    .unwrap();
    let init_hdr = FuseOutHeader::decode(&init_resp).unwrap();
    assert_eq!(init_hdr.error, 0);

    // 2. FUSE_LOOKUP `README.md`
    let lookup_resp = exchange_frame(
        &mut client,
        &build_lookup_request(2, ROOT_INODE, b"README.md"),
    )
    .unwrap();
    let lookup_hdr = FuseOutHeader::decode(&lookup_resp).unwrap();
    assert_eq!(lookup_hdr.error, 0);
    let entry = FuseEntryOut::decode(&lookup_resp[FUSE_OUT_HEADER_LEN..]).unwrap();

    // 3. FUSE_SETUPMAPPING over Unix domain socket
    let moffset = 0x0200_0000;
    let map_resp = exchange_frame(
        &mut client,
        &build_setupmapping_request(
            3,
            entry.nodeid,
            0,
            0,
            4096,
            FUSE_SETUPMAPPING_FLAG_READ,
            moffset,
        ),
    )
    .unwrap();
    let map_hdr = FuseOutHeader::decode(&map_resp).unwrap();
    assert_eq!(map_hdr.error, 0);

    let data = fs
        .dax_mapper()
        .read_window(moffset, entry.attr.size as usize)
        .unwrap();
    assert_eq!(
        data,
        b"# projgit-virtiofs\nZero-copy DAX projection test payload.\n"
    );

    drop(client);
    server.shutdown().expect("graceful server shutdown");
}
