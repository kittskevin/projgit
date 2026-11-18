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
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Snapshot of FUSE passthrough (`FOPEN_PASSTHROUGH`) counters for a [`ProjgitFuse`] instance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassthroughStats {
    /// Whether `FUSE_PASSTHROUGH` was negotiated with the kernel during `FUSE_INIT`.
    pub negotiated: bool,
    /// Number of `open()` calls served via `FOPEN_PASSTHROUGH` with a direct host backing fd.
    pub hits: u64,
    /// Number of `open()` calls where registering a backing fd failed and fell back to standard FUSE read.
    pub fallbacks: u64,
    /// Number of unique inodes with an active kernel `BackingId` registered.
    pub cached_inodes: usize,
}

/// Adapter that exposes any [`FsProvider`] as a [`fuser::Filesystem`].
///
/// Holds an `Arc<F>` so the underlying provider can be shared with
/// non-FS callers (CLI introspection, doctor commands, etc.).
pub struct ProjgitFuse<F: FsProvider> {
    provider: Arc<F>,
    request_passthrough: bool,
    passthrough_negotiated: AtomicBool,
    passthrough_hits: AtomicU64,
    passthrough_fallbacks: AtomicU64,
    /// Per-inode kernel `BackingId` cache for `FOPEN_PASSTHROUGH`.
    ///
    /// `fuser::BackingId` closes the kernel backing mapping on `Drop`
    /// (`FUSE_DEV_IOC_BACKING_CLOSE`), which must not happen while the
    /// file remains open in the kernel. Since projections are immutable,
    /// caching one `Arc<BackingId>` per inode for the lifetime of the
    /// mount keeps the backing mapping alive and avoids duplicate
    /// `FUSE_DEV_IOC_BACKING_OPEN` ioctls across repeated opens.
    backing_cache: Mutex<HashMap<u64, Arc<BackingId>>>,
}

impl<F: FsProvider> ProjgitFuse<F> {
    /// Wrap a provider as a fuser filesystem with `FUSE_PASSTHROUGH` enabled when supported by the kernel.
    pub fn new(provider: Arc<F>) -> Self {
        Self::with_passthrough(provider, true)
    }

    /// Wrap a provider as a fuser filesystem, explicitly controlling whether `FUSE_PASSTHROUGH`
    /// is requested during `FUSE_INIT`.
    pub fn with_passthrough(provider: Arc<F>, request_passthrough: bool) -> Self {
        Self {
            provider,
            request_passthrough,
            passthrough_negotiated: AtomicBool::new(false),
            passthrough_hits: AtomicU64::new(0),
            passthrough_fallbacks: AtomicU64::new(0),
            backing_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Borrow the underlying provider.
    pub fn provider(&self) -> &F {
        &self.provider
    }

    /// Return a snapshot of FUSE passthrough (`FOPEN_PASSTHROUGH`) counters.
    pub fn passthrough_stats(&self) -> PassthroughStats {
        let cached_inodes = self.backing_cache.lock().unwrap().len();
        PassthroughStats {
            negotiated: self.passthrough_negotiated.load(Ordering::Relaxed),
            hits: self.passthrough_hits.load(Ordering::Relaxed),
            fallbacks: self.passthrough_fallbacks.load(Ordering::Relaxed),
            cached_inodes,
        }
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
        if self.request_passthrough
            && config.capabilities().contains(InitFlags::FUSE_PASSTHROUGH)
            && config.add_capabilities(InitFlags::FUSE_PASSTHROUGH).is_ok()
            && config.set_max_stack_depth(1).is_ok()
        {
            self.passthrough_negotiated.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        if let Ok(mut cache) = self.backing_cache.lock() {
            cache.clear();
        }
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
        match self.provider.getattr(ino.0) {
            Ok(attr) if attr.kind == FileType::Directory => {
                reply.error(Errno::EISDIR);
                return;
            }
            Err(e) => {
                reply.error(errno_for(&e));
                return;
            }
            Ok(_) => {}
        }

        if self.passthrough_negotiated.load(Ordering::Relaxed) {
            let cached = self.backing_cache.lock().unwrap().get(&ino.0).cloned();
            if let Some(backing_id) = cached {
                self.passthrough_hits.fetch_add(1, Ordering::Relaxed);
                reply.opened_passthrough(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE, &backing_id);
                return;
            }
            if let Ok(Some(file)) = self.provider.backing_file(ino.0) {
                match reply.open_backing(&*file) {
                    Ok(backing_id) => {
                        let arc = Arc::new(backing_id);
                        self.backing_cache
                            .lock()
                            .unwrap()
                            .insert(ino.0, arc.clone());
                        self.passthrough_hits.fetch_add(1, Ordering::Relaxed);
                        reply.opened_passthrough(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE, &arc);
                        return;
                    }
                    Err(_) => {
                        self.passthrough_fallbacks.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // Stateless handle == 0 with FOPEN_KEEP_CACHE so the kernel page cache
        // retains read pages across close/open cycles on immutable blobs.
        reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
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
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
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

fn to_fuser_kind(k: FileType) -> FuserFileType {
    match k {
        FileType::RegularFile => FuserFileType::RegularFile,
        FileType::Directory => FuserFileType::Directory,
        FileType::Symlink => FuserFileType::Symlink,
    }
}

fn to_fuser_attr(a: &Attr, req_uid: u32, req_gid: u32) -> FileAttr {
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

fn errno_for(e: &FsError) -> Errno {
    match e {
        FsError::NotFound => Errno::ENOENT,
        FsError::NotADirectory => Errno::ENOTDIR,
        FsError::NotAFile => Errno::EISDIR,
        FsError::NotASymlink => Errno::EINVAL,
        FsError::Io(_) => Errno::EIO,
        FsError::Unsupported => Errno::ENOSYS,
    }
}
