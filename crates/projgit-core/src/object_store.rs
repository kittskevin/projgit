//! Read-only wrapper around `gix-odb`.
//!
//! Architectural rules from `docs/initial-plan.md` §5.3:
//!
//! - **Read-only.** No mutation methods. The Fetcher (Phase 2) is the
//!   only component that mutates the store, via gix's pack-receive
//!   APIs, with an explicit re-read here afterwards.
//! - **Projection-agnostic.** The store never knows which mount is
//!   asking. Hard invariant.
//! - **`MissingObject(oid)` error variant** is the single Fetcher hook;
//!   we raise it in preference to letting gix's nested error organization
//!   leak out.
//!
//! Caches:
//!
//! - **Parsed-tree LRU.** `read_tree` consults a small bounded LRU
//!   ([`crate::tree_cache`]) before walking the gix tree. Tree
//!   objects are immutable in the OID-keyed sense, so the cache
//!   never has to invalidate.
//! - **Small-blob LRU.** `read_blob` consults a byte-bounded LRU
//!   ([`crate::blob_cache`]) so warm `cat` calls of source-sized
//!   files don't re-decode through gix. Skips blobs above a
//!   per-entry size threshold.
//! - **Header LRU.** `header` consults a small bounded LRU
//!   ([`crate::header_cache`]) so warm `lookup`s and the
//!   `readdir`-time prefetch worker don't repeatedly re-decode
//!   the same `(kind, size)` tuple via gix.

use crate::blob_cache::{
    BlobCache, BlobCacheStats, DEFAULT_CAPACITY_BYTES, DEFAULT_PER_ENTRY_MAX_BYTES,
};
use crate::error::ObjectStoreError;
use crate::header_cache::{
    HeaderCache, HeaderCacheStats, DEFAULT_CAPACITY as DEFAULT_HEADER_CAPACITY,
};
use crate::tree_cache::{TreeCache, TreeCacheStats, DEFAULT_CAPACITY};
use bstr::BString;
use gix::ObjectId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The kind of a git object, mirroring git's four object types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// File contents.
    Blob,
    /// Directory listing.
    Tree,
    /// Commit metadata.
    Commit,
    /// Annotated tag metadata.
    Tag,
}

impl ObjectKind {
    fn from_gix(kind: gix::object::Kind) -> Self {
        match kind {
            gix::object::Kind::Blob => Self::Blob,
            gix::object::Kind::Tree => Self::Tree,
            gix::object::Kind::Commit => Self::Commit,
            gix::object::Kind::Tag => Self::Tag,
        }
    }
}

/// Read-only handle to an on-disk git object store.
///
/// Holds a `gix::ThreadSafeRepository` so it is `Send + Sync` and can
/// back many concurrent readers (and a single Fetcher) at once, per
/// the `docs/initial-plan.md` §5.3 architectural rule. Each method
/// produces a cheap per-call thread-local `gix::Repository` handle
/// for the actual lookup.
#[derive(Debug)]
pub struct ObjectStore {
    repo: gix::ThreadSafeRepository,
    git_dir: PathBuf,
    tree_cache: TreeCache,
    blob_cache: BlobCache,
    header_cache: HeaderCache,
}

impl ObjectStore {
    /// Open an existing git directory.
    ///
    /// `git_dir` may be either a `.git` directory or a bare repository
    /// root; gix figures it out.
    pub fn open(git_dir: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        let path = git_dir.as_ref();
        let repo = gix::open(path).map_err(|source| ObjectStoreError::Open {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        let git_dir = repo.git_dir().to_path_buf();
        Ok(Self {
            repo: repo.into_sync(),
            git_dir,
            tree_cache: TreeCache::with_capacity(DEFAULT_CAPACITY),
            blob_cache: BlobCache::new(DEFAULT_CAPACITY_BYTES, DEFAULT_PER_ENTRY_MAX_BYTES),
            header_cache: HeaderCache::with_capacity(DEFAULT_HEADER_CAPACITY),
        })
    }

    /// Internal: cheap per-call thread-local handle for hot-path reads.
    fn handle(&self) -> gix::Repository {
        self.repo.to_thread_local()
    }

    /// Per-call thread-local `gix::Repository` handle sharing the
    /// store's underlying odb snapshot.
    ///
    /// Exposed for the [`crate::Fetcher`] implementations that need
    /// to drive gix's `Remote` lifecycle against the same repo the
    /// store reads from. Using the same [`gix::ThreadSafeRepository`]
    /// for both fetch and read is what makes a freshly-written pack
    /// visible to subsequent `read_blob` / `read_tree` calls without
    /// re-opening the store.
    pub fn handle_for_fetch(&self) -> gix::Repository {
        self.handle()
    }

    /// Path of the underlying `.git` directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Cheap presence check.
    ///
    /// Returns `true` if the object is currently in the local store.
    /// Does **not** trigger any network activity even if the store has
    /// a promisor remote configured.
    pub fn contains(&self, oid: ObjectId) -> bool {
        self.handle()
            .try_find_object(oid)
            .map(|o| o.is_some())
            .unwrap_or(false)
    }

    /// Return the object's kind and uncompressed size, or
    /// `MissingObject` if absent.
    ///
    /// Backed by an internal LRU ([`crate::header_cache`]). Cold
    /// reads decode the header via gix once and publish to the
    /// cache; warm reads return the cached `(kind, size)` tuple
    /// without touching gix.
    pub fn header(&self, oid: ObjectId) -> Result<(ObjectKind, u64), ObjectStoreError> {
        if let Some((kind, size)) = self.header_cache.get(&oid) {
            return Ok((kind, size));
        }
        self.header_cache.record_miss();

        let h = self.handle();
        let header = h
            .try_find_header(oid)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
            .ok_or(ObjectStoreError::MissingObject(oid))?;
        let kind = ObjectKind::from_gix(header.kind());
        let size = header.size();
        self.header_cache.put(oid, kind, size);
        Ok((kind, size))
    }

    /// Populate the header cache directly without going through
    /// gix decode. Used by the prefetch worker when an upstream
    /// `git cat-file --batch-check` query returns the header for
    /// an OID in one round trip.
    ///
    /// No-op effects on the underlying gix store; this only
    /// touches the in-process cache. Safe to call concurrently
    /// with reads.
    pub fn put_header_cache(&self, oid: ObjectId, kind: ObjectKind, size: u64) {
        self.header_cache.put(oid, kind, size);
    }

    /// Read the raw bytes of a blob. Returns `MissingObject` if absent
    /// or `UnexpectedKind` if the OID names a non-blob.
    ///
    /// Backed by an internal byte-bounded LRU
    /// ([`crate::blob_cache`]). Cold reads decode the blob once and
    /// publish a clone to the cache; warm reads return a clone of
    /// the cached bytes without touching gix. Blobs over the cache's
    /// per-entry size cap (default 64 KiB) are served straight from
    /// gix on every read.
    pub fn read_blob(&self, oid: ObjectId) -> Result<Vec<u8>, ObjectStoreError> {
        if let Some(cached) = self.blob_cache.get(&oid) {
            return Ok((*cached).clone());
        }
        self.blob_cache.record_miss();

        let h = self.handle();
        let obj = h
            .try_find_object(oid)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
            .ok_or(ObjectStoreError::MissingObject(oid))?;
        let actual = ObjectKind::from_gix(obj.kind);
        if actual != ObjectKind::Blob {
            return Err(ObjectStoreError::UnexpectedKind {
                oid,
                expected: ObjectKind::Blob,
                actual,
            });
        }
        let bytes = obj.data.clone();
        // Publish to the cache after a successful read. The cache
        // skips entries above its per-entry cap internally, so we
        // don't need to gate the put here.
        self.blob_cache.put(oid, Arc::new(bytes.clone()));
        Ok(bytes)
    }

    /// Read and parse a tree's entries, returning them in the order
    /// gix yields them (which matches git's storage order: byte-wise
    /// path comparison with directories sorted as `name + '/'`).
    ///
    /// Returns `MissingObject` if absent or `UnexpectedKind` if the
    /// OID names a non-tree.
    ///
    /// Backed by an internal LRU keyed by tree OID. Cold reads parse
    /// the tree once and clone the resulting `Vec` for the caller;
    /// warm reads return a clone of the cached `Vec` without
    /// touching gix at all. The cache is invisible to consumers.
    pub fn read_tree(&self, oid: ObjectId) -> Result<Vec<RawTreeEntry>, ObjectStoreError> {
        if let Some(cached) = self.tree_cache.get(&oid) {
            return Ok((*cached).clone());
        }
        self.tree_cache.record_miss();

        let h = self.handle();
        let obj = h
            .try_find_object(oid)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
            .ok_or(ObjectStoreError::MissingObject(oid))?;
        let actual = ObjectKind::from_gix(obj.kind);
        if actual != ObjectKind::Tree {
            return Err(ObjectStoreError::UnexpectedKind {
                oid,
                expected: ObjectKind::Tree,
                actual,
            });
        }
        let tree = obj.into_tree();
        let mut out = Vec::with_capacity(8);
        for entry in tree.iter() {
            let entry = entry.map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
            out.push(RawTreeEntry {
                name: entry.filename().to_owned(),
                mode_raw: entry.mode().kind() as u16,
                oid: entry.oid().to_owned(),
            });
        }
        // Publish to cache after a successful parse. Cloning the Arc
        // contents on subsequent hits is far cheaper than re-parsing.
        let arc = Arc::new(out.clone());
        self.tree_cache.put(oid, arc);
        Ok(out)
    }

    /// Snapshot of the parsed-tree LRU's counters.
    ///
    /// Useful for tests, diagnostics, and future metrics. Resetting
    /// the cache or its counters is intentionally not supported on
    /// the public API; callers that need a clean slate can build a
    /// fresh `ObjectStore`.
    pub fn tree_cache_stats(&self) -> TreeCacheStats {
        self.tree_cache.stats()
    }

    /// Snapshot of the small-blob LRU's counters. See
    /// [`Self::tree_cache_stats`] for caveats.
    pub fn blob_cache_stats(&self) -> BlobCacheStats {
        self.blob_cache.stats()
    }

    /// Snapshot of the header LRU's counters. See
    /// [`Self::tree_cache_stats`] for caveats.
    pub fn header_cache_stats(&self) -> HeaderCacheStats {
        self.header_cache.stats()
    }

    /// Return the committer timestamp of a commit as a [`SystemTime`].
    ///
    /// Uses *committer* time (not author time) because that's what
    /// `git log` sorts by and what users mean by "when did this land".
    /// Projections expose this as the `mtime` of every file they
    /// surface; per-file mtime is not stored in git tree entries.
    ///
    /// Returns [`ObjectStoreError::UnexpectedKind`] if `oid` does not
    /// name a commit, or [`ObjectStoreError::MissingObject`] if absent.
    pub fn commit_time(&self, oid: ObjectId) -> Result<SystemTime, ObjectStoreError> {
        let h = self.handle();
        let obj = h
            .try_find_object(oid)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
            .ok_or(ObjectStoreError::MissingObject(oid))?;
        let actual = ObjectKind::from_gix(obj.kind);
        if actual != ObjectKind::Commit {
            return Err(ObjectStoreError::UnexpectedKind {
                oid,
                expected: ObjectKind::Commit,
                actual,
            });
        }
        // Decode the raw bytes directly; gix's high-level `Commit` API
        // moves around between versions, but `CommitRef::from_bytes`
        // is stable.
        let commit_ref = gix::objs::CommitRef::from_bytes(&obj.data)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        let secs = commit_ref.committer.time.seconds;
        let st = if secs >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
        } else {
            SystemTime::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
        };
        Ok(st)
    }

    /// Resolve a commit OID to its top-level tree OID.
    pub fn commit_tree(&self, oid: ObjectId) -> Result<ObjectId, ObjectStoreError> {
        let h = self.handle();
        let obj = h
            .try_find_object(oid)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
            .ok_or(ObjectStoreError::MissingObject(oid))?;
        let actual = ObjectKind::from_gix(obj.kind);
        if actual != ObjectKind::Commit {
            return Err(ObjectStoreError::UnexpectedKind {
                oid,
                expected: ObjectKind::Commit,
                actual,
            });
        }
        let commit = obj
            .try_into_commit()
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        let tree_id = commit
            .tree_id()
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        Ok(tree_id.detach())
    }

    /// Resolve a ref name (e.g. `"refs/heads/main"` or short `"main"`)
    /// to the commit OID it currently points at.
    ///
    /// This walks symbolic refs (e.g. `HEAD` → `refs/heads/main`) but
    /// does **not** dereference annotated tags; that is a separate
    /// projection-engine concern.
    pub fn resolve_ref(&self, refname: &str) -> Result<ObjectId, ObjectStoreError> {
        let h = self.handle();
        let mut reference = h
            .find_reference(refname)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        let id = reference
            .peel_to_id_in_place()
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        Ok(id.detach())
    }
}

/// One entry in a parsed tree.
///
/// `mode_raw` is git's raw mode field as a `u16` so consumers can do
/// their own classification (regular file, exec file, symlink, dir,
/// gitlink) without re-deriving it from a more abstract enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTreeEntry {
    /// Entry name (one path component, no slashes). Git stores names
    /// as bytes, not UTF-8.
    pub name: BString,
    /// Raw git mode (e.g. `0o100644`, `0o100755`, `0o120000`,
    /// `0o040000`, `0o160000`).
    pub mode_raw: u16,
    /// OID of the referenced object (blob OID for files / symlinks,
    /// tree OID for directories, commit OID for gitlinks).
    pub oid: ObjectId,
}
