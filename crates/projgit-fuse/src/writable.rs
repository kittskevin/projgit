//! Writable worktree overlay (Phase 2, Stage 2).
//!
//! A [`fuser::Filesystem`] that layers an in-memory **upper**
//! materialization store over a read-only [`FsProvider`] **lower**
//! (the projection), implementing the EdenFS-style materialize-on-write
//! model from `docs/design/writable-worktrees.md` §4–6:
//!
//! - Unmodified files stay **virtual** — reads fall through to the
//!   lower projection (which hydrates from the object store on demand).
//! - The first write to a file **materializes** it: the lower bytes are
//!   copied into the upper store and the write applied there; subsequent
//!   reads/writes serve the upper copy.
//! - Newly created files/dirs live entirely in the upper store; deleted
//!   entries are recorded as whiteouts.
//!
//! This is the design-faithful counterpart to the
//! `spikes/writable-nofork` harness (which proved stock git drives such
//! a mount without a fork), re-homed over the production [`FsProvider`].
//!
//! ## Scope (Stage 2)
//!
//! Read/write/create/unlink/mkdir/rmdir/rename/setattr are implemented.
//! The upper store is **in-memory** (on-disk overlay + crash
//! consistency are later stages), and the attribute-cache TTL is **zero**
//! — a production mount will keep a useful TTL and push a FUSE
//! invalidation on write (Stage 3 / R4); see
//! `docs/implementation/writable-worktrees-plan.md`.
//!
//! ## Inode namespace
//!
//! Lower inodes come from the [`FsProvider`] (high bit clear for tree
//! entries). New upper entries are allocated in the **synthetic** inode
//! space (high bit set, [`SYNTHETIC_INODE_BIT`]), keeping them disjoint
//! from lower tree inodes. Writable worktrees mount with an empty
//! `RootOverlay` (the `.git/` lives outside the mount), so there is no
//! clash with dotgit synthetic inodes.

use crate::adapter::{errno_for, to_fuser_attr, to_fuser_kind};
use fuser::{
    Config, FileHandle, Filesystem, FopenFlags, Generation, INodeNo, MountOption, OpenFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, TimeOrNow,
};
use projgit_core::overlay::SYNTHETIC_INODE_BIT;
use projgit_core::{Attr, FileType, FsError, FsProvider};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Attr/entry cache TTL for a writable mount **with** an invalidator
/// attached. Writes push a FUSE invalidation (Stage 3 / R4) off the
/// handler thread, so the kernel can safely cache attrs/data for
/// unmodified files between writes. A mount with no invalidator uses
/// `Duration::ZERO` (every getattr re-asks us) and stays correct.
const WRITABLE_TTL: Duration = Duration::from_secs(1);
const GEN: Generation = Generation(0);

/// A kernel-cache invalidation, applied off the FUSE handler thread.
/// Calling [`fuser::Notifier`] from *within* a handler can deadlock the
/// kernel, so write handlers enqueue these and a worker thread (holding
/// the post-mount `Notifier`) drains them.
enum Inval {
    /// Invalidate an inode's cached attributes + data.
    Inode(u64),
    /// Invalidate a `(parent, name)` directory-entry cache slot.
    Entry(u64, Vec<u8>),
}

impl Inval {
    fn apply(&self, n: &fuser::Notifier) {
        match self {
            Inval::Inode(ino) => {
                let _ = n.inval_inode(INodeNo(*ino), 0, 0);
            }
            Inval::Entry(parent, name) => {
                let _ = n.inval_entry(INodeNo(*parent), OsStr::from_bytes(name));
            }
        }
    }
}


/// A file or directory created inside the mount (no lower backing).
struct CreatedNode {
    parent: u64,
    name: Vec<u8>,
    kind: FileType,
    mode: u16,
}

/// The upper materialization layer.
#[derive(Default)]
struct Upper {
    /// Materialized bytes, keyed by inode (a modified lower file or a
    /// created file). Presence here means "serve from upper".
    content: HashMap<u64, Vec<u8>>,
    /// New entries created in the mount, keyed by their fresh inode.
    created: HashMap<u64, CreatedNode>,
    /// Per-parent additions: name -> child inode (created entries, plus
    /// re-pointed entries from `rename`).
    additions: HashMap<u64, BTreeMap<Vec<u8>, u64>>,
    /// Per-parent whiteouts: names that have been deleted/hidden.
    whiteouts: HashMap<u64, HashSet<Vec<u8>>>,
    /// Inodes whose mtime should read as "now" (materialized/edited).
    modified: HashSet<u64>,
    /// Counter for fresh upper inodes (OR'd with `SYNTHETIC_INODE_BIT`).
    next: u64,
}

impl Upper {
    fn alloc(&mut self) -> u64 {
        self.next += 1;
        SYNTHETIC_INODE_BIT | self.next
    }
    fn is_whiteouted(&self, parent: u64, name: &[u8]) -> bool {
        self.whiteouts
            .get(&parent)
            .is_some_and(|s| s.contains(name))
    }
}

/// Writable overlay filesystem: read-only `lower` projection + in-memory
/// upper materialization store.
pub struct WritableFs<F: FsProvider> {
    lower: Arc<F>,
    up: Mutex<Upper>,
    /// Off-thread kernel-cache invalidator. `None` => no invalidation
    /// and `ttl == 0` (every getattr re-asks us; always correct).
    inval: Option<Sender<Inval>>,
    /// Attr/entry cache TTL handed to the kernel.
    ttl: Duration,
}

impl<F: FsProvider> WritableFs<F> {
    /// Wrap a read-only projection provider as a writable overlay with
    /// no kernel-cache invalidation (TTL=0).
    pub fn new(lower: Arc<F>) -> Self {
        Self {
            lower,
            up: Mutex::new(Upper::default()),
            inval: None,
            ttl: Duration::ZERO,
        }
    }

    /// Like [`Self::new`] but with an off-thread invalidator, enabling a
    /// useful attr/data cache TTL backed by explicit invalidation.
    fn with_invalidator(lower: Arc<F>, inval: Sender<Inval>) -> Self {
        Self {
            lower,
            up: Mutex::new(Upper::default()),
            inval: Some(inval),
            ttl: WRITABLE_TTL,
        }
    }

    fn invalidate_inode(&self, ino: u64) {
        if let Some(tx) = &self.inval {
            let _ = tx.send(Inval::Inode(ino));
        }
    }

    fn invalidate_entry(&self, parent: u64, name: &[u8]) {
        if let Some(tx) = &self.inval {
            let _ = tx.send(Inval::Entry(parent, name.to_vec()));
        }
    }

    /// Resolve an inode's attributes (upper override, else lower).
    fn attr_for(&self, up: &Upper, ino: u64) -> Result<Attr, FsError> {
        if let Some(node) = up.created.get(&ino) {
            let size = up.content.get(&ino).map(|b| b.len() as u64).unwrap_or(0);
            let mut a = match node.kind {
                FileType::Directory => Attr::directory(ino),
                FileType::Symlink => Attr::symlink(ino, size),
                FileType::RegularFile => Attr::regular_file(ino, size, node.mode),
            };
            a.mode = node.mode;
            a.mtime = SystemTime::now();
            return Ok(a);
        }
        let mut a = self.lower.getattr(ino)?;
        if let Some(buf) = up.content.get(&ino) {
            a.size = buf.len() as u64;
        }
        if up.modified.contains(&ino) {
            a.mtime = SystemTime::now();
        }
        Ok(a)
    }

    /// Ensure `ino`'s content is present in the upper store, copying the
    /// lower bytes in on first materialization.
    fn materialize(&self, up: &mut Upper, ino: u64) -> Result<(), FsError> {
        if up.content.contains_key(&ino) {
            return Ok(());
        }
        // Lower file: read its full content through the projection.
        let size = self.lower.getattr(ino)?.size;
        let mut buf = Vec::with_capacity(size as usize);
        let mut off = 0u64;
        while off < size {
            let chunk = self.lower.read(ino, off, 64 * 1024)?;
            if chunk.is_empty() {
                break;
            }
            off += chunk.len() as u64;
            buf.extend_from_slice(&chunk);
        }
        up.content.insert(ino, buf);
        up.modified.insert(ino);
        Ok(())
    }

    /// Resolve `(parent, name)` to a child inode, honoring upper
    /// additions/whiteouts before the lower projection.
    fn resolve_child(&self, up: &Upper, parent: u64, name: &[u8]) -> Result<u64, FsError> {
        if up.is_whiteouted(parent, name) {
            return Err(FsError::NotFound);
        }
        if let Some(&ino) = up.additions.get(&parent).and_then(|m| m.get(name)) {
            return Ok(ino);
        }
        Ok(self.lower.lookup(parent, name)?.inode)
    }
}

impl<F: FsProvider + 'static> Filesystem for WritableFs<F> {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let up = self.up.lock().unwrap();
        let name = name.as_bytes();
        match self.resolve_child(&up, parent.0, name) {
            Ok(ino) => match self.attr_for(&up, ino) {
                Ok(a) => reply.entry(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid()), GEN),
                Err(e) => reply.error(errno_for(&e)),
            },
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let up = self.up.lock().unwrap();
        match self.attr_for(&up, ino.0) {
            Ok(a) => reply.attr(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid())),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let up = self.up.lock().unwrap();
        if let Some(buf) = up.content.get(&ino.0) {
            let b = buf.clone();
            drop(up);
            reply.data(&b);
            return;
        }
        drop(up);
        match self.lower.readlink(ino.0) {
            Ok(t) => reply.data(t.as_slice()),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
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
        _lock: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let up = self.up.lock().unwrap();
        if let Some(buf) = up.content.get(&ino.0) {
            let start = (offset as usize).min(buf.len());
            let end = (start + size as usize).min(buf.len());
            let slice = buf[start..end].to_vec();
            drop(up);
            reply.data(&slice);
            return;
        }
        drop(up);
        match self.lower.read(ino.0, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno_for(&e)),
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
        let up = self.up.lock().unwrap();
        let parent = ino.0;
        // A created directory has no lower backing; start empty.
        let is_created_dir = up
            .created
            .get(&parent)
            .is_some_and(|n| matches!(n.kind, FileType::Directory));
        let mut entries: Vec<(u64, FileType, Vec<u8>)> = Vec::new();
        let empty_whiteouts = HashSet::new();
        let whiteouts = up.whiteouts.get(&parent).unwrap_or(&empty_whiteouts);
        let additions = up.additions.get(&parent);

        if !is_created_dir {
            match self.lower.readdir(parent, 0) {
                Ok(lower) => {
                    for e in lower {
                        let name = e.name.to_vec();
                        if whiteouts.contains(&name) {
                            continue;
                        }
                        if additions.is_some_and(|m| m.contains_key(&name)) {
                            continue; // overridden by an upper addition
                        }
                        entries.push((e.inode, e.kind, name));
                    }
                }
                Err(e) => {
                    reply.error(errno_for(&e));
                    return;
                }
            }
        }
        if let Some(m) = additions {
            for (name, &cino) in m {
                if whiteouts.contains(name) {
                    continue;
                }
                let kind = up
                    .created
                    .get(&cino)
                    .map(|n| n.kind)
                    .unwrap_or(FileType::RegularFile);
                entries.push((cino, kind, name.clone()));
            }
        }

        // "." and ".." first, then merged entries, paginated by offset.
        let mut all: Vec<(u64, FileType, Vec<u8>)> = vec![
            (parent, FileType::Directory, b".".to_vec()),
            (parent, FileType::Directory, b"..".to_vec()),
        ];
        all.extend(entries);
        for (i, (cino, kind, name)) in all.iter().enumerate().skip(offset as usize) {
            let full = reply.add(
                INodeNo(*cino),
                (i + 1) as u64,
                to_fuser_kind(*kind),
                OsStr::from_bytes(name),
            );
            if full {
                break;
            }
        }
        reply.ok();
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut up = self.up.lock().unwrap();
        if let Some(newsize) = size {
            if !up.content.contains_key(&ino.0) && !up.created.contains_key(&ino.0) {
                if let Err(e) = self.materialize(&mut up, ino.0) {
                    reply.error(errno_for(&e));
                    return;
                }
            }
            up.content.entry(ino.0).or_default().resize(newsize as usize, 0);
            up.modified.insert(ino.0);
        }
        let attr = self.attr_for(&up, ino.0);
        drop(up);
        if size.is_some() {
            self.invalidate_inode(ino.0);
        }
        match attr {
            Ok(a) => reply.attr(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid())),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut up = self.up.lock().unwrap();
        if !up.content.contains_key(&ino.0) && !up.created.contains_key(&ino.0) {
            if let Err(e) = self.materialize(&mut up, ino.0) {
                reply.error(errno_for(&e));
                return;
            }
        }
        let buf = up.content.entry(ino.0).or_default();
        let start = offset as usize;
        let end = start + data.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[start..end].copy_from_slice(data);
        up.modified.insert(ino.0);
        drop(up);
        self.invalidate_inode(ino.0);
        reply.written(data.len() as u32);
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name = name.as_bytes().to_vec();
        let mut up = self.up.lock().unwrap();
        let ino = up.alloc();
        up.created.insert(
            ino,
            CreatedNode {
                parent: parent.0,
                name: name.clone(),
                kind: FileType::RegularFile,
                mode: (mode as u16) & 0o777,
            },
        );
        up.additions.entry(parent.0).or_default().insert(name.clone(), ino);
        up.content.insert(ino, Vec::new());
        up.modified.insert(ino);
        if let Some(s) = up.whiteouts.get_mut(&parent.0) {
            s.remove(&name);
        }
        let attr = self.attr_for(&up, ino);
        drop(up);
        self.invalidate_entry(parent.0, &name);
        match attr {
            Ok(a) => reply.created(
                &self.ttl,
                &to_fuser_attr(&a, req.uid(), req.gid()),
                GEN,
                FileHandle(0),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = name.as_bytes().to_vec();
        let mut up = self.up.lock().unwrap();
        let ino = up.alloc();
        up.created.insert(
            ino,
            CreatedNode {
                parent: parent.0,
                name: name.clone(),
                kind: FileType::Directory,
                mode: (mode as u16) & 0o777,
            },
        );
        up.additions.entry(parent.0).or_default().insert(name.clone(), ino);
        if let Some(s) = up.whiteouts.get_mut(&parent.0) {
            s.remove(&name);
        }
        let attr = self.attr_for(&up, ino);
        drop(up);
        self.invalidate_entry(parent.0, &name);
        match attr {
            Ok(a) => reply.entry(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid()), GEN),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = name.as_bytes().to_vec();
        let mut up = self.up.lock().unwrap();
        // If it was an upper addition, drop it; otherwise whiteout the
        // lower entry.
        let was_added = up
            .additions
            .get_mut(&parent.0)
            .and_then(|m| m.remove(&name));
        if let Some(ino) = was_added {
            up.content.remove(&ino);
            up.created.remove(&ino);
            up.modified.remove(&ino);
        }
        up.whiteouts.entry(parent.0).or_default().insert(name.clone());
        drop(up);
        self.invalidate_entry(parent.0, &name);
        reply.ok();
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.unlink(req, parent, name, reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let name = name.as_bytes().to_vec();
        let newname = newname.as_bytes().to_vec();
        let mut up = self.up.lock().unwrap();
        let src = match self.resolve_child(&up, parent.0, &name) {
            Ok(i) => i,
            Err(e) => {
                reply.error(errno_for(&e));
                return;
            }
        };
        // Materialize a lower source so the moved entry keeps its bytes
        // even though its name (and thus lower path) changes.
        let is_lower_file = !up.created.contains_key(&src)
            && self
                .lower
                .getattr(src)
                .map(|a| matches!(a.kind, FileType::RegularFile))
                .unwrap_or(false);
        if is_lower_file {
            let _ = self.materialize(&mut up, src);
        }
        // Detach the source name.
        up.additions
            .get_mut(&parent.0)
            .map(|m| m.remove(&name));
        up.whiteouts.entry(parent.0).or_default().insert(name.clone());
        // Point the destination at the same inode; overwrite any target.
        up.additions
            .entry(newparent.0)
            .or_default()
            .insert(newname.clone(), src);
        if let Some(s) = up.whiteouts.get_mut(&newparent.0) {
            s.remove(&newname);
        }
        if let Some(node) = up.created.get_mut(&src) {
            node.parent = newparent.0;
            node.name = newname.clone();
        }
        drop(up);
        self.invalidate_entry(parent.0, &name);
        self.invalidate_entry(newparent.0, &newname);
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _ds: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }
}

/// Mount a [`WritableFs`] over `lower` at `mountpoint`, returning a
/// background session. Unlike [`crate::mount_background`], the mount is
/// **read-write** (no `MountOption::RO`). The default read-only mount
/// path is unaffected.
pub fn mount_writable_background<F: FsProvider + 'static>(
    lower: Arc<F>,
    mountpoint: impl AsRef<Path>,
    config: &crate::MountConfig,
) -> std::io::Result<fuser::BackgroundSession> {
    let (tx, rx) = std::sync::mpsc::channel::<Inval>();
    let fs = WritableFs::with_invalidator(lower, tx);
    let mut fc = Config::default();
    fc.mount_options = vec![
        MountOption::FSName(config.name.clone()),
        MountOption::Subtype(config.subtype.clone()),
        MountOption::NoSuid,
        MountOption::NoDev,
    ];
    fc.acl = config.acl;
    fc.n_threads = config.n_threads;
    let session = fuser::spawn_mount2(fs, mountpoint, &fc)?;
    // Drain invalidations on a dedicated thread holding the post-mount
    // Notifier — calling it from within a handler can deadlock the
    // kernel. The thread exits when the WritableFs (and its Sender) is
    // dropped on unmount.
    let notifier = session.notifier();
    std::thread::Builder::new()
        .name("projgit-fuse-inval".to_string())
        .spawn(move || {
            for op in rx {
                op.apply(&notifier);
            }
        })
        .ok();
    Ok(session)
}
