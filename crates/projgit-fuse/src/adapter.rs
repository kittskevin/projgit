//! Adapter from [`projgit_core::FsProvider`] to [`fuser::Filesystem`].
//!
//! Translates fuser's tuple-struct newtypes (`INodeNo(u64)`,
//! `FileHandle(u64)`, `Generation(u64)`) and the `Errno` value-type
//! to / from the plain `u64` / `i32` shapes our trait uses, and
//! constructs the `FileAttr` shape fuser wants.
//!
//! The adapter is **stateless**: file and directory handles always
//! return `0` from `open` / `opendir`, and we re-resolve the inode on
//! every `read` / `readdir`. This is the right MVP design for a
//! projection-backed filesystem because all per-handle state would
//! just be a redundant cache of `(inode -> projection metadata)` that
//! the [`projgit_core::InodeAllocator`] already memoises.

use fuser::{
    BackingId, Errno, FileAttr, FileHandle, FileType as FuserFileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use projgit_core::{Attr, FileType, FsError, FsProvider};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Cache TTL we report to the kernel for `lookup` / `getattr` / `readdirplus` replies.
///
/// Projections are immutable for the lifetime of a mount (we mount one
/// commit, period), so the kernel can safely cache for a long time.
/// A day is generous and avoids re-asking us on every access; we'll
/// tune later if needed.
const ATTR_TTL: Duration = Duration::from_secs(86_400);

/// Generation number for `ReplyEntry` replies. Constant for the
/// lifetime of the mount because inodes don't get reused.
const GENERATION: Generation = Generation(0);

/// Default maximum blob size (16 MiB) that `open()` will synchronously
/// hydrate into `<git-dir>/blobs/<xx>/<oid>` for `FOPEN_PASSTHROUGH`.
///
/// Blobs larger than this threshold still use `FOPEN_PASSTHROUGH` if they
/// are already materialized on disk (or are projected `SyntheticHostFile`
/// entries like `.git/objects/pack/*.pack`), but unmaterialized large blobs
/// stay on `read_blob_range()` / standard FUSE so partial reads do not
/// stall `open()`.
pub const DEFAULT_PASSTHROUGH_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Default maximum number of idle (`open_count == 0`) kernel `BackingId`s
/// retained in the per-mount LRU cache for fast re-opens.
///
/// Bounding the idle cache prevents unbounded kernel inode pinning and lets
/// `prune_uncompressed_blobs()` reclaim disk space on long-lived mounts.
/// Set to `0` to close `BackingId` immediately when the last file handle
/// for an inode is released.
pub const DEFAULT_PASSTHROUGH_IDLE_CACHE_CAPACITY: usize = 512;

/// Snapshot of FUSE passthrough (`FOPEN_PASSTHROUGH`) counters for a [`ProjgitFuse`] instance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassthroughStats {
    /// Whether `FUSE_PASSTHROUGH` was requested in the mount configuration.
    pub requested: bool,
    /// Whether `FUSE_PASSTHROUGH` was negotiated with the kernel during `FUSE_INIT`.
    pub negotiated: bool,
    /// Whether passthrough was permanently latched off after the kernel rejected
    /// `FUSE_DEV_IOC_BACKING_OPEN` with `EPERM` (missing `CAP_SYS_ADMIN`) or `ELOOP`.
    pub disabled_by_kernel: bool,
    /// Number of `open()` calls served via `FOPEN_PASSTHROUGH` with a direct host backing fd.
    pub hits: u64,
    /// Number of `open()` calls where registering a backing fd failed and fell back to standard FUSE read.
    pub fallbacks: u64,
    /// Number of `open()` calls skipped for hydrate-on-open because the unmaterialized
    /// blob exceeded `passthrough_max_bytes`.
    pub skipped_too_large: u64,
    /// Number of unique inodes currently open in passthrough mode (`open_count > 0`).
    pub active_inodes: usize,
    /// Number of unique inodes currently open in standard FUSE mode (`open_count > 0`)
    /// while passthrough is negotiated on this mount.
    pub active_standard_inodes: usize,
    /// Number of idle (`open_count == 0`) kernel `BackingId`s retained in the bounded LRU.
    pub idle_cached_inodes: usize,
    /// Total number of unique inodes with a live kernel `BackingId`
    /// (`active_inodes + idle_cached_inodes`).
    pub cached_inodes: usize,
    /// Number of idle `BackingId`s evicted from the LRU (by capacity or explicit flush).
    pub idle_evictions: u64,
}

/// Shared handle for observing and managing a mounted [`ProjgitFuse`]'s
/// passthrough state after ownership of the filesystem has moved into
/// `fuser::spawn_mount2`.
#[derive(Clone)]
pub struct PassthroughHandle {
    state: Arc<PassthroughState>,
}

impl PassthroughHandle {
    /// Return a point-in-time snapshot of FUSE passthrough counters.
    pub fn stats(&self) -> PassthroughStats {
        self.state.stats()
    }

    /// Release all idle (`open_count == 0`) kernel `BackingId`s immediately
    /// without affecting any currently open passthrough file handles.
    ///
    /// Useful before or after [`projgit_core::prune_uncompressed_blobs`] so
    /// unlinked `<git-dir>/blobs/<xx>/<oid>` disk blocks are reclaimed
    /// promptly on long-lived mounts. Returns the number of idle backing
    /// entries released.
    pub fn evict_idle(&self) -> usize {
        self.state.evict_idle()
    }
}

/// Per-inode open-mode state coordinating Linux's `fs/fuse/iomode.c` invariants:
///
/// 1. Every concurrent passthrough open on the same inode must reuse the
///    exact same `BackingId` (`struct fuse_backing *`), otherwise the kernel
///    rejects the second open with `-EIO` ("mismatched backing").
/// 2. A `BackingId` must remain registered until at least `release()` (never
///    dropped while an `open()` reply referencing it is still in flight or
///    while any handle using it remains open).
/// 3. Passthrough and standard cached opens must never be mixed concurrently
///    on the same inode (`-EIO` "conflicting caching mode" / "FOPEN_PASSTHROUGH expected").
struct InodeSlot {
    open_count: u64,
    mode: SlotMode,
}

enum SlotMode {
    Empty,
    Passthrough(Arc<BackingId>),
    Standard,
    IdlePassthrough(Arc<BackingId>),
}

/// Bounded FIFO/LRU index of idle (`open_count == 0`) passthrough inodes.
///
/// Lock hierarchy invariant (strictly enforced across `PassthroughState`):
/// `inode_slots` (Level 1) -> `InodeSlot` (Level 2) -> `idle_lru` (Level 3).
/// No lock is ever acquired in reverse order, and `BackingId` values are
/// always dropped outside all locks.
#[derive(Default)]
struct IdleLru {
    next_gen: u64,
    ino_to_gen: HashMap<u64, u64>,
    gen_to_ino: BTreeMap<u64, u64>,
}

impl IdleLru {
    fn len(&self) -> usize {
        self.ino_to_gen.len()
    }

    fn contains(&self, ino: u64) -> bool {
        self.ino_to_gen.contains_key(&ino)
    }

    fn remove(&mut self, ino: u64) -> bool {
        if let Some(old_gen) = self.ino_to_gen.remove(&ino) {
            self.gen_to_ino.remove(&old_gen);
            true
        } else {
            false
        }
    }

    /// Insert or promote `ino` as most-recently-used and pop oldest entries
    /// until `len() <= capacity`. Returns any evicted inode numbers.
    fn insert_and_evict(&mut self, ino: u64, capacity: usize) -> Vec<u64> {
        self.remove(ino);
        self.next_gen = self.next_gen.wrapping_add(1);
        let g = self.next_gen;
        self.ino_to_gen.insert(ino, g);
        self.gen_to_ino.insert(g, ino);

        let mut evicted = Vec::new();
        while self.ino_to_gen.len() > capacity {
            let Some((&oldest_gen, &oldest_ino)) = self.gen_to_ino.iter().next() else {
                break;
            };
            self.gen_to_ino.remove(&oldest_gen);
            self.ino_to_gen.remove(&oldest_ino);
            evicted.push(oldest_ino);
        }
        evicted
    }

    fn drain_all(&mut self) -> Vec<u64> {
        self.gen_to_ino.clear();
        self.ino_to_gen.drain().map(|(ino, _)| ino).collect()
    }
}

struct PassthroughState {
    requested: bool,
    max_bytes: u64,
    idle_cache_capacity: usize,
    negotiated: AtomicBool,
    disabled_by_kernel: AtomicBool,
    hits: AtomicU64,
    fallbacks: AtomicU64,
    skipped_too_large: AtomicU64,
    active_passthrough_inodes: AtomicUsize,
    active_standard_inodes: AtomicUsize,
    idle_evictions: AtomicU64,
    inode_slots: Mutex<HashMap<u64, Arc<Mutex<InodeSlot>>>>,
    idle_lru: Mutex<IdleLru>,
}

impl PassthroughState {
    fn new(requested: bool, max_bytes: u64, idle_cache_capacity: usize) -> Self {
        Self {
            requested,
            max_bytes,
            idle_cache_capacity,
            negotiated: AtomicBool::new(false),
            disabled_by_kernel: AtomicBool::new(false),
            hits: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            skipped_too_large: AtomicU64::new(0),
            active_passthrough_inodes: AtomicUsize::new(0),
            active_standard_inodes: AtomicUsize::new(0),
            idle_evictions: AtomicU64::new(0),
            inode_slots: Mutex::new(HashMap::new()),
            idle_lru: Mutex::new(IdleLru::default()),
        }
    }

    fn is_active(&self) -> bool {
        self.negotiated.load(Ordering::Relaxed) && !self.disabled_by_kernel.load(Ordering::Relaxed)
    }

    fn stats(&self) -> PassthroughStats {
        let active_inodes = self.active_passthrough_inodes.load(Ordering::Relaxed);
        let active_standard_inodes = self.active_standard_inodes.load(Ordering::Relaxed);
        let idle_cached_inodes = self.idle_lru.lock().unwrap().len();
        PassthroughStats {
            requested: self.requested,
            negotiated: self.negotiated.load(Ordering::Relaxed),
            disabled_by_kernel: self.disabled_by_kernel.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            fallbacks: self.fallbacks.load(Ordering::Relaxed),
            skipped_too_large: self.skipped_too_large.load(Ordering::Relaxed),
            active_inodes,
            active_standard_inodes,
            idle_cached_inodes,
            cached_inodes: active_inodes + idle_cached_inodes,
            idle_evictions: self.idle_evictions.load(Ordering::Relaxed),
        }
    }

    fn get_or_create_slot(&self, ino: u64) -> Arc<Mutex<InodeSlot>> {
        let mut slots = self.inode_slots.lock().unwrap();
        slots
            .entry(ino)
            .or_insert_with(|| {
                Arc::new(Mutex::new(InodeSlot {
                    open_count: 0,
                    mode: SlotMode::Empty,
                }))
            })
            .clone()
    }

    fn get_slot(&self, ino: u64) -> Option<Arc<Mutex<InodeSlot>>> {
        self.inode_slots.lock().unwrap().get(&ino).cloned()
    }

    /// Remove `ino` from `inode_slots` if no other thread holds a handle to
    /// its `Arc<Mutex<InodeSlot>>` and the slot is empty (`open_count == 0`).
    fn cleanup_empty_slot(&self, ino: u64) {
        let mut slots = self.inode_slots.lock().unwrap();
        let should_remove = if let Some(entry) = slots.get(&ino) {
            if Arc::strong_count(entry) == 1 {
                let g = entry.lock().unwrap();
                g.open_count == 0 && matches!(g.mode, SlotMode::Empty)
            } else {
                false
            }
        } else {
            false
        };
        if should_remove {
            slots.remove(&ino);
        }
    }

    /// Drop the idle `BackingId` for `ino` if it is still idle and no longer
    /// tracked in `idle_lru`. Returns `true` if a `BackingId` was evicted.
    fn drop_evicted_idle_inode(&self, ino: u64) -> bool {
        let Some(slot) = self.get_slot(ino) else {
            return false;
        };
        let dropped_backing = {
            let mut guard = slot.lock().unwrap();
            if guard.open_count == 0
                && matches!(guard.mode, SlotMode::IdlePassthrough(_))
                && !self.idle_lru.lock().unwrap().contains(ino)
            {
                match std::mem::replace(&mut guard.mode, SlotMode::Empty) {
                    SlotMode::IdlePassthrough(b) => Some(b),
                    other => {
                        guard.mode = other;
                        None
                    }
                }
            } else {
                None
            }
        };
        drop(slot);
        let evicted = dropped_backing.is_some();
        drop(dropped_backing);
        if evicted {
            self.idle_evictions.fetch_add(1, Ordering::Relaxed);
            self.cleanup_empty_slot(ino);
        }
        evicted
    }

    fn evict_idle(&self) -> usize {
        let candidates = self.idle_lru.lock().unwrap().drain_all();
        let mut count = 0;
        for ino in candidates {
            if self.drop_evicted_idle_inode(ino) {
                count += 1;
            }
        }
        count
    }

    fn clear_all(&self) {
        self.idle_lru.lock().unwrap().drain_all();
        let drained: Vec<Arc<Mutex<InodeSlot>>> = self
            .inode_slots
            .lock()
            .unwrap()
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for slot in drained {
            let old = {
                let mut g = slot.lock().unwrap();
                g.open_count = 0;
                std::mem::replace(&mut g.mode, SlotMode::Empty)
            };
            drop(old);
        }
        self.active_passthrough_inodes.store(0, Ordering::Relaxed);
        self.active_standard_inodes.store(0, Ordering::Relaxed);
    }
}

/// Only `EPERM` (missing `CAP_SYS_ADMIN`) is mount-global and permanent.
/// Errors such as `ELOOP` (backing filesystem exceeds `max_stack_depth`) or
/// `EMFILE` can be specific to one backing filesystem or transient, so they
/// fall back for that open without disabling passthrough mount-wide.
fn is_permanent_passthrough_error(err: &std::io::Error) -> bool {
    err.kind() == ErrorKind::PermissionDenied || err.raw_os_error() == Some(libc::EPERM)
}

/// Adapter that exposes any [`FsProvider`] as a [`fuser::Filesystem`].
///
/// Holds an `Arc<F>` so the underlying provider can be shared with
/// non-FS callers (CLI introspection, doctor commands, etc.).
pub struct ProjgitFuse<F: FsProvider> {
    provider: Arc<F>,
    passthrough: Arc<PassthroughState>,
}

impl<F: FsProvider> ProjgitFuse<F> {
    /// Wrap a provider as a fuser filesystem with `FUSE_PASSTHROUGH` enabled when supported by the kernel.
    pub fn new(provider: Arc<F>) -> Self {
        Self::with_passthrough_config(
            provider,
            true,
            DEFAULT_PASSTHROUGH_MAX_BYTES,
            DEFAULT_PASSTHROUGH_IDLE_CACHE_CAPACITY,
        )
    }

    /// Wrap a provider as a fuser filesystem, explicitly controlling whether `FUSE_PASSTHROUGH`
    /// is requested during `FUSE_INIT`.
    pub fn with_passthrough(provider: Arc<F>, request_passthrough: bool) -> Self {
        Self::with_passthrough_config(
            provider,
            request_passthrough,
            DEFAULT_PASSTHROUGH_MAX_BYTES,
            DEFAULT_PASSTHROUGH_IDLE_CACHE_CAPACITY,
        )
    }

    /// Wrap a provider with full passthrough tuning parameters:
    ///
    /// - `request_passthrough`: negotiate `FUSE_PASSTHROUGH` during `FUSE_INIT`.
    /// - `passthrough_max_bytes`: maximum size of an unmaterialized blob to
    ///   hydrate synchronously on `open()`. Already-materialized blobs and
    ///   projected host files (`SyntheticHostFile`) always bypass this cap.
    /// - `idle_cache_capacity`: maximum idle (`open_count == 0`) `BackingId`s
    ///   kept in the LRU (`0` drops `BackingId` immediately on final `release()`).
    pub fn with_passthrough_config(
        provider: Arc<F>,
        request_passthrough: bool,
        passthrough_max_bytes: u64,
        idle_cache_capacity: usize,
    ) -> Self {
        Self {
            provider,
            passthrough: Arc::new(PassthroughState::new(
                request_passthrough,
                passthrough_max_bytes,
                idle_cache_capacity,
            )),
        }
    }

    /// Borrow the underlying provider.
    pub fn provider(&self) -> &F {
        &self.provider
    }

    /// Return a cloneable handle for reading [`PassthroughStats`] and flushing
    /// idle kernel `BackingId`s after ownership of `self` is moved into
    /// `fuser::spawn_mount2`.
    pub fn passthrough_handle(&self) -> PassthroughHandle {
        PassthroughHandle {
            state: Arc::clone(&self.passthrough),
        }
    }

    /// Return a snapshot of FUSE passthrough (`FOPEN_PASSTHROUGH`) counters.
    pub fn passthrough_stats(&self) -> PassthroughStats {
        self.passthrough.stats()
    }
}

impl<F: FsProvider + 'static> Filesystem for ProjgitFuse<F> {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), std::io::Error> {
        let desired = InitFlags::FUSE_DO_READDIRPLUS
            | InitFlags::FUSE_READDIRPLUS_AUTO
            | InitFlags::FUSE_PARALLEL_DIROPS
            | InitFlags::FUSE_CACHE_SYMLINKS
            | InitFlags::FUSE_EXPLICIT_INVAL_DATA;
        let supported = config.capabilities() & desired;
        let _ = config.add_capabilities(supported);

        // Negotiate FUSE passthrough (Linux 6.9+ with CAP_SYS_ADMIN) when the
        // kernel advertises `FUSE_PASSTHROUGH`. Setting `max_stack_depth = 1`
        // allows backing files on ext4/xfs/tmpfs (`s_stack_depth = 0 < 1`)
        // while still permitting `overlayfs` (`s_stack_depth = 2 <= FILESYSTEM_MAX_STACK_DEPTH`)
        // to stack on top of this FUSE mount.
        if self.passthrough.requested
            && config.capabilities().contains(InitFlags::FUSE_PASSTHROUGH)
            && config.add_capabilities(InitFlags::FUSE_PASSTHROUGH).is_ok()
            && config.set_max_stack_depth(1).is_ok()
        {
            self.passthrough.negotiated.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        self.passthrough.clear_all();
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.provider.lookup(parent.0, name.as_bytes()) {
            Ok(attr) => reply.entry(
                &ATTR_TTL,
                &to_fuser_attr(&attr, req.uid(), req.gid()),
                GENERATION,
            ),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.provider.getattr(ino.0) {
            Ok(attr) => reply.attr(&ATTR_TTL, &to_fuser_attr(&attr, req.uid(), req.gid())),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.provider.readlink(ino.0) {
            Ok(target) => reply.data(target.as_slice()),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let attr = match self.provider.getattr(ino.0) {
            Ok(attr) if attr.kind == FileType::Directory => {
                reply.error(Errno::EISDIR);
                return;
            }
            Err(e) => {
                reply.error(errno_for(&e));
                return;
            }
            Ok(attr) => attr,
        };

        // Fast path when passthrough was never negotiated: no per-inode
        // open-mode tracking is needed because every open uses standard FUSE.
        if !self.passthrough.negotiated.load(Ordering::Relaxed) {
            reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
            return;
        }

        let slot = self.passthrough.get_or_create_slot(ino.0);
        let mut stale_idle_backing: Option<Arc<BackingId>> = None;
        let mut latched_disable = false;

        let chosen_backing: Option<Arc<BackingId>> = {
            let mut guard = slot.lock().unwrap();
            match &guard.mode {
                // Already open in passthrough mode: reuse the exact same
                // `BackingId` (Linux `iomode.c` rejects mismatched backing IDs
                // on concurrent opens with `-EIO`).
                SlotMode::Passthrough(existing) => {
                    let existing = Arc::clone(existing);
                    guard.open_count += 1;
                    Some(existing)
                }
                // Already open in standard FUSE mode: stay in standard mode so
                // we never mix cached FUSE and passthrough on the same inode.
                SlotMode::Standard => {
                    guard.open_count += 1;
                    None
                }
                // Idle cached `BackingId` (`open_count == 0`): promote back to
                // active if passthrough is still enabled, or discard if disabled.
                SlotMode::IdlePassthrough(_) => {
                    self.passthrough.idle_lru.lock().unwrap().remove(ino.0);
                    if self.passthrough.is_active() {
                        let prev = std::mem::replace(&mut guard.mode, SlotMode::Empty);
                        if let SlotMode::IdlePassthrough(backing) = prev {
                            guard.open_count = 1;
                            guard.mode = SlotMode::Passthrough(Arc::clone(&backing));
                            self.passthrough
                                .active_passthrough_inodes
                                .fetch_add(1, Ordering::Relaxed);
                            Some(backing)
                        } else {
                            unreachable!("matched IdlePassthrough above");
                        }
                    } else {
                        if let SlotMode::IdlePassthrough(old) =
                            std::mem::replace(&mut guard.mode, SlotMode::Empty)
                        {
                            stale_idle_backing = Some(old);
                        }
                        guard.open_count = 1;
                        guard.mode = SlotMode::Standard;
                        self.passthrough
                            .active_standard_inodes
                            .fetch_add(1, Ordering::Relaxed);
                        None
                    }
                }
                // Cold open (`open_count == 0`): serialize first registration
                // per inode so concurrent openers share one `BackingId`.
                // Uses `local_backing_file` / `cached_backing_file` so `open()`
                // never blocks on a remote promisor fetch; remote-only blobs
                // hydrate on `read()` into L2 and promote on subsequent opens.
                SlotMode::Empty => {
                    let mut registered: Option<Arc<BackingId>> = None;
                    if self.passthrough.is_active() {
                        let file_opt = if attr.size <= self.passthrough.max_bytes {
                            self.provider.local_backing_file(ino.0).ok().flatten()
                        } else {
                            match self.provider.cached_backing_file(ino.0) {
                                Ok(Some(file)) => Some(file),
                                Ok(None) => {
                                    self.passthrough
                                        .skipped_too_large
                                        .fetch_add(1, Ordering::Relaxed);
                                    None
                                }
                                Err(_) => None,
                            }
                        };

                        if let Some(file) = file_opt {
                            match reply.open_backing(&*file) {
                                Ok(backing_id) => {
                                    registered = Some(Arc::new(backing_id));
                                }
                                Err(err) => {
                                    self.passthrough.fallbacks.fetch_add(1, Ordering::Relaxed);
                                    if is_permanent_passthrough_error(&err) {
                                        self.passthrough
                                            .disabled_by_kernel
                                            .store(true, Ordering::Relaxed);
                                        latched_disable = true;
                                    }
                                }
                            }
                        }
                    }

                    guard.open_count = 1;
                    if let Some(arc) = registered {
                        guard.mode = SlotMode::Passthrough(Arc::clone(&arc));
                        self.passthrough
                            .active_passthrough_inodes
                            .fetch_add(1, Ordering::Relaxed);
                        Some(arc)
                    } else {
                        guard.mode = SlotMode::Standard;
                        self.passthrough
                            .active_standard_inodes
                            .fetch_add(1, Ordering::Relaxed);
                        None
                    }
                }
            }
        };

        // Drop `slot` BEFORE writing the reply to `/dev/fuse` so that if the
        // opener immediately closes the fd and triggers `release()` on another
        // FUSE worker thread, `cleanup_empty_slot()` sees `strong_count == 1`
        // and reclaims the empty slot cleanly instead of leaking it.
        drop(slot);
        drop(stale_idle_backing);
        if latched_disable {
            self.passthrough.evict_idle();
        }

        if let Some(backing) = chosen_backing {
            self.passthrough.hits.fetch_add(1, Ordering::Relaxed);
            // Linux `fs/fuse/iomode.c` rejects `FOPEN_KEEP_CACHE` when combined
            // with `FOPEN_PASSTHROUGH` (`FOPEN_PASSTHROUGH_MASK` permits only
            // `FOPEN_DIRECT_IO`, `FOPEN_PARALLEL_DIRECT_WRITES`, and `FOPEN_NOFLUSH`).
            // `FOPEN_NOFLUSH` is safe on read-only projections and skips `FUSE_FLUSH`
            // on `close()` while still delivering `FUSE_RELEASE` on final `fput()`.
            reply.opened_passthrough(FileHandle(0), FopenFlags::FOPEN_NOFLUSH, &backing);
        } else {
            // Stateless handle == 0 with FOPEN_KEEP_CACHE so the kernel page cache
            // retains read pages across close/open cycles on immutable blobs.
            reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.provider.read(ino.0, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(slot) = self.passthrough.get_slot(ino.0) {
            let mut dropped_backing: Option<Arc<BackingId>> = None;
            let mut evicted_inodes: Vec<u64> = Vec::new();
            let mut became_empty = false;

            {
                let mut guard = slot.lock().unwrap();
                guard.open_count = guard.open_count.saturating_sub(1);
                if guard.open_count == 0 {
                    match std::mem::replace(&mut guard.mode, SlotMode::Empty) {
                        SlotMode::Passthrough(backing) => {
                            self.passthrough
                                .active_passthrough_inodes
                                .fetch_sub(1, Ordering::Relaxed);
                            if self.passthrough.idle_cache_capacity > 0
                                && self.passthrough.is_active()
                            {
                                guard.mode = SlotMode::IdlePassthrough(backing);
                                evicted_inodes =
                                    self.passthrough.idle_lru.lock().unwrap().insert_and_evict(
                                        ino.0,
                                        self.passthrough.idle_cache_capacity,
                                    );
                            } else {
                                dropped_backing = Some(backing);
                                became_empty = true;
                            }
                        }
                        SlotMode::Standard => {
                            self.passthrough
                                .active_standard_inodes
                                .fetch_sub(1, Ordering::Relaxed);
                            became_empty = true;
                        }
                        SlotMode::Empty => {
                            became_empty = true;
                        }
                        SlotMode::IdlePassthrough(backing) => {
                            // Defensive: shouldn't happen on release, preserve slot.
                            guard.mode = SlotMode::IdlePassthrough(backing);
                        }
                    }
                }
            }

            drop(slot);
            drop(dropped_backing);
            if became_empty {
                self.passthrough.cleanup_empty_slot(ino.0);
            }
            for victim in evicted_inodes {
                self.passthrough.drop_evicted_idle_inode(victim);
            }
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.provider.getattr(ino.0) {
            Ok(attr) if attr.kind != FileType::Directory => {
                reply.error(Errno::ENOTDIR);
            }
            Err(e) => {
                reply.error(errno_for(&e));
            }
            Ok(_) => {
                reply.opened(
                    FileHandle(0),
                    FopenFlags::FOPEN_KEEP_CACHE | FopenFlags::FOPEN_CACHE_DIR,
                );
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.provider.readdir(ino.0, offset) {
            Ok(e) => e,
            Err(e) => {
                reply.error(errno_for(&e));
                return;
            }
        };
        // FUSE assigns a per-entry resume offset. We use absolute
        // offset = (existing offset when we started) + (index in this
        // batch) + 1, so the next readdir call resumes correctly.
        for (i, entry) in entries.iter().enumerate() {
            let next_offset = offset + (i as u64) + 1;
            // `add` returns true if the buffer is full -- stop here
            // and the kernel will call back with `next_offset`.
            let full = reply.add(
                INodeNo(entry.inode),
                next_offset,
                to_fuser_kind(entry.kind),
                OsStr::from_bytes(entry.name.as_slice()),
            );
            if full {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let entries = match self.provider.readdirplus(ino.0, offset) {
            Ok(e) => e,
            Err(e) => {
                reply.error(errno_for(&e));
                return;
            }
        };
        for (i, entry) in entries.iter().enumerate() {
            let next_offset = offset + (i as u64) + 1;
            let fuser_attr = to_fuser_attr(&entry.attr, req.uid(), req.gid());
            let full = reply.add(
                INodeNo(entry.inode),
                next_offset,
                OsStr::from_bytes(entry.name.as_slice()),
                &ATTR_TTL,
                &fuser_attr,
                GENERATION,
            );
            if full {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

// -----------------------------------------------------------------------------
// type conversions
// -----------------------------------------------------------------------------

pub(crate) fn to_fuser_kind(k: FileType) -> FuserFileType {
    match k {
        FileType::RegularFile => FuserFileType::RegularFile,
        FileType::Directory => FuserFileType::Directory,
        FileType::Symlink => FuserFileType::Symlink,
    }
}

pub(crate) fn to_fuser_attr(a: &Attr, req_uid: u32, req_gid: u32) -> FileAttr {
    let kind = to_fuser_kind(a.kind);
    FileAttr {
        ino: INodeNo(a.inode),
        size: a.size,
        // 512-byte block accounting; we don't track real allocation,
        // so we just round up to satisfy `du` and friends.
        blocks: a.size.div_ceil(512),
        atime: a.mtime,
        mtime: a.mtime,
        ctime: a.mtime,
        crtime: a.mtime,
        kind,
        perm: a.mode,
        nlink: a.nlink,
        // Mirror the requesting process's uid/gid back as file
        // ownership. The provider has no OS-level concept of "the
        // mounting user"; the FUSE adapter does. Echoing per-request
        // makes the mount look owned by whoever is asking, which is
        // what `git`'s `safe.directory` check, IDE repo detection,
        // and `ls -la` all expect. The `a.uid` / `a.gid` from the
        // provider are ignored today (they default to 0 anyway).
        uid: req_uid,
        gid: req_gid,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

pub(crate) fn errno_for(e: &FsError) -> Errno {
    match e {
        FsError::NotFound => Errno::ENOENT,
        FsError::NotADirectory => Errno::ENOTDIR,
        FsError::NotAFile => Errno::EISDIR,
        FsError::NotASymlink => Errno::EINVAL,
        FsError::Io(_) => Errno::EIO,
        FsError::Unsupported => Errno::ENOSYS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_lru_evicts_oldest_and_promotes_on_reinsert() {
        let mut lru = IdleLru::default();
        assert!(lru.insert_and_evict(10, 2).is_empty());
        assert!(lru.insert_and_evict(20, 2).is_empty());
        assert_eq!(lru.len(), 2);

        // Promote 10 so 20 becomes oldest.
        assert!(lru.insert_and_evict(10, 2).is_empty());
        assert_eq!(lru.insert_and_evict(30, 2), vec![20]);
        assert!(!lru.contains(20));
        assert!(lru.contains(10));
        assert!(lru.contains(30));

        assert!(lru.remove(10));
        assert!(!lru.remove(10));
        assert_eq!(lru.drain_all(), vec![30]);
        assert_eq!(lru.len(), 0);
    }

    #[test]
    fn permanent_passthrough_error_only_latches_on_eperm() {
        let eperm = std::io::Error::from_raw_os_error(libc::EPERM);
        assert!(is_permanent_passthrough_error(&eperm));

        // ELOOP is per-backing-filesystem (not mount-global) and must not disable
        // passthrough for other backing files on valid filesystems.
        let eloop = std::io::Error::from_raw_os_error(libc::ELOOP);
        assert!(!is_permanent_passthrough_error(&eloop));

        let emfile = std::io::Error::from_raw_os_error(libc::EMFILE);
        assert!(!is_permanent_passthrough_error(&emfile));
    }

    #[test]
    fn passthrough_open_flags_exclude_keep_cache() {
        // Linux `fs/fuse/iomode.c` (`FOPEN_PASSTHROUGH_MASK`) rejects
        // `FOPEN_KEEP_CACHE` combined with `FOPEN_PASSTHROUGH` with `-EIO`.
        let allowed_mask = FopenFlags::FOPEN_PASSTHROUGH
            | FopenFlags::FOPEN_DIRECT_IO
            | FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES
            | FopenFlags::FOPEN_NOFLUSH;
        let sent = FopenFlags::FOPEN_NOFLUSH | FopenFlags::FOPEN_PASSTHROUGH;
        assert!(
            (sent & !allowed_mask).is_empty(),
            "passthrough open flags must stay within Linux FOPEN_PASSTHROUGH_MASK"
        );
    }
}
