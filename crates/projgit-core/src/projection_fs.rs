//! `ProjectionFsProvider` — bridges [`crate::Projection`] +
//! [`crate::HydratingObjectStore`] + [`crate::RootOverlay`] to the
//! OS-agnostic [`crate::FsProvider`] trait.
//!
//! This is the glue that lets the FUSE / WinFsp backends mount real
//! git data (instead of just the [`crate::InMemoryFsProvider`] test
//! fixture). It is pure logic — no platform code — and is generic
//! over [`crate::Fetcher`] so tests can use [`crate::NoopFetcher`]
//! while runtime mounts use [`crate::GixFetcher`].
//!
//! ## Cache shape
//!
//! [`crate::InodeAllocator`] memoises `inode → (oid, path)` but does
//! not record file kind, mode, size, or per-entry content. To answer
//! `getattr` / `read` / `readlink` without re-walking the projection
//! we maintain a per-inode `AttrSnapshot` cache populated lazily from
//! `lookup` / `readdir`. The cache is purely for speed and never
//! becomes the source of truth — it stores OIDs and synthetic
//! references, never decoded blob bytes.
//!
//! ## `getattr` on uncached inodes
//!
//! Returns [`FsError::NotFound`]. Both FUSE and WinFsp guarantee an
//! entry has been `lookup`ed (or is the root) before its inode flows
//! into a `getattr`, so this matches the platform contract while
//! keeping the cache shape simple. If a future backend needs
//! out-of-band `getattr` it can call [`Self::lookup`] first.

use crate::error::ProjectionError;
use crate::fetcher::{Fetcher, HydrateError, HydratingObjectStore};
use crate::fs_provider::{
    Attr, DirEntry, FileType, FsError, FsProvider, InodeAllocator, ROOT_INODE,
};
use crate::overlay::{RootOverlay, SyntheticEntry};
use crate::prefetch::{PrefetchHandle, PrefetchStats};
use crate::projection::{Projection, ResolvedEntry};
use crate::tree::{EntryMode, TreeEntry, TreeNavigator};
use bstr::BString;
use gix::ObjectId;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

/// Per-inode metadata cached during `lookup` / `readdir` so subsequent
/// `getattr` / `read` / `readlink` calls don't re-walk the projection.
///
/// Each variant carries everything needed to answer the read-side
/// operations for one entry. Decoded blob bytes are deliberately not
/// cached here — that's a Phase 5 concern.
#[derive(Debug, Clone)]
enum AttrSnapshot {
    /// The mount root.
    Root,
    /// A real git tree entry that's a regular or executable file.
    /// Carries the blob OID so `read` can hydrate on demand.
    TreeFile { oid: ObjectId, size: u64, mode: u16 },
    /// A real git tree directory. Carries the tree OID so `readdir`
    /// can list children.
    TreeDir {
        tree_oid: ObjectId,
        /// Virtual path from the projection root (no leading slash).
        path: BString,
    },
    /// A real git symlink. Target lives in a blob, hydrated lazily.
    TreeSymlink { oid: ObjectId, size: u64 },
    /// A submodule (gitlink). Renders as an empty directory per the
    /// MVP locked decision.
    Gitlink,
    /// A synthetic file from the [`RootOverlay`].
    SyntheticFile { content: Arc<Vec<u8>>, mode: u16 },
    /// A synthetic directory from the [`RootOverlay`]. Children are
    /// shared via [`Arc`] so cloning the snapshot is cheap.
    SyntheticDir {
        children: Arc<BTreeMap<BString, SyntheticEntry>>,
        /// Virtual path from the projection root.
        path: BString,
    },
    /// A synthetic symlink from the [`RootOverlay`].
    SyntheticSymlink { target: BString },
}

/// A read-only [`FsProvider`] backed by a [`Projection`] over a
/// [`HydratingObjectStore`], with synthetic entries from a
/// [`RootOverlay`] spliced in at the projection root.
pub struct ProjectionFsProvider<F: Fetcher + 'static> {
    projection: Projection,
    store: Arc<HydratingObjectStore<F>>,
    overlay: RootOverlay,
    allocator: InodeAllocator,
    commit_time: SystemTime,
    attrs: RwLock<HashMap<u64, AttrSnapshot>>,
    /// Background T1 prefetch worker (see `crate::prefetch`).
    /// Spawned at construction; joined on drop.
    prefetch: PrefetchHandle,
}

impl<F: Fetcher + 'static> ProjectionFsProvider<F> {
    /// Construct a new provider.
    ///
    /// `projection_id` is the per-mount namespace passed to
    /// [`InodeAllocator::new`] — a future mount manager (Phase 4)
    /// owns the policy for assigning IDs across simultaneous mounts.
    ///
    /// Resolves the projection's commit OID up front to seed the
    /// `mtime` exposed by every entry. Errors are bubbled up from
    /// the projection / object-store layers.
    ///
    /// Spawns the T1 prefetch worker bound to `store`. The worker
    /// runs for the lifetime of the provider and is joined on drop.
    pub fn new(
        projection: Projection,
        store: Arc<HydratingObjectStore<F>>,
        overlay: RootOverlay,
        projection_id: u64,
    ) -> Result<Self, ProjectionError> {
        let commit_oid = resolve_commit_oid(&projection, store.store())?;
        let commit_time = store
            .store()
            .commit_time(commit_oid)
            .map_err(ProjectionError::from)?;

        let allocator = InodeAllocator::new(projection_id);
        let mut attrs = HashMap::new();
        attrs.insert(ROOT_INODE, AttrSnapshot::Root);

        let prefetch = PrefetchHandle::spawn(store.clone());

        Ok(Self {
            projection,
            store,
            overlay,
            allocator,
            commit_time,
            attrs: RwLock::new(attrs),
            prefetch,
        })
    }

    /// Access the underlying [`HydratingObjectStore`].
    pub fn store(&self) -> &HydratingObjectStore<F> {
        &self.store
    }

    /// Access the inode allocator (for diagnostics / tests).
    pub fn allocator(&self) -> &InodeAllocator {
        &self.allocator
    }

    /// Commit time exposed as `mtime` on every entry.
    pub fn commit_time(&self) -> SystemTime {
        self.commit_time
    }

    /// Snapshot of the prefetch worker's counters.
    pub fn prefetch_stats(&self) -> PrefetchStats {
        self.prefetch.stats()
    }

    // -- internals --

    /// Recover the virtual path for a known inode. Returns `None` if
    /// the inode names a file (no children) or is unknown.
    fn parent_path(&self, parent: u64) -> Result<BString, FsError> {
        if parent == ROOT_INODE {
            return Ok(BString::default());
        }
        let attrs = self.attrs.read().unwrap();
        match attrs.get(&parent) {
            Some(AttrSnapshot::TreeDir { path, .. })
            | Some(AttrSnapshot::SyntheticDir { path, .. }) => Ok(path.clone()),
            Some(AttrSnapshot::Gitlink) => {
                // A gitlink is a "directory" with no children. Treat
                // any lookup inside it as not-found.
                Err(FsError::NotFound)
            }
            Some(_) => Err(FsError::NotADirectory),
            None => Err(FsError::NotFound),
        }
    }

    /// Build the joined `/`-separated path for `lookup(parent, name)`.
    fn join_path(parent_path: &BString, name: &[u8]) -> String {
        // FsProvider's `name` is a single component. Project's
        // `lookup` takes a `&str`; tree-entry names in git are bytes
        // but in practice are UTF-8 for the paths we surface. Lossy
        // decode keeps the API working when they're not.
        let name_str = String::from_utf8_lossy(name);
        if parent_path.is_empty() {
            name_str.into_owned()
        } else {
            let parent_str = String::from_utf8_lossy(parent_path);
            format!("{parent_str}/{name_str}")
        }
    }

    /// Lightweight `readdir` companion: allocate the inode and pick
    /// the [`FileType`] for `resolved`, **without** consulting the
    /// object store.
    ///
    /// This deliberately avoids `ObjectStore::header` (the only thing
    /// `snapshot_for` needs the store for, to fetch a blob's size)
    /// because `readdir` does not need sizes — fuser only emits
    /// `inode + kind + name` from a `DirEntry`. Hydrating every blob
    /// just to list a directory would defeat the partial-clone story
    /// for URL-backed mounts: a single `ls` would force a fetch of
    /// every file's blob.
    ///
    /// The kernel guarantees a follow-up `lookup` for any entry the
    /// user actually `stat`s; that's where [`Self::snapshot_for`]
    /// runs and resolves the real size.
    fn dir_entry_for(&self, resolved: &ResolvedEntry, full_path: &str) -> (u64, FileType) {
        let path_bytes = full_path.as_bytes();
        match resolved {
            ResolvedEntry::Tree(entry) => {
                let inode = self.allocator.for_tree_entry(entry.oid, path_bytes);
                let kind = match entry.mode {
                    EntryMode::RegularFile | EntryMode::ExecutableFile => FileType::RegularFile,
                    EntryMode::Directory | EntryMode::Gitlink => FileType::Directory,
                    EntryMode::Symlink => FileType::Symlink,
                };
                (inode, kind)
            }
            ResolvedEntry::Synthetic { entry, .. } => {
                let inode = self.allocator.for_synthetic(path_bytes);
                let kind = match entry {
                    SyntheticEntry::File { .. } => FileType::RegularFile,
                    SyntheticEntry::Directory { .. } => FileType::Directory,
                    SyntheticEntry::Symlink { .. } => FileType::Symlink,
                };
                (inode, kind)
            }
        }
    }

    /// Convert a [`ResolvedEntry`] into `(inode, AttrSnapshot, FileType, size, mode)`.
    /// Allocates an inode for the entry and returns the freshly-built
    /// snapshot so the caller can decide whether to also publish it
    /// to the cache.
    fn snapshot_for(
        &self,
        resolved: ResolvedEntry,
        full_path: &str,
    ) -> Result<(u64, AttrSnapshot, FileType, u64, u16), FsError> {
        match resolved {
            ResolvedEntry::Tree(entry) => self.snapshot_for_tree(entry, full_path),
            ResolvedEntry::Synthetic { entry, .. } => {
                Ok(self.snapshot_for_synthetic(entry, full_path))
            }
        }
    }

    fn snapshot_for_tree(
        &self,
        entry: TreeEntry,
        full_path: &str,
    ) -> Result<(u64, AttrSnapshot, FileType, u64, u16), FsError> {
        let path_bytes = full_path.as_bytes();
        let inode = self.allocator.for_tree_entry(entry.oid, path_bytes);
        match entry.mode {
            EntryMode::RegularFile | EntryMode::ExecutableFile => {
                let mode: u16 = if entry.mode == EntryMode::ExecutableFile {
                    0o755
                } else {
                    0o644
                };
                let (_, size) = self.store.header(entry.oid).map_err(hydrate_to_fs)?;
                Ok((
                    inode,
                    AttrSnapshot::TreeFile {
                        oid: entry.oid,
                        size,
                        mode,
                    },
                    FileType::RegularFile,
                    size,
                    mode,
                ))
            }
            EntryMode::Directory => Ok((
                inode,
                AttrSnapshot::TreeDir {
                    tree_oid: entry.oid,
                    path: BString::from(path_bytes),
                },
                FileType::Directory,
                0,
                0o755,
            )),
            EntryMode::Symlink => {
                let (_, size) = self.store.header(entry.oid).map_err(hydrate_to_fs)?;
                Ok((
                    inode,
                    AttrSnapshot::TreeSymlink {
                        oid: entry.oid,
                        size,
                    },
                    FileType::Symlink,
                    size,
                    0o777,
                ))
            }
            EntryMode::Gitlink => Ok((inode, AttrSnapshot::Gitlink, FileType::Directory, 0, 0o755)),
        }
    }

    fn snapshot_for_synthetic(
        &self,
        entry: SyntheticEntry,
        full_path: &str,
    ) -> (u64, AttrSnapshot, FileType, u64, u16) {
        let path_bytes = full_path.as_bytes();
        let inode = self.allocator.for_synthetic(path_bytes);
        match entry {
            SyntheticEntry::File { content, mode_raw } => {
                let mode: u16 = if mode_raw == 0o100755 { 0o755 } else { 0o644 };
                let size = content.len() as u64;
                (
                    inode,
                    AttrSnapshot::SyntheticFile {
                        content: Arc::new(content),
                        mode,
                    },
                    FileType::RegularFile,
                    size,
                    mode,
                )
            }
            SyntheticEntry::Directory { children } => (
                inode,
                AttrSnapshot::SyntheticDir {
                    children: Arc::new(children),
                    path: BString::from(path_bytes),
                },
                FileType::Directory,
                0,
                0o755,
            ),
            SyntheticEntry::Symlink { target } => {
                let size = target.len() as u64;
                (
                    inode,
                    AttrSnapshot::SyntheticSymlink {
                        target: target.clone(),
                    },
                    FileType::Symlink,
                    size,
                    0o777,
                )
            }
        }
    }

    /// Build an `Attr` from snapshot pieces, stamping `commit_time`.
    fn attr(&self, inode: u64, kind: FileType, size: u64, mode: u16) -> Attr {
        let mut a = match kind {
            FileType::RegularFile => Attr::regular_file(inode, size, mode),
            FileType::Directory => Attr::directory(inode),
            FileType::Symlink => Attr::symlink(inode, size),
        };
        a.mtime = self.commit_time;
        a
    }

    fn publish(&self, inode: u64, snap: AttrSnapshot) {
        let mut m = self.attrs.write().unwrap();
        m.entry(inode).or_insert(snap);
    }
}

impl<F: Fetcher + 'static> FsProvider for ProjectionFsProvider<F> {
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<Attr, FsError> {
        let parent_path = self.parent_path(parent)?;
        let full = Self::join_path(&parent_path, name);
        let resolved = self
            .projection
            .lookup(self.store.store(), &self.overlay, &full)
            .map_err(projection_to_fs)?;
        let (inode, snap, kind, size, mode) = self.snapshot_for(resolved, &full)?;
        self.publish(inode, snap);
        Ok(self.attr(inode, kind, size, mode))
    }

    fn getattr(&self, inode: u64) -> Result<Attr, FsError> {
        if inode == ROOT_INODE {
            return Ok(self.attr(ROOT_INODE, FileType::Directory, 0, 0o755));
        }
        let attrs = self.attrs.read().unwrap();
        let snap = attrs.get(&inode).ok_or(FsError::NotFound)?;
        let (kind, size, mode) = match snap {
            AttrSnapshot::Root => (FileType::Directory, 0, 0o755),
            AttrSnapshot::TreeFile { size, mode, .. } => (FileType::RegularFile, *size, *mode),
            AttrSnapshot::TreeDir { .. } => (FileType::Directory, 0, 0o755),
            AttrSnapshot::TreeSymlink { size, .. } => (FileType::Symlink, *size, 0o777),
            AttrSnapshot::Gitlink => (FileType::Directory, 0, 0o755),
            AttrSnapshot::SyntheticFile { content, mode } => {
                (FileType::RegularFile, content.len() as u64, *mode)
            }
            AttrSnapshot::SyntheticDir { .. } => (FileType::Directory, 0, 0o755),
            AttrSnapshot::SyntheticSymlink { target } => {
                (FileType::Symlink, target.len() as u64, 0o777)
            }
        };
        Ok(self.attr(inode, kind, size, mode))
    }

    fn readdir(&self, inode: u64, offset: u64) -> Result<Vec<DirEntry>, FsError> {
        // Simple-contract pagination (matches `InMemoryFsProvider`).
        if offset > 0 {
            return Ok(Vec::new());
        }

        // Determine the listing source up front. We deliberately do
        // NOT publish AttrSnapshots from `readdir`: doing so would
        // require resolving each file's blob size via `header()`,
        // which on a partial clone hydrates every blob just to `ls`.
        // The kernel guarantees a follow-up `lookup` for any entry
        // the user actually `stat`s, and `lookup` populates the cache
        // with real sizes/modes lazily.
        //
        // T1 prefetch: we *do* post the directory's blob/symlink OIDs
        // to the background prefetch worker after returning. The
        // worker will batch them into a single
        // `git cat-file --batch-check` round trip and warm the
        // header cache so the kernel's follow-up `lookup`s are fast.
        if inode == ROOT_INODE {
            let pairs = self
                .projection
                .read_root(self.store.store(), &self.overlay)
                .map_err(projection_to_fs)?;
            let mut out = Vec::with_capacity(pairs.len());
            let mut prefetch_oids: Vec<gix::ObjectId> = Vec::with_capacity(pairs.len());
            for (name, resolved) in pairs {
                // The `name` from `read_root` is already the entry's
                // own component name; full path == name at the root.
                let full = String::from_utf8_lossy(&name).into_owned();
                if let Some(oid) = harvest_prefetch_oid(&resolved) {
                    prefetch_oids.push(oid);
                }
                let (child_inode, kind) = self.dir_entry_for(&resolved, &full);
                out.push(DirEntry {
                    inode: child_inode,
                    name,
                    kind,
                });
            }
            self.prefetch.post_headers(prefetch_oids);
            return Ok(out);
        }

        // Non-root: snapshot must already be cached.
        let snap = {
            let attrs = self.attrs.read().unwrap();
            attrs.get(&inode).cloned().ok_or(FsError::NotFound)?
        };
        match snap {
            AttrSnapshot::TreeDir { tree_oid, path } => {
                let raw = self.store.read_tree(tree_oid).map_err(hydrate_to_fs)?;
                let entries: Vec<TreeEntry> = raw.into_iter().map(TreeEntry::from).collect();
                let mut out = Vec::with_capacity(entries.len());
                let mut prefetch_oids: Vec<gix::ObjectId> = Vec::with_capacity(entries.len());
                for entry in entries {
                    let full = if path.is_empty() {
                        String::from_utf8_lossy(&entry.name).into_owned()
                    } else {
                        format!(
                            "{}/{}",
                            String::from_utf8_lossy(&path),
                            String::from_utf8_lossy(&entry.name)
                        )
                    };
                    let name = entry.name.clone();
                    let resolved = ResolvedEntry::Tree(entry);
                    if let Some(oid) = harvest_prefetch_oid(&resolved) {
                        prefetch_oids.push(oid);
                    }
                    let (child_inode, kind) = self.dir_entry_for(&resolved, &full);
                    out.push(DirEntry {
                        inode: child_inode,
                        name,
                        kind,
                    });
                }
                self.prefetch.post_headers(prefetch_oids);
                Ok(out)
            }
            AttrSnapshot::SyntheticDir { children, path } => {
                let mut out = Vec::with_capacity(children.len());
                for (name, entry) in children.iter() {
                    let full = if path.is_empty() {
                        String::from_utf8_lossy(name).into_owned()
                    } else {
                        format!(
                            "{}/{}",
                            String::from_utf8_lossy(&path),
                            String::from_utf8_lossy(name)
                        )
                    };
                    let resolved = ResolvedEntry::Synthetic {
                        name: name.clone(),
                        entry: entry.clone(),
                    };
                    let (child_inode, kind) = self.dir_entry_for(&resolved, &full);
                    out.push(DirEntry {
                        inode: child_inode,
                        name: name.clone(),
                        kind,
                    });
                }
                Ok(out)
            }
            AttrSnapshot::Gitlink => {
                // Submodules render as empty directories per locked decision.
                Ok(Vec::new())
            }
            AttrSnapshot::Root => {
                // Unreachable: handled above. Keep exhaustive for safety.
                Ok(Vec::new())
            }
            _ => Err(FsError::NotADirectory),
        }
    }

    fn read(&self, inode: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let snap = {
            let attrs = self.attrs.read().unwrap();
            attrs.get(&inode).cloned().ok_or(FsError::NotFound)?
        };
        match snap {
            AttrSnapshot::TreeFile { oid, .. } => {
                let bytes = self.store.read_blob(oid).map_err(hydrate_to_fs)?;
                Ok(slice_blob(&bytes, offset, size))
            }
            AttrSnapshot::SyntheticFile { content, .. } => Ok(slice_blob(&content, offset, size)),
            _ => Err(FsError::NotAFile),
        }
    }

    fn readlink(&self, inode: u64) -> Result<BString, FsError> {
        let snap = {
            let attrs = self.attrs.read().unwrap();
            attrs.get(&inode).cloned().ok_or(FsError::NotFound)?
        };
        match snap {
            AttrSnapshot::TreeSymlink { oid, .. } => {
                let bytes = self.store.read_blob(oid).map_err(hydrate_to_fs)?;
                Ok(BString::from(bytes))
            }
            AttrSnapshot::SyntheticSymlink { target } => Ok(target),
            _ => Err(FsError::NotASymlink),
        }
    }
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

fn slice_blob(bytes: &[u8], offset: u64, size: u32) -> Vec<u8> {
    let start = (offset as usize).min(bytes.len());
    let end = start.saturating_add(size as usize).min(bytes.len());
    bytes[start..end].to_vec()
}

fn resolve_commit_oid(
    projection: &Projection,
    store: &crate::ObjectStore,
) -> Result<ObjectId, ProjectionError> {
    match projection {
        Projection::Ref(name) => store.resolve_ref(name).map_err(ProjectionError::from),
        Projection::Commit(oid) => Ok(*oid),
        Projection::Subtree { commit, .. } => Ok(*commit),
    }
}

fn projection_to_fs(e: ProjectionError) -> FsError {
    match e {
        ProjectionError::NotFound { .. } => FsError::NotFound,
        ProjectionError::NotADirectory { .. } => FsError::NotADirectory,
        // Defensive default: an invalid path has no entry to resolve to.
        ProjectionError::InvalidPath { .. } => FsError::NotFound,
        ProjectionError::Store(s) => FsError::Io(s.to_string()),
    }
}

fn hydrate_to_fs(e: HydrateError) -> FsError {
    FsError::Io(e.to_string())
}

/// Pull the blob OID worth prefetching from a `ResolvedEntry`.
///
/// Returns `Some(oid)` for entries the T1 prefetch worker can
/// usefully header-resolve via `cat-file --batch-check`:
/// regular files, executable files, and symlinks (their targets
/// live in a blob too, so a `lookup`'s `getattr` wants the size).
///
/// Returns `None` for:
///
/// - **Directories.** Their OID is a tree, not a blob; a future
///   T2 tier may prefetch tree contents recursively, but T1's
///   contract is "headers for blobs."
/// - **Submodules / gitlinks.** Their OID points at a commit in
///   another repository; we have no upstream that can serve it,
///   and our store renders submodules as empty directories
///   anyway. Posting them would just queue inevitable failures.
/// - **Synthetic root-overlay entries.** They live in memory; no
///   blob, no header, no upstream to ask.
fn harvest_prefetch_oid(resolved: &ResolvedEntry) -> Option<gix::ObjectId> {
    match resolved {
        ResolvedEntry::Tree(entry) => match entry.mode {
            EntryMode::RegularFile | EntryMode::ExecutableFile | EntryMode::Symlink => {
                Some(entry.oid)
            }
            EntryMode::Directory | EntryMode::Gitlink => None,
        },
        ResolvedEntry::Synthetic { .. } => None,
    }
}

// `TreeNavigator` is only used via `Projection`; the import keeps
// rustdoc links resolvable from the docstrings above.
#[allow(dead_code)]
fn _doc_anchor(_: TreeNavigator<'_>) {}
