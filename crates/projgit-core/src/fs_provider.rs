//! `FsProvider` — the OS-agnostic, read-only filesystem trait that
//! every projgit FS backend implements.
//!
//! ## Scope
//!
//! Phase 3a: just the trait shape, attribute / entry / error types,
//! the [`InodeAllocator`], and an [`InMemoryFsProvider`] that exists
//! purely so we can unit-test the abstraction without bringing in a
//! real backend. Phase 3b implements `fuser`; Phase 3d implements
//! WinFsp.
//!
//! ## Key design choices
//!
//! 1. **Inode keys, not paths.** Both FUSE and WinFsp address files
//!    by handle / inode after the initial lookup, so a path-keyed API
//!    would force redundant tree walks on every call.
//! 2. **OS-shaped types.** [`Attr`], [`FileType`], etc. are POSIX-ish
//!    (with platform notes) so backends can map them directly to the
//!    OS structures they need to fill in. `gix` types stay buried in
//!    the projection layer.
//! 3. **Sync, not async.** FS callbacks are synchronous on every
//!    backend we target. Going async would force every consumer to
//!    also be async with no immediate benefit.
//! 4. **No mutation methods.** Read-only is an invariant of the MVP
//!    architecture, not a deferred feature.
//!
//! ## Inode allocation
//!
//! See [`InodeAllocator`]. The mapping is:
//!
//! - **Inode 1**: always the projection root.
//! - **Tree-derived inodes**: hash of `(projection_id, oid, path)`
//!   with the high bit clear.
//! - **Synthetic inodes**: same shape with the high bit set, per the
//!   reservation in `crate::overlay`.
//!
//! The allocator memoises (inode → metadata) so repeated lookups are
//! O(1) and so backends can resolve an opaque inode back to whatever
//! they need to answer `getattr` etc.

use crate::overlay::{mark_synthetic_inode, SYNTHETIC_INODE_BIT};
use bstr::BString;
use gix::ObjectId;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

// ----------------------------------------------------------------------------
// Public types
// ----------------------------------------------------------------------------

/// FUSE convention: the root of any mount has inode `1`.
pub const ROOT_INODE: u64 = 1;

/// Distinguishes the OS-level kind of a virtual file. Mirrors what
/// FUSE's `FileType` and WinFsp's `FILE_ATTRIBUTE_*` flags care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// A regular file.
    RegularFile,
    /// A directory.
    Directory,
    /// A POSIX-style symlink. The target string is fetched via
    /// [`FsProvider::readlink`].
    Symlink,
}

/// POSIX-shaped file attributes. Backends translate these into their
/// own structures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    /// Inode number identifying this entry within the mount.
    pub inode: u64,
    /// File kind.
    pub kind: FileType,
    /// Logical file size in bytes. For directories this is opaque /
    /// implementation-defined; backends may report 0.
    pub size: u64,
    /// Permission bits (`0o644`, `0o755`, `0o777` for symlinks, etc.).
    /// Backends decide which bits the OS honors.
    pub mode: u16,
    /// Hard-link count. For projgit projections this is always 1; we
    /// surface it for completeness.
    pub nlink: u32,
    /// Owning user id (POSIX). On Windows backends this is irrelevant
    /// at the FsProvider layer; the WinFsp backend synthesises a
    /// per-user security descriptor independently.
    pub uid: u32,
    /// Owning group id (POSIX); see `uid`.
    pub gid: u32,
    /// Last-modified time. Projections expose the commit time, not
    /// per-file mtime.
    pub mtime: SystemTime,
}

impl Attr {
    /// Helper: a sensible default for a regular file with `size`
    /// bytes and mode `mode`. `mtime` defaults to the UNIX epoch so
    /// callers must overwrite it with the projection's commit time.
    pub fn regular_file(inode: u64, size: u64, mode: u16) -> Self {
        Self {
            inode,
            kind: FileType::RegularFile,
            size,
            mode,
            nlink: 1,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    /// Helper: a sensible default for a directory.
    pub fn directory(inode: u64) -> Self {
        Self {
            inode,
            kind: FileType::Directory,
            size: 0,
            mode: 0o755,
            // Convention: directories carry nlink = 2 (`.` and parent).
            // Backends that want the precise child count can override.
            nlink: 2,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    /// Helper: a sensible default for a symlink.
    pub fn symlink(inode: u64, target_len: u64) -> Self {
        Self {
            inode,
            kind: FileType::Symlink,
            size: target_len,
            mode: 0o777,
            nlink: 1,
            uid: 0,
            gid: 0,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }
}

/// One entry as returned by [`FsProvider::readdir`].
///
/// Keeping `name` as a [`BString`] (bytes, not UTF-8) matches git's
/// own filename storage and lets the FUSE backend pass through names
/// containing arbitrary bytes losslessly. The Windows backend will
/// have to UTF-8-decode for `U16CSTR` conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Inode of the child.
    pub inode: u64,
    /// Entry name.
    pub name: BString,
    /// File kind, for fast-path handling without a follow-up
    /// `getattr` (FUSE's `readdirplus` consumes this).
    pub kind: FileType,
}

/// Errors a backend may surface up to the OS.
///
/// Backends translate these into their own platform errors:
/// FUSE → `errno`, WinFsp → `NTSTATUS`. The set is intentionally
/// small.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    /// Path component does not exist.
    #[error("entry not found")]
    NotFound,

    /// Tried to descend into a non-directory or treat a directory as
    /// a file.
    #[error("invalid entry kind for this operation")]
    NotADirectory,

    /// Tried to read from a non-file.
    #[error("entry is not a regular file")]
    NotAFile,

    /// Tried to readlink something that isn't a symlink.
    #[error("entry is not a symlink")]
    NotASymlink,

    /// I/O error or backend failure.
    #[error("io error: {0}")]
    Io(String),

    /// Operation is not implemented for this backend or this entry.
    #[error("operation not supported")]
    Unsupported,
}

/// The contract every FS backend implements.
///
/// All methods take `&self`; backends are expected to use interior
/// mutability for any per-handle state.
pub trait FsProvider: Send + Sync {
    /// Resolve `(parent_inode, name)` to the child's [`Attr`].
    /// Returns [`FsError::NotFound`] if no entry with that name exists.
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<Attr, FsError>;

    /// Look up an inode's attributes directly. The inode must have
    /// previously been returned by [`Self::lookup`] or be
    /// [`ROOT_INODE`].
    fn getattr(&self, inode: u64) -> Result<Attr, FsError>;

    /// List the entries of a directory.
    ///
    /// `offset` lets backends page through long listings: callers
    /// pass `0` first, then the offset of the last entry they
    /// consumed plus one. Implementations that don't need pagination
    /// may always return all entries when `offset == 0` and an empty
    /// vector otherwise.
    fn readdir(&self, inode: u64, offset: u64) -> Result<Vec<DirEntry>, FsError>;

    /// Read up to `size` bytes from `inode` starting at `offset`.
    /// Returns fewer bytes than requested at end-of-file.
    fn read(&self, inode: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError>;

    /// Return a symlink's target as a byte string.
    fn readlink(&self, inode: u64) -> Result<BString, FsError>;
}

// ----------------------------------------------------------------------------
// InodeAllocator
// ----------------------------------------------------------------------------

/// What an inode resolves to. Backends use this to map an opaque
/// inode back to whatever they need to answer subsequent calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InodeKind {
    /// The mount root.
    Root,
    /// A real git tree entry inside the projection.
    TreeEntry {
        /// The OID of the underlying object (blob / tree / commit-for-gitlink).
        oid: ObjectId,
        /// `/`-separated virtual path from the projection root.
        path: BString,
    },
    /// A synthetic [`crate::overlay::SyntheticEntry`] from the
    /// [`crate::RootOverlay`].
    Synthetic {
        /// Virtual path (from the projection root) of the synthetic entry.
        path: BString,
    },
}

/// Allocates and memoises stable inode numbers per mount.
///
/// Per `docs/implementation/initial-plan.md` §5.2 the desired property is
/// `(projection_id, blob_oid, path) -> u64`. This implementation
/// deterministically hashes the inputs and caches the bidirectional
/// mapping so backends can both *allocate* (during `lookup` /
/// `readdir`) and *resolve* (during `getattr` / `read`) inodes.
///
/// Stability:
///
/// - The same `(projection_id, oid, path)` always produces the same
///   inode within a single allocator's lifetime.
/// - Synthetic-inode allocations always have the high bit set, per
///   the reservation in [`crate::overlay::SYNTHETIC_INODE_BIT`].
/// - Inode `1` is reserved for the projection root.
pub struct InodeAllocator {
    projection_id: u64,
    forward: RwLock<std::collections::HashMap<u64, InodeKind>>,
    // We don't need a reverse map for correctness (the forward map
    // alone is enough because lookup re-derives), but caching it
    // would let us turn a (oid, path) pair into an inode in O(1)
    // without re-hashing. Skipping for MVP.
    _next: Mutex<()>, // placeholder if we ever want sequential IDs
}

impl InodeAllocator {
    /// Create an allocator scoped to one projection. The
    /// `projection_id` mixes into the hash so two projections of the
    /// same content produce different inodes (avoids cross-mount
    /// inode bleed in tests + future daemon scenarios).
    pub fn new(projection_id: u64) -> Self {
        let mut alloc = Self {
            projection_id,
            forward: RwLock::new(std::collections::HashMap::new()),
            _next: Mutex::new(()),
        };
        alloc
            .forward
            .get_mut()
            .unwrap()
            .insert(ROOT_INODE, InodeKind::Root);
        alloc
    }

    /// Inode for the projection root.
    pub const fn root_inode(&self) -> u64 {
        ROOT_INODE
    }

    /// Get or allocate an inode for a real tree entry.
    pub fn for_tree_entry(&self, oid: ObjectId, path: &[u8]) -> u64 {
        let inode = self.hash_tree(oid, path);
        self.remember(
            inode,
            InodeKind::TreeEntry {
                oid,
                path: BString::from(path),
            },
        );
        inode
    }

    /// Get or allocate an inode for a synthetic entry. Always has the
    /// synthetic high bit set.
    pub fn for_synthetic(&self, path: &[u8]) -> u64 {
        let raw = self.hash_synthetic(path);
        let inode = mark_synthetic_inode(raw & !SYNTHETIC_INODE_BIT);
        self.remember(
            inode,
            InodeKind::Synthetic {
                path: BString::from(path),
            },
        );
        inode
    }

    /// Resolve an inode back to what it represents. Returns `None` if
    /// the allocator has never seen this inode.
    pub fn resolve(&self, inode: u64) -> Option<InodeKind> {
        self.forward.read().unwrap().get(&inode).cloned()
    }

    /// Whether the allocator has memoised this inode.
    pub fn is_known(&self, inode: u64) -> bool {
        self.forward.read().unwrap().contains_key(&inode)
    }

    /// Test-facing: how many distinct inodes have been issued.
    pub fn len(&self) -> usize {
        self.forward.read().unwrap().len()
    }

    /// True if no inodes besides the root have been issued.
    pub fn is_empty(&self) -> bool {
        self.len() <= 1
    }

    // -- internals --

    fn remember(&self, inode: u64, kind: InodeKind) {
        let mut m = self.forward.write().unwrap();
        m.entry(inode).or_insert(kind);
    }

    fn hash_tree(&self, oid: ObjectId, path: &[u8]) -> u64 {
        let mut h = DefaultHasher::new();
        // Tag distinguishes namespace so the hash inputs can't collide.
        b"tree".hash(&mut h);
        self.projection_id.hash(&mut h);
        oid.as_bytes().hash(&mut h);
        path.hash(&mut h);
        // Clear the synthetic high bit and avoid 0/1 (reserved).
        let raw = h.finish() & !SYNTHETIC_INODE_BIT;
        if raw <= ROOT_INODE {
            raw + 2
        } else {
            raw
        }
    }

    fn hash_synthetic(&self, path: &[u8]) -> u64 {
        let mut h = DefaultHasher::new();
        b"synth".hash(&mut h);
        self.projection_id.hash(&mut h);
        path.hash(&mut h);
        let raw = h.finish() & !SYNTHETIC_INODE_BIT;
        if raw <= ROOT_INODE {
            raw + 2
        } else {
            raw
        }
    }
}

// ----------------------------------------------------------------------------
// InMemoryFsProvider — for tests
// ----------------------------------------------------------------------------

/// A fully in-memory [`FsProvider`] used purely to validate the trait
/// shape and write integration tests. Not a public API; consumers
/// will build their own providers (one per backend) on top of
/// [`crate::Projection`] in subsequent phase-3 deliveries.
pub struct InMemoryFsProvider {
    inner: RwLock<MemFs>,
}

#[derive(Default)]
struct MemFs {
    next_inode: u64,
    nodes: std::collections::HashMap<u64, MemNode>,
}

#[derive(Debug, Clone)]
enum MemNode {
    Dir {
        children: std::collections::BTreeMap<BString, u64>,
        attr: Attr,
    },
    File {
        bytes: Vec<u8>,
        attr: Attr,
    },
    Symlink {
        target: BString,
        attr: Attr,
    },
}

impl InMemoryFsProvider {
    /// New empty FS containing only the root directory.
    pub fn new() -> Self {
        let mut nodes = std::collections::HashMap::new();
        let root_attr = Attr::directory(ROOT_INODE);
        nodes.insert(
            ROOT_INODE,
            MemNode::Dir {
                children: Default::default(),
                attr: root_attr,
            },
        );
        Self {
            inner: RwLock::new(MemFs {
                next_inode: ROOT_INODE + 1,
                nodes,
            }),
        }
    }

    /// Adds a regular file under `parent_inode` with the given name.
    /// Returns its inode.
    pub fn add_file(&self, parent: u64, name: &[u8], bytes: Vec<u8>) -> u64 {
        let mut m = self.inner.write().unwrap();
        let inode = m.next_inode;
        m.next_inode += 1;
        let attr = Attr::regular_file(inode, bytes.len() as u64, 0o644);
        m.nodes.insert(inode, MemNode::File { bytes, attr });
        if let Some(MemNode::Dir { children, .. }) = m.nodes.get_mut(&parent) {
            children.insert(BString::from(name), inode);
        } else {
            panic!("parent inode {parent} is not a directory");
        }
        inode
    }

    /// Adds a subdirectory.
    pub fn add_dir(&self, parent: u64, name: &[u8]) -> u64 {
        let mut m = self.inner.write().unwrap();
        let inode = m.next_inode;
        m.next_inode += 1;
        let attr = Attr::directory(inode);
        m.nodes.insert(
            inode,
            MemNode::Dir {
                children: Default::default(),
                attr,
            },
        );
        if let Some(MemNode::Dir { children, .. }) = m.nodes.get_mut(&parent) {
            children.insert(BString::from(name), inode);
        } else {
            panic!("parent inode {parent} is not a directory");
        }
        inode
    }

    /// Adds a symlink pointing at `target`.
    pub fn add_symlink(&self, parent: u64, name: &[u8], target: &[u8]) -> u64 {
        let mut m = self.inner.write().unwrap();
        let inode = m.next_inode;
        m.next_inode += 1;
        let attr = Attr::symlink(inode, target.len() as u64);
        m.nodes.insert(
            inode,
            MemNode::Symlink {
                target: BString::from(target),
                attr,
            },
        );
        if let Some(MemNode::Dir { children, .. }) = m.nodes.get_mut(&parent) {
            children.insert(BString::from(name), inode);
        } else {
            panic!("parent inode {parent} is not a directory");
        }
        inode
    }
}

impl Default for InMemoryFsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FsProvider for InMemoryFsProvider {
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<Attr, FsError> {
        let m = self.inner.read().unwrap();
        let dir = m.nodes.get(&parent).ok_or(FsError::NotFound)?;
        let children = match dir {
            MemNode::Dir { children, .. } => children,
            _ => return Err(FsError::NotADirectory),
        };
        let inode = *children
            .get(bstr::BStr::new(name))
            .ok_or(FsError::NotFound)?;
        Ok(node_attr(m.nodes.get(&inode).unwrap()))
    }

    fn getattr(&self, inode: u64) -> Result<Attr, FsError> {
        let m = self.inner.read().unwrap();
        let n = m.nodes.get(&inode).ok_or(FsError::NotFound)?;
        Ok(node_attr(n))
    }

    fn readdir(&self, inode: u64, offset: u64) -> Result<Vec<DirEntry>, FsError> {
        let m = self.inner.read().unwrap();
        let n = m.nodes.get(&inode).ok_or(FsError::NotFound)?;
        let children = match n {
            MemNode::Dir { children, .. } => children,
            _ => return Err(FsError::NotADirectory),
        };
        let mut out: Vec<DirEntry> = children
            .iter()
            .map(|(name, inode)| {
                let kind = match m.nodes.get(inode).unwrap() {
                    MemNode::Dir { .. } => FileType::Directory,
                    MemNode::File { .. } => FileType::RegularFile,
                    MemNode::Symlink { .. } => FileType::Symlink,
                };
                DirEntry {
                    inode: *inode,
                    name: name.clone(),
                    kind,
                }
            })
            .collect();
        if offset > 0 {
            // Drop the first `offset` entries; this is the simplest
            // pagination contract that satisfies the doc.
            let start = (offset as usize).min(out.len());
            out.drain(..start);
        }
        Ok(out)
    }

    fn read(&self, inode: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let m = self.inner.read().unwrap();
        let n = m.nodes.get(&inode).ok_or(FsError::NotFound)?;
        match n {
            MemNode::File { bytes, .. } => {
                let start = (offset as usize).min(bytes.len());
                let end = start.saturating_add(size as usize).min(bytes.len());
                Ok(bytes[start..end].to_vec())
            }
            _ => Err(FsError::NotAFile),
        }
    }

    fn readlink(&self, inode: u64) -> Result<BString, FsError> {
        let m = self.inner.read().unwrap();
        let n = m.nodes.get(&inode).ok_or(FsError::NotFound)?;
        match n {
            MemNode::Symlink { target, .. } => Ok(target.clone()),
            _ => Err(FsError::NotASymlink),
        }
    }
}

fn node_attr(n: &MemNode) -> Attr {
    match n {
        MemNode::Dir { attr, .. } => *attr,
        MemNode::File { attr, .. } => *attr,
        MemNode::Symlink { attr, .. } => *attr,
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::is_synthetic_inode;

    #[test]
    fn root_inode_is_one() {
        let alloc = InodeAllocator::new(0);
        assert_eq!(alloc.root_inode(), 1);
        assert!(matches!(alloc.resolve(1), Some(InodeKind::Root)));
    }

    #[test]
    fn tree_inode_is_stable_and_low_bit() {
        let alloc = InodeAllocator::new(7);
        let oid = ObjectId::null(gix::hash::Kind::Sha1);
        let i1 = alloc.for_tree_entry(oid, b"src/main.rs");
        let i2 = alloc.for_tree_entry(oid, b"src/main.rs");
        assert_eq!(i1, i2, "same key must yield same inode");
        assert!(!is_synthetic_inode(i1));
        assert_ne!(i1, ROOT_INODE);
    }

    #[test]
    fn synthetic_inode_has_high_bit() {
        let alloc = InodeAllocator::new(0);
        let i = alloc.for_synthetic(b".projgit/info.json");
        assert!(is_synthetic_inode(i));
    }

    #[test]
    fn distinct_paths_produce_distinct_inodes() {
        let alloc = InodeAllocator::new(0);
        let oid = ObjectId::null(gix::hash::Kind::Sha1);
        let a = alloc.for_tree_entry(oid, b"a");
        let b = alloc.for_tree_entry(oid, b"b");
        assert_ne!(a, b);
    }

    #[test]
    fn distinct_projections_isolate_inodes() {
        let oid = ObjectId::null(gix::hash::Kind::Sha1);
        let p1 = InodeAllocator::new(1);
        let p2 = InodeAllocator::new(2);
        let i1 = p1.for_tree_entry(oid, b"same/path");
        let i2 = p2.for_tree_entry(oid, b"same/path");
        assert_ne!(
            i1, i2,
            "different projection_ids must namespace inodes apart"
        );
    }

    #[test]
    fn resolve_round_trips() {
        let alloc = InodeAllocator::new(0);
        let oid = ObjectId::null(gix::hash::Kind::Sha1);
        let i = alloc.for_tree_entry(oid, b"x/y/z");
        match alloc.resolve(i) {
            Some(InodeKind::TreeEntry { oid: o, path }) => {
                assert_eq!(o, oid);
                assert_eq!(path, BString::from("x/y/z"));
            }
            other => panic!("expected TreeEntry, got {other:?}"),
        }
    }

    // ---- InMemoryFsProvider ----

    #[test]
    fn inmem_fs_basics() {
        let fs = InMemoryFsProvider::new();

        // Empty root: lookup any name fails.
        assert_eq!(fs.lookup(ROOT_INODE, b"nope"), Err(FsError::NotFound));
        assert_eq!(fs.readdir(ROOT_INODE, 0).unwrap(), Vec::<DirEntry>::new());

        // Add a file at the root.
        let file_inode = fs.add_file(ROOT_INODE, b"hello.txt", b"hi\n".to_vec());
        let attr = fs.lookup(ROOT_INODE, b"hello.txt").unwrap();
        assert_eq!(attr.inode, file_inode);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.size, 3);

        // Read it.
        let bytes = fs.read(file_inode, 0, 100).unwrap();
        assert_eq!(&bytes, b"hi\n");

        // Read past EOF returns empty.
        let bytes = fs.read(file_inode, 100, 100).unwrap();
        assert!(bytes.is_empty());

        // readlink on a regular file fails clearly.
        assert_eq!(fs.readlink(file_inode), Err(FsError::NotASymlink));
    }

    #[test]
    fn inmem_fs_directory_and_symlink() {
        let fs = InMemoryFsProvider::new();
        let dir = fs.add_dir(ROOT_INODE, b"src");
        fs.add_file(dir, b"main.rs", b"fn main(){}\n".to_vec());
        let link = fs.add_symlink(ROOT_INODE, b"main", b"src/main.rs");

        // readdir at root sees both children, in sorted order.
        let entries = fs.readdir(ROOT_INODE, 0).unwrap();
        let names: Vec<&[u8]> = entries.iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(names, vec![b"main".as_ref(), b"src".as_ref()]);

        // readdir into the subdir.
        let entries = fs.readdir(dir, 0).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, FileType::RegularFile);

        // readlink works on the symlink.
        assert_eq!(fs.readlink(link).unwrap(), BString::from("src/main.rs"));

        // read on a directory fails clearly.
        assert_eq!(fs.read(dir, 0, 1).unwrap_err(), FsError::NotAFile);
    }

    #[test]
    fn inmem_fs_readdir_pagination() {
        let fs = InMemoryFsProvider::new();
        for i in 0..5 {
            let name = format!("f{i}");
            fs.add_file(ROOT_INODE, name.as_bytes(), vec![i as u8]);
        }
        let all = fs.readdir(ROOT_INODE, 0).unwrap();
        assert_eq!(all.len(), 5);

        // Skip the first 3.
        let rest = fs.readdir(ROOT_INODE, 3).unwrap();
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].name, all[3].name);
        assert_eq!(rest[1].name, all[4].name);
    }
}
