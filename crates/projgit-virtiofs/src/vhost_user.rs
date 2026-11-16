//! Linux `rust-vmm` `vhost-user-fs` shared-memory virtqueue and `SHMEM_MAP` DAX backend.
//!
//! This module implements [`vhost_user_backend::VhostUserBackendMut`] on top of
//! [`crate::ProjgitVirtioFs`], allowing `projgit` to attach directly to `qemu` and
//! `cloud-hypervisor` over a `vhost-user` Unix domain socket:
//!
//! * Exposes two split virtqueues: queue `0` (`hiprio` for `FUSE_FORGET` /
//!   `FUSE_BATCH_FORGET`) and queue `1` (`request` queue for FUSE + DAX requests).
//! * Negotiates `VIRTIO_F_VERSION_1`, `VIRTIO_RING_F_INDIRECT_DESC`,
//!   `VIRTIO_RING_F_EVENT_IDX`, and `VhostUserVirtioFeatures::PROTOCOL_FEATURES`,
//!   along with `VhostUserProtocolFeatures::{MQ, REPLY_ACK, CONFIGURE_MEM_SLOTS,
//!   BACKEND_REQ, BACKEND_SEND_FD, SHMEM, CONFIG}`.
//! * Serves the 40-byte `virtio_fs_config` device configuration space (36-byte UTF-8
//!   filesystem tag padded with NULs + little-endian `u32` `num_request_queues = 1`).
//! * Advertises a single DAX shared memory region (`shmid = 0`) via `get_shmem_config`
//!   and forwards `FUSE_SETUPMAPPING` / `FUSE_REMOVEMAPPING` requests over the
//!   `vhost-user` backend request channel (`SHMEM_MAP` with `SCM_RIGHTS` file descriptor
//!   passing and `SHMEM_UNMAP`) via [`VhostShmemDaxMapper`].

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use projgit_core::FsProvider;
use vhost::vhost_user::message::{
    VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures, VhostUserShMemConfig,
    VhostUserVirtioFeatures,
};
use vhost::vhost_user::{Backend, Listener, VhostUserFrontendReqHandler};
use vhost_user_backend::{
    ShutdownHandle, VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT,
};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::virtio_ring::{VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC};
use virtio_queue::QueueT;
use vm_memory::{Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::event::{
    new_event_consumer_and_notifier, EventConsumer, EventFlag, EventNotifier,
};

use crate::dax::{DaxMapper, MemoryDaxWindow, DEFAULT_DAX_WINDOW_SIZE};
use crate::wire::{errno, FUSE_SETUPMAPPING_FLAG_WRITE};
use crate::ProjgitVirtioFs;

/// High-priority virtqueue index (`FUSE_FORGET` / `FUSE_BATCH_FORGET`).
pub const HIPRIO_QUEUE_INDEX: usize = 0;

/// Normal FUSE + DAX request virtqueue index.
pub const REQ_QUEUE_INDEX: usize = 1;

/// Total number of virtqueues exposed by `VhostUserFsBackend` (`hiprio` + 1 request queue).
pub const NUM_QUEUES: usize = 2;

/// Default maximum descriptor count per virtqueue.
pub const DEFAULT_MAX_QUEUE_SIZE: usize = 256;

/// Default `virtio-fs` filesystem tag (`mount -t virtiofs <tag> <mountpoint>`).
pub const DEFAULT_TAG: &str = "projgit";

/// Length of the fixed-size `tag` field in `struct virtio_fs_config` (VirtIO spec §5.11.4).
pub const VIRTIO_FS_TAG_LEN: usize = 36;

/// Total size in bytes of `struct virtio_fs_config` (`36` bytes `tag` + `4` bytes `num_request_queues`).
pub const VIRTIO_FS_CONFIG_SIZE: usize = 40;

type BackendReqHook = Arc<dyn Fn(Backend) + Send + Sync>;

/// Encoded `struct virtio_fs_config` exposed via `VHOST_USER_GET_CONFIG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtioFsConfig {
    /// UTF-8 filesystem tag padded with NUL bytes (36 bytes).
    pub tag: [u8; VIRTIO_FS_TAG_LEN],
    /// Number of request virtqueues (excluding the hiprio queue).
    pub num_request_queues: u32,
}

impl VirtioFsConfig {
    /// Construct a `virtio_fs_config` from a tag string and request queue count.
    ///
    /// Tags longer than [`VIRTIO_FS_TAG_LEN`] bytes are truncated to 36 bytes.
    pub fn new(tag: &str, num_request_queues: u32) -> Self {
        let mut raw_tag = [0u8; VIRTIO_FS_TAG_LEN];
        let bytes = tag.as_bytes();
        let copy_len = bytes.len().min(VIRTIO_FS_TAG_LEN);
        raw_tag[..copy_len].copy_from_slice(&bytes[..copy_len]);
        Self {
            tag: raw_tag,
            num_request_queues,
        }
    }

    /// Return the trimmed UTF-8 tag string (up to the first NUL byte).
    pub fn tag_str(&self) -> &str {
        let end = self
            .tag
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(VIRTIO_FS_TAG_LEN);
        std::str::from_utf8(&self.tag[..end]).unwrap_or("")
    }

    /// Encode `struct virtio_fs_config` into its 40-byte little-endian wire layout.
    pub fn encode(&self) -> [u8; VIRTIO_FS_CONFIG_SIZE] {
        let mut buf = [0u8; VIRTIO_FS_CONFIG_SIZE];
        buf[..VIRTIO_FS_TAG_LEN].copy_from_slice(&self.tag);
        buf[VIRTIO_FS_TAG_LEN..VIRTIO_FS_CONFIG_SIZE]
            .copy_from_slice(&self.num_request_queues.to_le_bytes());
        buf
    }

    /// Decode a 40-byte `virtio_fs_config` slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < VIRTIO_FS_CONFIG_SIZE {
            return None;
        }
        let mut tag = [0u8; VIRTIO_FS_TAG_LEN];
        tag.copy_from_slice(&bytes[..VIRTIO_FS_TAG_LEN]);
        let num_request_queues = u32::from_le_bytes(
            bytes[VIRTIO_FS_TAG_LEN..VIRTIO_FS_CONFIG_SIZE]
                .try_into()
                .ok()?,
        );
        Some(Self {
            tag,
            num_request_queues,
        })
    }
}

/// [`DaxMapper`] implementation that pairs [`MemoryDaxWindow`] with the `vhost-user`
/// backend request channel (`VHOST_USER_BACKEND_SHMEM_MAP` / `VHOST_USER_BACKEND_SHMEM_UNMAP`).
///
/// When a VMM attaches a [`Backend`] socket via `VHOST_USER_SET_BACKEND_REQ_FD`,
/// `FUSE_SETUPMAPPING` both records the mapping in [`MemoryDaxWindow`] and sends
/// `SHMEM_MAP` (with the uncompressed blob or packfile's `RawFd` over `SCM_RIGHTS`)
/// to the VMM frontend so the VMM `mmap`s the host file directly into the guest's
/// DAX shared-memory window.
pub struct VhostShmemDaxMapper {
    window: MemoryDaxWindow,
    backend: Mutex<Option<Backend>>,
}

impl std::fmt::Debug for VhostShmemDaxMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VhostShmemDaxMapper")
            .field("window", &self.window)
            .field("has_backend", &self.has_backend())
            .finish()
    }
}

impl Default for VhostShmemDaxMapper {
    fn default() -> Self {
        Self::new(DEFAULT_DAX_WINDOW_SIZE)
    }
}

impl VhostShmemDaxMapper {
    /// Create a new [`VhostShmemDaxMapper`] with the given DAX window size in bytes.
    pub fn new(window_size: u64) -> Self {
        Self {
            window: MemoryDaxWindow::new(window_size),
            backend: Mutex::new(None),
        }
    }

    /// Borrow the underlying [`MemoryDaxWindow`] tracker.
    pub fn window(&self) -> &MemoryDaxWindow {
        &self.window
    }

    /// Attach the `vhost-user` [`Backend`] request channel provided by the VMM via
    /// `VHOST_USER_SET_BACKEND_REQ_FD`.
    pub fn set_backend(&self, backend: Backend) {
        backend.set_shmem_flag(true);
        *self.backend.lock() = Some(backend);
    }

    /// Detach any previously configured [`Backend`] request channel.
    pub fn clear_backend(&self) {
        *self.backend.lock() = None;
    }

    /// Whether a VMM [`Backend`] request channel is currently attached.
    pub fn has_backend(&self) -> bool {
        self.backend.lock().is_some()
    }
}

impl DaxMapper for VhostShmemDaxMapper {
    fn setup_mapping(
        &self,
        ino: u64,
        file: Arc<File>,
        foffset: u64,
        len: u64,
        moffset: u64,
        flags: u64,
    ) -> Result<(), i32> {
        self.window
            .setup_mapping(ino, Arc::clone(&file), foffset, len, moffset, flags)?;

        let backend_opt = self.backend.lock().clone();
        if let Some(backend) = backend_opt {
            let mmap_flags = if (flags & FUSE_SETUPMAPPING_FLAG_WRITE) != 0 {
                VhostUserMMapFlags::WRITABLE
            } else {
                VhostUserMMapFlags::empty()
            };
            let req = VhostUserMMap {
                shmid: 0,
                padding: [0u8; 7],
                fd_offset: foffset,
                shm_offset: moffset,
                len,
                flags: mmap_flags.bits(),
            };
            if backend.shmem_map(&req, file.as_ref()).is_err() {
                let _ = self.window.remove_mapping(moffset, len);
                return Err(errno::EIO);
            }
        }
        Ok(())
    }

    fn remove_mapping(&self, moffset: u64, len: u64) -> Result<(), i32> {
        self.window.remove_mapping(moffset, len)?;

        let backend_opt = self.backend.lock().clone();
        if let Some(backend) = backend_opt {
            let req = VhostUserMMap {
                shmid: 0,
                padding: [0u8; 7],
                fd_offset: 0,
                shm_offset: moffset,
                len,
                flags: 0,
            };
            if backend.shmem_unmap(&req).is_err() {
                return Err(errno::EIO);
            }
        }
        Ok(())
    }
}

/// `rust-vmm` `vhost-user-fs` backend wrapping [`ProjgitVirtioFs`].
pub struct VhostUserFsBackend<F: FsProvider, M: DaxMapper = VhostShmemDaxMapper> {
    fs: Arc<ProjgitVirtioFs<F, M>>,
    tag: String,
    max_queue_size: usize,
    dax_window_size: u64,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    acked_features: u64,
    event_idx: bool,
    backend_req: Option<Backend>,
    on_set_backend_req: Option<BackendReqHook>,
    exit_consumer: EventConsumer,
    exit_notifier: EventNotifier,
}

impl<F: FsProvider, M: DaxMapper> std::fmt::Debug for VhostUserFsBackend<F, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VhostUserFsBackend")
            .field("tag", &self.tag)
            .field("max_queue_size", &self.max_queue_size)
            .field("dax_window_size", &self.dax_window_size)
            .field("acked_features", &self.acked_features)
            .field("event_idx", &self.event_idx)
            .field("has_mem", &self.mem.is_some())
            .field("has_backend_req", &self.backend_req.is_some())
            .finish()
    }
}

impl<F: FsProvider> VhostUserFsBackend<F, VhostShmemDaxMapper> {
    /// Construct a new [`VhostUserFsBackend`] with default tag (`"projgit"`),
    /// default DAX window size (`8 GiB`), and default max queue size (`256`).
    pub fn new(provider: F) -> io::Result<Self> {
        Self::with_config(
            provider,
            DEFAULT_TAG,
            DEFAULT_DAX_WINDOW_SIZE,
            DEFAULT_MAX_QUEUE_SIZE,
        )
    }

    /// Construct a new [`VhostUserFsBackend`] wired to a [`VhostShmemDaxMapper`]
    /// that automatically receives the VMM's [`Backend`] request channel when
    /// `VHOST_USER_SET_BACKEND_REQ_FD` is negotiated.
    pub fn with_config(
        provider: F,
        tag: impl Into<String>,
        dax_window_size: u64,
        max_queue_size: usize,
    ) -> io::Result<Self> {
        let mapper = Arc::new(VhostShmemDaxMapper::new(dax_window_size));
        let fs = Arc::new(ProjgitVirtioFs::with_mapper(provider, Arc::clone(&mapper)));
        let hook_mapper = Arc::clone(&mapper);
        let mut backend = Self::with_engine(fs, tag, dax_window_size, max_queue_size)?;
        backend.on_set_backend_req = Some(Arc::new(move |chan: Backend| {
            hook_mapper.set_backend(chan);
        }));
        Ok(backend)
    }
}

impl<F: FsProvider, M: DaxMapper> VhostUserFsBackend<F, M> {
    /// Construct a [`VhostUserFsBackend`] from an existing [`ProjgitVirtioFs`] engine.
    pub fn with_engine(
        fs: Arc<ProjgitVirtioFs<F, M>>,
        tag: impl Into<String>,
        dax_window_size: u64,
        max_queue_size: usize,
    ) -> io::Result<Self> {
        let (exit_consumer, exit_notifier) = new_event_consumer_and_notifier(EventFlag::NONBLOCK)?;
        Ok(Self {
            fs,
            tag: tag.into(),
            max_queue_size,
            dax_window_size,
            mem: None,
            acked_features: 0,
            event_idx: false,
            backend_req: None,
            on_set_backend_req: None,
            exit_consumer,
            exit_notifier,
        })
    }

    /// Borrow the underlying [`ProjgitVirtioFs`] FUSE + DAX wire engine.
    pub fn fs(&self) -> &Arc<ProjgitVirtioFs<F, M>> {
        &self.fs
    }

    /// Configured `virtio-fs` filesystem tag.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Configured DAX shared-memory window size in bytes.
    pub fn dax_window_size(&self) -> u64 {
        self.dax_window_size
    }

    /// Clone the [`EventNotifier`] used to stop the `vring` epoll worker thread.
    pub fn clone_exit_notifier(&self) -> io::Result<EventNotifier> {
        self.exit_notifier.try_clone()
    }

    /// Signal the `vring` epoll worker thread to exit cleanly.
    pub fn send_exit_event(&self) {
        let _ = self.exit_notifier.notify();
    }

    fn process_queue(
        &self,
        vring: &VringRwLock,
        mem_atomic: &GuestMemoryAtomic<GuestMemoryMmap>,
    ) -> io::Result<bool> {
        let mut used_any = false;
        if self.event_idx {
            loop {
                vring.disable_notification().map_err(io::Error::other)?;
                while self.process_one_chain(vring, mem_atomic)? {
                    used_any = true;
                }
                if !vring.enable_notification().map_err(io::Error::other)? {
                    break;
                }
            }
        } else {
            while self.process_one_chain(vring, mem_atomic)? {
                used_any = true;
            }
        }

        if used_any {
            let _ = vring.needs_notification();
            vring.signal_used_queue()?;
        }
        Ok(used_any)
    }

    fn process_one_chain(
        &self,
        vring: &VringRwLock,
        mem_atomic: &GuestMemoryAtomic<GuestMemoryMmap>,
    ) -> io::Result<bool> {
        let mem_guard = mem_atomic.memory();
        let chain_opt = vring
            .get_mut()
            .get_queue_mut()
            .pop_descriptor_chain(mem_guard.clone());
        let Some(chain) = chain_opt else {
            return Ok(false);
        };

        let head_index = chain.head_index();
        let mut req_buf = Vec::new();
        let mut writable_descs = Vec::new();

        for desc in chain {
            if desc.is_write_only() {
                writable_descs.push((desc.addr(), desc.len() as usize));
            } else {
                let len = desc.len() as usize;
                let start = req_buf.len();
                req_buf.resize(start + len, 0);
                mem_guard
                    .read_slice(&mut req_buf[start..], desc.addr())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            }
        }

        let mut bytes_written = 0usize;
        if !req_buf.is_empty() {
            let resp_buf = self.fs.dispatch(&req_buf);
            if !resp_buf.is_empty() {
                for (addr, len) in writable_descs {
                    if bytes_written >= resp_buf.len() {
                        break;
                    }
                    let chunk_len = (resp_buf.len() - bytes_written).min(len);
                    mem_guard
                        .write_slice(&resp_buf[bytes_written..bytes_written + chunk_len], addr)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                    bytes_written += chunk_len;
                }
            }
        }

        vring
            .add_used(head_index, bytes_written as u32)
            .map_err(io::Error::other)?;
        Ok(true)
    }
}

impl<F, M> VhostUserBackendMut for VhostUserFsBackend<F, M>
where
    F: FsProvider + 'static,
    M: DaxMapper + 'static,
{
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> usize {
        self.max_queue_size
    }

    fn features(&self) -> u64 {
        (1u64 << VIRTIO_F_VERSION_1)
            | (1u64 << VIRTIO_RING_F_INDIRECT_DESC)
            | (1u64 << VIRTIO_RING_F_EVENT_IDX)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn acked_features(&mut self, features: u64) {
        self.acked_features = features;
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::REPLY_ACK
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
            | VhostUserProtocolFeatures::BACKEND_REQ
            | VhostUserProtocolFeatures::BACKEND_SEND_FD
            | VhostUserProtocolFeatures::SHMEM
            | VhostUserProtocolFeatures::CONFIG
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        let cfg = VirtioFsConfig::new(&self.tag, 1).encode();
        let start = (offset as usize).min(cfg.len());
        let end = start.saturating_add(size as usize).min(cfg.len());
        let mut out = vec![0u8; size as usize];
        let copy_len = end.saturating_sub(start);
        out[..copy_len].copy_from_slice(&cfg[start..end]);
        out
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> io::Result<()> {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: EventSet,
        vrings: &[VringRwLock],
        _thread_id: usize,
    ) -> io::Result<()> {
        if evset != EventSet::IN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected epoll event set",
            ));
        }
        let vring = vrings.get(device_event as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown vring device_event {device_event}"),
            )
        })?;
        let Some(ref mem_atomic) = self.mem else {
            return Ok(());
        };
        let mem_cloned = mem_atomic.clone();
        self.process_queue(vring, &mem_cloned)?;
        Ok(())
    }

    fn set_backend_req_fd(&mut self, backend: Backend) {
        if let Some(ref hook) = self.on_set_backend_req {
            hook(backend.clone());
        }
        self.backend_req = Some(backend);
    }

    fn get_shmem_config(&self) -> io::Result<VhostUserShMemConfig> {
        Ok(VhostUserShMemConfig::new(1, &[self.dax_window_size]))
    }

    fn queues_per_thread(&self) -> Vec<u64> {
        vec![0b11]
    }

    fn exit_event(&self, thread_index: usize) -> Option<(EventConsumer, EventNotifier)> {
        if thread_index != 0 {
            return None;
        }
        let consumer = self.exit_consumer.try_clone().ok()?;
        let notifier = self.exit_notifier.try_clone().ok()?;
        Some((consumer, notifier))
    }
}

/// Handle to a running background `VhostUserDaemon` spawned by [`spawn_vhost_user_daemon`].
pub struct VhostUserDaemonHandle<F: FsProvider, M: DaxMapper = VhostShmemDaxMapper> {
    socket_path: PathBuf,
    backend: Arc<RwLock<VhostUserFsBackend<F, M>>>,
    exit_notifier: EventNotifier,
    shutdown_handle: Arc<Mutex<Option<ShutdownHandle>>>,
    accepted_flag: Arc<AtomicBool>,
    shutdown_flag: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<io::Result<()>>>,
}

impl<F: FsProvider, M: DaxMapper> std::fmt::Debug for VhostUserDaemonHandle<F, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VhostUserDaemonHandle")
            .field("socket_path", &self.socket_path)
            .finish()
    }
}

impl<F: FsProvider, M: DaxMapper> VhostUserDaemonHandle<F, M> {
    /// Path to the bound Unix domain socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Shared reference to the underlying [`VhostUserFsBackend`].
    pub fn backend(&self) -> &Arc<RwLock<VhostUserFsBackend<F, M>>> {
        &self.backend
    }

    /// Signal the `vhost-user` daemon and its `vring` epoll worker thread to shut down.
    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(ref handle) = *self.shutdown_handle.lock() {
            handle.shutdown();
        }
        if !self.accepted_flag.load(Ordering::SeqCst) {
            let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        }
        let _ = self.exit_notifier.notify();
    }

    /// Shut down the daemon (if still running) and wait for its server thread to exit.
    pub fn join(mut self) -> io::Result<()> {
        self.shutdown();
        if let Some(handle) = self.join_handle.take() {
            match handle.join() {
                Ok(res) => res,
                Err(_) => Err(io::Error::other("vhost-user daemon thread panicked")),
            }
        } else {
            Ok(())
        }
    }
}

impl<F: FsProvider, M: DaxMapper> Drop for VhostUserDaemonHandle<F, M> {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Bind a `vhost-user` Unix domain socket at `socket_path` and serve a single VMM
/// connection until the frontend disconnects.
pub fn serve_vhost_user_socket<F, M, P>(
    socket_path: P,
    backend: Arc<RwLock<VhostUserFsBackend<F, M>>>,
) -> io::Result<()>
where
    F: FsProvider + 'static,
    M: DaxMapper + 'static,
    P: AsRef<Path>,
{
    let socket_path = socket_path.as_ref().to_path_buf();
    let exit_notifier = backend
        .read()
        .map_err(|_| io::Error::other("vhost-user backend lock poisoned"))?
        .clone_exit_notifier()?;

    let mut listener =
        Listener::new(&socket_path, true).map_err(|e| io::Error::other(e.to_string()))?;
    let atomic_mem = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let mut daemon = VhostUserDaemon::new(
        "projgit-virtiofs".to_string(),
        Arc::clone(&backend),
        atomic_mem,
    )
    .map_err(|e| io::Error::other(e.to_string()))?;

    let result = (|| -> io::Result<()> {
        daemon
            .start(&mut listener)
            .map_err(|e| io::Error::other(e.to_string()))?;
        // Wait for the VMM session to complete. Disconnection at session end is normal.
        let _ = daemon.wait();
        Ok(())
    })();

    let _ = exit_notifier.notify();
    let _ = std::fs::remove_file(&socket_path);
    result
}

/// Bind a `vhost-user` Unix domain socket synchronously at `socket_path` and spawn a
/// background [`VhostUserDaemon`] thread ready for a VMM (`Frontend`) to connect.
pub fn spawn_vhost_user_daemon<F, M, P>(
    socket_path: P,
    backend: Arc<RwLock<VhostUserFsBackend<F, M>>>,
) -> io::Result<VhostUserDaemonHandle<F, M>>
where
    F: FsProvider + 'static,
    M: DaxMapper + 'static,
    P: AsRef<Path>,
{
    let socket_path = socket_path.as_ref().to_path_buf();
    let exit_notifier = backend
        .read()
        .map_err(|_| io::Error::other("vhost-user backend lock poisoned"))?
        .clone_exit_notifier()?;
    let worker_exit_notifier = exit_notifier.try_clone()?;

    let mut listener =
        Listener::new(&socket_path, true).map_err(|e| io::Error::other(e.to_string()))?;
    let atomic_mem = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let mut daemon = VhostUserDaemon::new(
        "projgit-virtiofs".to_string(),
        Arc::clone(&backend),
        atomic_mem,
    )
    .map_err(|e| io::Error::other(e.to_string()))?;

    let accepted_flag = Arc::new(AtomicBool::new(false));
    let accepted_flag_bg = Arc::clone(&accepted_flag);
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_bg = Arc::clone(&shutdown_flag);
    let shutdown_handle = Arc::new(Mutex::new(None));
    let shutdown_handle_bg = Arc::clone(&shutdown_handle);

    let join_handle = thread::Builder::new()
        .name("projgit-vhost-user-daemon".to_string())
        .spawn(move || -> io::Result<()> {
            let start_res = daemon.start(&mut listener);
            accepted_flag_bg.store(true, Ordering::SeqCst);
            if let Err(e) = start_res {
                let _ = worker_exit_notifier.notify();
                if shutdown_flag_bg.load(Ordering::SeqCst) {
                    return Ok(());
                }
                return Err(io::Error::other(e.to_string()));
            }
            let sh = daemon.shutdown_handle();
            if shutdown_flag_bg.load(Ordering::SeqCst) {
                if let Some(ref h) = sh {
                    h.shutdown();
                }
            }
            *shutdown_handle_bg.lock() = sh;
            let _ = daemon.wait();
            let _ = worker_exit_notifier.notify();
            Ok(())
        })?;

    Ok(VhostUserDaemonHandle {
        socket_path,
        backend,
        exit_notifier,
        shutdown_handle,
        accepted_flag,
        shutdown_flag,
        join_handle: Some(join_handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtio_fs_config_encode_decode_and_truncate() {
        let cfg = VirtioFsConfig::new("projgit", 1);
        assert_eq!(cfg.tag_str(), "projgit");
        assert_eq!(cfg.num_request_queues, 1);

        let bytes = cfg.encode();
        assert_eq!(bytes.len(), VIRTIO_FS_CONFIG_SIZE);
        assert_eq!(&bytes[..7], b"projgit");
        assert!(bytes[7..VIRTIO_FS_TAG_LEN].iter().all(|&b| b == 0));
        assert_eq!(
            u32::from_le_bytes(bytes[VIRTIO_FS_TAG_LEN..].try_into().unwrap()),
            1
        );

        let decoded = VirtioFsConfig::decode(&bytes).expect("decode full config");
        assert_eq!(decoded, cfg);
        assert!(VirtioFsConfig::decode(&bytes[..VIRTIO_FS_CONFIG_SIZE - 1]).is_none());

        // Truncates tags longer than 36 bytes cleanly
        let long_cfg = VirtioFsConfig::new(&"x".repeat(VIRTIO_FS_TAG_LEN + 8), 1);
        assert_eq!(long_cfg.tag_str().len(), VIRTIO_FS_TAG_LEN);
    }
}
