//! End-to-end Linux integration tests for the `rust-vmm` `vhost-user-fs`
//! shared-memory virtqueue and `SHMEM_MAP` DAX transport (`crates/projgit-virtiofs/src/vhost_user.rs`).
//!
//! Spawns a real [`VhostUserDaemon`] via [`spawn_vhost_user_daemon`] backed by
//! [`ProjectionFsProvider`] + [`dotgit::a1_plus_isolated_overlay`], connects a
//! real [`vhost::vhost_user::Frontend`] (VMM master), maps a `memfd` shared
//! memory region with split virtqueue rings (`desc`, `avail`, `used`), attaches
//! a `FrontendReqHandler` over `VHOST_USER_SET_BACKEND_REQ_FD` to intercept
//! `SHMEM_MAP` (`SCM_RIGHTS` fd passing) and `SHMEM_UNMAP`, and drives FUSE +
//! DAX requests end-to-end through the shared-memory virtqueue.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::memfd::{memfd_create, MFdFlags};
use parking_lot::Mutex;
use projgit_core::{
    dotgit, FsProvider, HydratingObjectStore, NoopFetcher, ObjectStore, Projection,
    ProjectionFsProvider, ROOT_INODE,
};
use projgit_virtiofs::dax::DEFAULT_DAX_WINDOW_SIZE;
use projgit_virtiofs::vhost_user::{
    spawn_vhost_user_daemon, VhostUserFsBackend, VirtioFsConfig, DEFAULT_MAX_QUEUE_SIZE,
    HIPRIO_QUEUE_INDEX, NUM_QUEUES, REQ_QUEUE_INDEX, VIRTIO_FS_CONFIG_SIZE,
};
use projgit_virtiofs::wire::{
    errno, FuseEntryOut, FuseInHeader, FuseInitIn, FuseInitOut, FuseOpcode, FuseOutHeader,
    FuseReadIn, FuseRemoveMappingIn, FuseRemoveMappingOne, FuseSetupMappingIn,
    FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_MAP_ALIGNMENT, FUSE_OUT_HEADER_LEN,
};
use vhost::vhost_user::message::{
    VhostUserConfigFlags, VhostUserMMap, VhostUserProtocolFeatures, VhostUserVirtioFeatures,
};
use vhost::vhost_user::{
    Frontend, FrontendReqHandler, HandlerResult, VhostUserFrontend, VhostUserFrontendReqHandlerMut,
};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::virtio_ring::{
    VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
};
use vm_memory::{Bytes, FileOffset, GuestAddress, GuestMemoryMmap, GuestRegionMmap};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

const GUEST_MEM_SIZE: usize = 2 * 1024 * 1024;
const QUEUE_SIZE: u16 = 16;

// Queue 0 (hiprio) ring addresses in guest physical memory.
const Q0_DESC_GPA: u64 = 0x1000;
const Q0_AVAIL_GPA: u64 = 0x2000;
const Q0_USED_GPA: u64 = 0x3000;

// Queue 1 (request) ring addresses in guest physical memory.
const Q1_DESC_GPA: u64 = 0x4000;
const Q1_AVAIL_GPA: u64 = 0x5000;
const Q1_USED_GPA: u64 = 0x6000;

// Request / response payload buffers in guest physical memory.
const REQ_BUF_GPA: u64 = 0x10_000;
const RESP_BUF_GPA: u64 = 0x20_000;
const RESP_BUF_CAP: u32 = 64 * 1024;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "projgit-vhost-linux-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Recorded `SHMEM_MAP` request intercepted by the simulated VMM [`FrontendReqHandler`].
#[derive(Debug)]
struct RecordedShmemMap {
    shmid: u8,
    fd_offset: u64,
    shm_offset: u64,
    len: u64,
    flags: u64,
    /// Owned file handle opened from `/proc/self/fd/<raw_fd>` when the `SCM_RIGHTS`
    /// file descriptor was delivered by the backend.
    file: File,
}

impl RecordedShmemMap {
    fn read_mapped_bytes(&self, offset_in_mapping: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let file_off = self.fd_offset + offset_in_mapping;
        let mut read_total = 0usize;
        while read_total < len {
            let n = self
                .file
                .read_at(&mut buf[read_total..], file_off + read_total as u64)
                .expect("read_at on SCM_RIGHTS file");
            if n == 0 {
                break;
            }
            read_total += n;
        }
        buf.truncate(read_total);
        buf
    }
}

/// Simulated VMM `VhostUserFrontendReqHandlerMut` that records `SHMEM_MAP` and
/// `SHMEM_UNMAP` requests sent by `VhostShmemDaxMapper` over the backend channel.
#[derive(Default)]
struct VmmShmemHandlerState {
    active_maps: BTreeMap<u64, RecordedShmemMap>,
    unmapped_ranges: Vec<(u64, u64)>,
}

struct TestVmmReqHandler {
    state: Arc<Mutex<VmmShmemHandlerState>>,
}

impl VhostUserFrontendReqHandlerMut for TestVmmReqHandler {
    fn shmem_map(&mut self, req: &VhostUserMMap, fd: &dyn AsRawFd) -> HandlerResult<u64> {
        // Safely open a duplicate `File` description via `/proc/self/fd/<fd>` without `unsafe`.
        let proc_fd_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        let file = File::open(&proc_fd_path)?;
        self.state.lock().active_maps.insert(
            req.shm_offset,
            RecordedShmemMap {
                shmid: req.shmid,
                fd_offset: req.fd_offset,
                shm_offset: req.shm_offset,
                len: req.len,
                flags: req.flags,
                file,
            },
        );
        Ok(0)
    }

    fn shmem_unmap(&mut self, req: &VhostUserMMap) -> HandlerResult<u64> {
        let shm_offset = req.shm_offset;
        let len = req.len;
        let mut guard = self.state.lock();
        guard.active_maps.remove(&shm_offset);
        guard.unmapped_ranges.push((shm_offset, len));
        Ok(0)
    }
}

fn create_isolated_fixture() -> (
    TempDir,
    Arc<ProjectionFsProvider<NoopFetcher>>,
    gix::ObjectId,
) {
    let tmp = TempDir::new("repo");
    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&tmp.path)
            .env("GIT_AUTHOR_NAME", "projgit-test")
            .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
            .env("GIT_COMMITTER_NAME", "projgit-test")
            .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };

    run_git(&["init", "-q", "-b", "main"]);
    std::fs::write(
        tmp.path.join("README.md"),
        b"# projgit vhost-user-fs linux test\n",
    )
    .expect("write README.md");
    run_git(&["add", "-A"]);
    run_git(&["commit", "-q", "-m", "initial commit"]);
    run_git(&["repack", "-a", "-d", "-q"]);
    run_git(&["commit-graph", "write", "--reachable"]);

    let git_dir = tmp.path.join(".git");
    let store = Arc::new(ObjectStore::open(&git_dir).expect("open ObjectStore"));
    let commit_oid = store.resolve_ref("HEAD").expect("resolve HEAD");
    let overlay =
        dotgit::a1_plus_isolated_overlay(&store, commit_oid).expect("a1_plus_isolated_overlay");
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(Projection::Commit(commit_oid), hydrating, overlay, 1)
            .expect("new ProjectionFsProvider"),
    );

    (tmp, provider, commit_oid)
}

fn build_request(opcode: FuseOpcode, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
    let hdr = FuseInHeader {
        len: (projgit_virtiofs::wire::FUSE_IN_HEADER_LEN + payload.len()) as u32,
        opcode: opcode as u32,
        unique,
        nodeid,
        uid: 1000,
        gid: 1000,
        pid: 42,
        padding: 0,
    };
    let mut buf = Vec::with_capacity(hdr.len as usize);
    buf.extend_from_slice(&hdr.encode());
    buf.extend_from_slice(payload);
    buf
}

/// Write a 16-byte split virtqueue descriptor into guest memory at `desc_table_gpa + index * 16`.
fn write_split_desc(
    mem: &GuestMemoryMmap,
    desc_table_gpa: u64,
    index: u16,
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
) {
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&addr.to_le_bytes());
    raw[8..12].copy_from_slice(&len.to_le_bytes());
    raw[12..14].copy_from_slice(&flags.to_le_bytes());
    raw[14..16].copy_from_slice(&next.to_le_bytes());
    let desc_addr = GuestAddress(desc_table_gpa + (index as u64) * 16);
    mem.write_slice(&raw, desc_addr)
        .expect("write split descriptor");
}

/// Submit a 2-descriptor FUSE request/response chain (`desc[0]` readable, `desc[1]` writable)
/// on queue 1, kick `kick_fd`, wait for `used.idx` to advance, and return the response bytes.
fn submit_fuse_request_over_virtqueue(
    mem: &GuestMemoryMmap,
    kick_fd: &EventFd,
    call_fd: &EventFd,
    avail_idx: &mut u16,
    req_bytes: &[u8],
) -> Vec<u8> {
    // 1. Write request bytes into guest memory at REQ_BUF_GPA and clear RESP_BUF_GPA header.
    mem.write_slice(req_bytes, GuestAddress(REQ_BUF_GPA))
        .expect("write req_bytes to guest memory");
    mem.write_slice(&[0u8; 64], GuestAddress(RESP_BUF_GPA))
        .expect("clear resp header in guest memory");

    // 2. Descriptor 0: readable request buffer chaining to descriptor 1.
    write_split_desc(
        mem,
        Q1_DESC_GPA,
        0,
        REQ_BUF_GPA,
        req_bytes.len() as u32,
        VRING_DESC_F_NEXT as u16,
        1,
    );
    // 3. Descriptor 1: writable response buffer.
    write_split_desc(
        mem,
        Q1_DESC_GPA,
        1,
        RESP_BUF_GPA,
        RESP_BUF_CAP,
        VRING_DESC_F_WRITE as u16,
        0,
    );

    // 4. Publish head index 0 in `avail.ring[avail_idx % QUEUE_SIZE]` and advance `avail.idx`.
    let ring_slot = (*avail_idx % QUEUE_SIZE) as u64;
    let ring_entry_gpa = Q1_AVAIL_GPA + 4 + ring_slot * 2;
    mem.write_slice(&0u16.to_le_bytes(), GuestAddress(ring_entry_gpa))
        .expect("write avail ring entry");

    *avail_idx = avail_idx.wrapping_add(1);
    // Also update `used_event` at the end of the avail ring so `EVENT_IDX` notifications fire.
    let used_event_gpa = Q1_AVAIL_GPA + 4 + (QUEUE_SIZE as u64) * 2;
    mem.write_slice(
        &avail_idx.wrapping_sub(1).to_le_bytes(),
        GuestAddress(used_event_gpa),
    )
    .expect("write used_event");

    mem.store(
        avail_idx.to_le(),
        GuestAddress(Q1_AVAIL_GPA + 2),
        Ordering::Release,
    )
    .expect("store avail.idx");

    // 5. Kick the vring EventFd.
    kick_fd.write(1).expect("kick vring");

    // 6. Wait for `used.idx` to reach `*avail_idx`.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let used_idx = u16::from_le(
            mem.load::<u16>(GuestAddress(Q1_USED_GPA + 2), Ordering::Acquire)
                .expect("load used.idx"),
        );
        if used_idx == *avail_idx {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for vring used.idx (expected {}, got {used_idx})",
            *avail_idx
        );
        thread::sleep(Duration::from_millis(2));
    }
    // Drain call_fd if signaled.
    let _ = call_fd.read();

    // 7. Read `virtq_used_elem` for the completed request.
    let used_slot = (avail_idx.wrapping_sub(1) % QUEUE_SIZE) as u64;
    let used_elem_gpa = Q1_USED_GPA + 4 + used_slot * 8;
    let mut elem_bytes = [0u8; 8];
    mem.read_slice(&mut elem_bytes, GuestAddress(used_elem_gpa))
        .expect("read virtq_used_elem");
    let used_id = u32::from_le_bytes(elem_bytes[0..4].try_into().unwrap());
    let used_len = u32::from_le_bytes(elem_bytes[4..8].try_into().unwrap());
    assert_eq!(used_id, 0, "expected head descriptor 0 in used ring");
    assert!(
        used_len as usize >= FUSE_OUT_HEADER_LEN,
        "expected at least FUSE_OUT_HEADER_LEN bytes written, got {used_len}"
    );

    let mut resp = vec![0u8; used_len as usize];
    mem.read_slice(&mut resp, GuestAddress(RESP_BUF_GPA))
        .expect("read FUSE response from guest memory");
    resp
}

#[test]
fn vhost_user_virtqueue_and_shmem_dax_end_to_end() {
    let (_tmp_repo, provider, _commit_oid) = create_isolated_fixture();
    let sock_dir = TempDir::new("sock");
    let sock_path = sock_dir.path.join("vhost-fs.sock");

    let backend = Arc::new(RwLock::new(
        VhostUserFsBackend::with_config(
            Arc::clone(&provider),
            "projgit-test",
            DEFAULT_DAX_WINDOW_SIZE,
            DEFAULT_MAX_QUEUE_SIZE,
        )
        .expect("create VhostUserFsBackend"),
    ));

    let daemon_handle =
        spawn_vhost_user_daemon(&sock_path, Arc::clone(&backend)).expect("spawn_vhost_user_daemon");

    // 1. Connect the real `rust-vmm` `vhost::vhost_user::Frontend` (VMM master).
    let mut frontend = Frontend::connect(&sock_path, NUM_QUEUES as u64).expect("Frontend::connect");

    // 2. Negotiate VirtIO features + vhost-user protocol features.
    let avail_features = frontend.get_features().expect("get_features");
    let expected_virtio_features = (1u64 << VIRTIO_F_VERSION_1)
        | (1u64 << VIRTIO_RING_F_INDIRECT_DESC)
        | (1u64 << VIRTIO_RING_F_EVENT_IDX)
        | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
    assert_eq!(
        avail_features & expected_virtio_features,
        expected_virtio_features
    );
    frontend
        .set_features(expected_virtio_features)
        .expect("set_features");

    let proto_features = frontend
        .get_protocol_features()
        .expect("get_protocol_features");
    let required_proto = VhostUserProtocolFeatures::MQ
        | VhostUserProtocolFeatures::REPLY_ACK
        | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
        | VhostUserProtocolFeatures::BACKEND_REQ
        | VhostUserProtocolFeatures::BACKEND_SEND_FD
        | VhostUserProtocolFeatures::SHMEM
        | VhostUserProtocolFeatures::CONFIG;
    assert!(
        proto_features.contains(required_proto),
        "missing expected protocol features: {proto_features:?}"
    );
    frontend
        .set_protocol_features(required_proto)
        .expect("set_protocol_features");

    frontend.set_owner().expect("set_owner");

    // 3. Verify `get_config` (36-byte tag + num_request_queues == 1) and `get_shmem_config`.
    let (_, raw_cfg) = frontend
        .get_config(
            0,
            VIRTIO_FS_CONFIG_SIZE as u32,
            VhostUserConfigFlags::WRITABLE,
            &[0u8; VIRTIO_FS_CONFIG_SIZE],
        )
        .expect("get_config");
    let fs_cfg = VirtioFsConfig::decode(&raw_cfg).expect("decode VirtioFsConfig");
    assert_eq!(fs_cfg.tag_str(), "projgit-test");
    assert_eq!(fs_cfg.num_request_queues, 1);

    let shmem_cfg = frontend.get_shmem_config().expect("get_shmem_config");
    assert_eq!(shmem_cfg.nregions, 1);
    assert_eq!(shmem_cfg.memory_sizes[0], DEFAULT_DAX_WINDOW_SIZE);

    // 4. Set up the `Backend` <-> `FrontendReqHandler` socket pair via `set_backend_request_fd`
    //    so `SHMEM_MAP` (with `SCM_RIGHTS` fd passing) and `SHMEM_UNMAP` are handled by our
    //    simulated VMM thread.
    let vmm_shmem_state = Arc::new(Mutex::new(VmmShmemHandlerState::default()));
    let mut req_handler = FrontendReqHandler::new(Arc::new(StdMutex::new(TestVmmReqHandler {
        state: Arc::clone(&vmm_shmem_state),
    })))
    .expect("FrontendReqHandler::new");
    req_handler.set_reply_ack_flag(true);
    frontend
        .set_backend_request_fd(&req_handler.get_tx_raw_fd())
        .expect("set_backend_request_fd");

    // Our test issues 2 FUSE_SETUPMAPPING requests (`SHMEM_MAP`) and
    // 1 FUSE_REMOVEMAPPING request with 2 entries (`2 x SHMEM_UNMAP`) = 4 backend requests.
    let req_handler_thread = thread::spawn(move || {
        for _ in 0..4 {
            req_handler
                .handle_request()
                .expect("FrontendReqHandler::handle_request");
        }
    });

    // 5. Allocate a Linux `memfd`, map it as `GuestMemoryMmap`, and register it via `set_mem_table`.
    let memfd = memfd_create("projgit_guest_mem", MFdFlags::MFD_CLOEXEC).expect("memfd_create");
    let mem_file = File::from(memfd);
    mem_file
        .set_len(GUEST_MEM_SIZE as u64)
        .expect("set_len on memfd");

    let guest_region = GuestRegionMmap::<()>::from_range(
        GuestAddress(0),
        GUEST_MEM_SIZE,
        Some(FileOffset::new(mem_file, 0)),
    )
    .expect("GuestRegionMmap::from_range");
    let region_info =
        VhostUserMemoryRegionInfo::from_guest_region(&guest_region).expect("from_guest_region");
    let vmm_base_va = region_info.userspace_addr;
    let guest_mem = GuestMemoryMmap::<()>::from_regions(vec![guest_region]).expect("from_regions");

    frontend
        .set_mem_table(&[region_info])
        .expect("set_mem_table");

    // 6. Configure split virtqueues 0 (hiprio) and 1 (request) and register kick/call EventFds.
    let setup_vring = |frontend: &mut Frontend,
                       queue_index: usize,
                       desc_gpa: u64,
                       avail_gpa: u64,
                       used_gpa: u64|
     -> (EventFd, EventFd) {
        frontend
            .set_vring_num(queue_index, QUEUE_SIZE)
            .expect("set_vring_num");
        frontend
            .set_vring_base(queue_index, 0)
            .expect("set_vring_base");
        let cfg_data = VringConfigData {
            queue_max_size: DEFAULT_MAX_QUEUE_SIZE as u16,
            queue_size: QUEUE_SIZE,
            flags: 0,
            desc_table_addr: vmm_base_va + desc_gpa,
            used_ring_addr: vmm_base_va + used_gpa,
            avail_ring_addr: vmm_base_va + avail_gpa,
            log_addr: None,
        };
        frontend
            .set_vring_addr(queue_index, &cfg_data)
            .expect("set_vring_addr");

        let kick_fd = EventFd::new(EFD_NONBLOCK).expect("kick EventFd");
        let call_fd = EventFd::new(EFD_NONBLOCK).expect("call EventFd");
        frontend
            .set_vring_kick(queue_index, &kick_fd)
            .expect("set_vring_kick");
        frontend
            .set_vring_call(queue_index, &call_fd)
            .expect("set_vring_call");
        frontend
            .set_vring_enable(queue_index, true)
            .expect("set_vring_enable");
        (kick_fd, call_fd)
    };

    let (_q0_kick, _q0_call) = setup_vring(
        &mut frontend,
        HIPRIO_QUEUE_INDEX,
        Q0_DESC_GPA,
        Q0_AVAIL_GPA,
        Q0_USED_GPA,
    );
    let (q1_kick, q1_call) = setup_vring(
        &mut frontend,
        REQ_QUEUE_INDEX,
        Q1_DESC_GPA,
        Q1_AVAIL_GPA,
        Q1_USED_GPA,
    );

    let mut q1_avail_idx = 0u16;

    // 7. Drive FUSE_INIT over the shared-memory virtqueue.
    let init_req = build_request(
        FuseOpcode::Init,
        1,
        ROOT_INODE,
        &FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 65536,
            flags: FUSE_MAP_ALIGNMENT,
        }
        .encode(),
    );
    let init_resp = submit_fuse_request_over_virtqueue(
        &guest_mem,
        &q1_kick,
        &q1_call,
        &mut q1_avail_idx,
        &init_req,
    );
    let init_hdr = FuseOutHeader::decode(&init_resp).expect("decode FuseOutHeader");
    assert_eq!(init_hdr.error, 0);
    let init_out = FuseInitOut::decode(&init_resp[FUSE_OUT_HEADER_LEN..]).expect("decode InitOut");
    assert_eq!(init_out.major, FUSE_KERNEL_VERSION);
    assert_ne!(init_out.flags & FUSE_MAP_ALIGNMENT, 0);

    // 8. Drive FUSE_LOOKUP + FUSE_READ for `README.md` over the shared-memory virtqueue.
    let lookup_readme = submit_fuse_request_over_virtqueue(
        &guest_mem,
        &q1_kick,
        &q1_call,
        &mut q1_avail_idx,
        &build_request(FuseOpcode::Lookup, 2, ROOT_INODE, b"README.md\0"),
    );
    let lookup_hdr = FuseOutHeader::decode(&lookup_readme).unwrap();
    assert_eq!(lookup_hdr.error, 0);
    let readme_entry = FuseEntryOut::decode(&lookup_readme[FUSE_OUT_HEADER_LEN..]).unwrap();
    let expected_readme = b"# projgit vhost-user-fs linux test\n";
    assert_eq!(readme_entry.attr.size, expected_readme.len() as u64);

    let read_readme = submit_fuse_request_over_virtqueue(
        &guest_mem,
        &q1_kick,
        &q1_call,
        &mut q1_avail_idx,
        &build_request(
            FuseOpcode::Read,
            3,
            readme_entry.nodeid,
            &FuseReadIn {
                fh: 0,
                offset: 0,
                size: 4096,
                read_flags: 0,
                lock_owner: 0,
                flags: 0,
                padding: 0,
            }
            .encode(),
        ),
    );
    let read_hdr = FuseOutHeader::decode(&read_readme).unwrap();
    assert_eq!(read_hdr.error, 0);
    assert_eq!(&read_readme[FUSE_OUT_HEADER_LEN..], expected_readme);

    // 9. Drive FUSE_SETUPMAPPING on `README.md` and verify `SHMEM_MAP` is delivered
    //    over the backend channel with a valid open `File` descriptor via `SCM_RIGHTS`!
    let readme_moffset = 0x10_0000u64;
    let setup_readme = submit_fuse_request_over_virtqueue(
        &guest_mem,
        &q1_kick,
        &q1_call,
        &mut q1_avail_idx,
        &build_request(
            FuseOpcode::SetupMapping,
            4,
            readme_entry.nodeid,
            &FuseSetupMappingIn {
                fh: 0,
                foffset: 0,
                len: expected_readme.len() as u64,
                flags: 0,
                moffset: readme_moffset,
            }
            .encode(),
        ),
    );
    let setup_hdr = FuseOutHeader::decode(&setup_readme).unwrap();
    assert_eq!(
        setup_hdr.error, 0,
        "FUSE_SETUPMAPPING on README.md must succeed"
    );

    {
        let guard = vmm_shmem_state.lock();
        let mapped = guard
            .active_maps
            .get(&readme_moffset)
            .expect("SHMEM_MAP must be recorded for README.md at moffset");
        assert_eq!(mapped.shmid, 0);
        assert_eq!(mapped.shm_offset, readme_moffset);
        assert_eq!(mapped.fd_offset, 0);
        assert_eq!(mapped.len, expected_readme.len() as u64);
        assert_eq!(mapped.flags, 0);
        let bytes = mapped.read_mapped_bytes(0, expected_readme.len());
        assert_eq!(bytes.as_slice(), expected_readme);
    }

    // 10. Drive FUSE_SETUPMAPPING on `.git/objects/pack/pack-*.pack` (isolated `.git/` HostFile)
    //     and verify the host `.pack` file descriptor is passed via `SCM_RIGHTS` (`SHMEM_MAP`).
    let lookup_child = |avail_idx: &mut u16, unique: u64, parent: u64, name: &[u8]| -> u64 {
        let mut payload = name.to_vec();
        payload.push(0);
        let resp = submit_fuse_request_over_virtqueue(
            &guest_mem,
            &q1_kick,
            &q1_call,
            avail_idx,
            &build_request(FuseOpcode::Lookup, unique, parent, &payload),
        );
        let hdr = FuseOutHeader::decode(&resp).unwrap();
        assert_eq!(
            hdr.error,
            0,
            "lookup {:?} failed",
            String::from_utf8_lossy(name)
        );
        FuseEntryOut::decode(&resp[FUSE_OUT_HEADER_LEN..])
            .unwrap()
            .nodeid
    };

    let dotgit_ino = lookup_child(&mut q1_avail_idx, 5, ROOT_INODE, b".git");
    let objects_ino = lookup_child(&mut q1_avail_idx, 6, dotgit_ino, b"objects");
    let pack_dir_ino = lookup_child(&mut q1_avail_idx, 7, objects_ino, b"pack");

    let pack_entries = provider.readdir(pack_dir_ino, 0).expect("readdir pack");
    let pack_entry = pack_entries
        .iter()
        .find(|e| e.name.ends_with(b".pack"))
        .expect("find .pack entry");
    let pack_ino = lookup_child(
        &mut q1_avail_idx,
        8,
        pack_dir_ino,
        pack_entry.name.as_slice(),
    );

    let pack_moffset = 0x40_0000u64;
    let setup_pack = submit_fuse_request_over_virtqueue(
        &guest_mem,
        &q1_kick,
        &q1_call,
        &mut q1_avail_idx,
        &build_request(
            FuseOpcode::SetupMapping,
            9,
            pack_ino,
            &FuseSetupMappingIn {
                fh: 0,
                foffset: 0,
                len: 12,
                flags: 0,
                moffset: pack_moffset,
            }
            .encode(),
        ),
    );
    assert_eq!(FuseOutHeader::decode(&setup_pack).unwrap().error, 0);

    {
        let guard = vmm_shmem_state.lock();
        let mapped_pack = guard
            .active_maps
            .get(&pack_moffset)
            .expect("SHMEM_MAP must be recorded for .pack file");
        let header = mapped_pack.read_mapped_bytes(0, 4);
        assert_eq!(
            &header, b"PACK",
            "passed pack fd must start with PACK magic"
        );
    }

    // 11. Drive FUSE_SETUPMAPPING on in-memory synthetic `.git/HEAD` -> verify `-ENOSYS`
    //     fallback without triggering any `SHMEM_MAP`.
    let head_ino = lookup_child(&mut q1_avail_idx, 10, dotgit_ino, b"HEAD");
    let setup_head = submit_fuse_request_over_virtqueue(
        &guest_mem,
        &q1_kick,
        &q1_call,
        &mut q1_avail_idx,
        &build_request(
            FuseOpcode::SetupMapping,
            11,
            head_ino,
            &FuseSetupMappingIn {
                fh: 0,
                foffset: 0,
                len: 41,
                flags: 0,
                moffset: 0x80_0000,
            }
            .encode(),
        ),
    );
    assert_eq!(
        FuseOutHeader::decode(&setup_head).unwrap().error,
        -errno::ENOSYS
    );

    // 12. Drive FUSE_REMOVEMAPPING for both `readme_moffset` and `pack_moffset` ->
    //     verify `SHMEM_UNMAP` is forwarded to the VMM `FrontendReqHandler`.
    let mut remove_payload = Vec::new();
    remove_payload.extend_from_slice(&FuseRemoveMappingIn { count: 2 }.encode());
    remove_payload.extend_from_slice(
        &FuseRemoveMappingOne {
            moffset: readme_moffset,
            len: expected_readme.len() as u64,
        }
        .encode(),
    );
    remove_payload.extend_from_slice(
        &FuseRemoveMappingOne {
            moffset: pack_moffset,
            len: 12,
        }
        .encode(),
    );
    let remove_resp = submit_fuse_request_over_virtqueue(
        &guest_mem,
        &q1_kick,
        &q1_call,
        &mut q1_avail_idx,
        &build_request(FuseOpcode::RemoveMapping, 12, ROOT_INODE, &remove_payload),
    );
    assert_eq!(FuseOutHeader::decode(&remove_resp).unwrap().error, 0);

    {
        let guard = vmm_shmem_state.lock();
        assert!(
            guard.active_maps.is_empty(),
            "all DAX maps must be unmapped"
        );
        assert_eq!(
            guard.unmapped_ranges,
            vec![
                (readme_moffset, expected_readme.len() as u64),
                (pack_moffset, 12),
            ]
        );
    }

    // 13. Clean shutdown: drop `frontend`, join the daemon and VMM handler threads.
    drop(frontend);
    daemon_handle.join().expect("daemon_handle.join");
    let _ = req_handler_thread.join();
}
