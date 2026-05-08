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
    Errno, FileAttr, FileHandle, FileType as FuserFileType, Filesystem, FopenFlags, Generation,
    INodeNo, KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use projgit_core::{Attr, FileType, FsError, FsProvider};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Duration;

/// Cache TTL we report to the kernel for `lookup` / `getattr` replies.
///
/// Projections are immutable for the lifetime of a mount (we mount one
/// commit, period), so the kernel can safely cache for a long time.
/// A day is generous and avoids re-asking us on every access; we'll
/// tune later if needed.
const ATTR_TTL: Duration = Duration::from_secs(86_400);

/// Generation number for `ReplyEntry` replies. Constant for the
/// lifetime of the mount because inodes don't get reused.
const GENERATION: Generation = Generation(0);

/// Adapter that exposes any [`FsProvider`] as a [`fuser::Filesystem`].
///
/// Holds an `Arc<F>` so the underlying provider can be shared with
/// non-FS callers (CLI introspection, doctor commands, etc.).
pub struct ProjgitFuse<F: FsProvider> {
    provider: Arc<F>,
}

impl<F: FsProvider> ProjgitFuse<F> {
    /// Wrap a provider as a fuser filesystem.
    pub fn new(provider: Arc<F>) -> Self {
        Self { provider }
    }

    /// Borrow the underlying provider.
    pub fn provider(&self) -> &F {
        &self.provider
    }
}

impl<F: FsProvider + 'static> Filesystem for ProjgitFuse<F> {
    fn init(
        &mut self,
        _req: &Request,
        _config: &mut KernelConfig,
    ) -> Result<(), std::io::Error> {
        // Nothing to negotiate yet. Future: enable readdirplus,
        // adjust readahead, etc.
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.provider.lookup(parent.0, name.as_bytes()) {
            Ok(attr) => reply.entry(&ATTR_TTL, &to_fuser_attr(&attr), GENERATION),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        match self.provider.getattr(ino.0) {
            Ok(attr) => reply.attr(&ATTR_TTL, &to_fuser_attr(&attr)),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.provider.readlink(ino.0) {
            Ok(target) => reply.data(target.as_slice()),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // Stateless: handle == 0. Caller passes us back the inode in
        // every subsequent op anyway.
        reply.opened(FileHandle(0), FopenFlags::empty());
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

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
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

fn to_fuser_attr(a: &Attr) -> FileAttr {
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
        uid: a.uid,
        gid: a.gid,
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
