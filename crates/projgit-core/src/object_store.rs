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
//! Caches (LRU for parsed trees, LRU for small blobs ≤ 64 KB) and
//! many-readers `Repository`-handle plumbing arrive in Phase 5; the
//! Phase 1 implementation calls `gix` directly for clarity.

use crate::error::ObjectStoreError;
use bstr::BString;
use gix::ObjectId;
use std::path::{Path, PathBuf};

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
/// Cheap to clone in spirit: the inner `gix::Repository` shares its
/// underlying `gix-odb` state via `Arc`, so multiple `ObjectStore`
/// values backed by the same git directory cooperate correctly.
#[derive(Debug)]
pub struct ObjectStore {
    repo: gix::Repository,
    git_dir: PathBuf,
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
        Ok(Self {
            git_dir: repo.git_dir().to_path_buf(),
            repo,
        })
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
        self.repo.try_find_object(oid).map(|o| o.is_some()).unwrap_or(false)
    }

    /// Return the object's kind and uncompressed size, or
    /// `MissingObject` if absent.
    pub fn header(&self, oid: ObjectId) -> Result<(ObjectKind, u64), ObjectStoreError> {
        let header = self
            .repo
            .try_find_header(oid)
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?
            .ok_or(ObjectStoreError::MissingObject(oid))?;
        Ok((ObjectKind::from_gix(header.kind()), header.size()))
    }

    /// Read the raw bytes of a blob. Returns `MissingObject` if absent
    /// or `UnexpectedKind` if the OID names a non-blob.
    pub fn read_blob(&self, oid: ObjectId) -> Result<Vec<u8>, ObjectStoreError> {
        let obj = self
            .repo
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
        Ok(obj.data.clone())
    }

    /// Read and parse a tree's entries, returning them in the order
    /// gix yields them (which matches git's storage order: byte-wise
    /// path comparison with directories sorted as `name + '/'`).
    ///
    /// Returns `MissingObject` if absent or `UnexpectedKind` if the
    /// OID names a non-tree.
    pub fn read_tree(&self, oid: ObjectId) -> Result<Vec<RawTreeEntry>, ObjectStoreError> {
        let obj = self
            .repo
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
        Ok(out)
    }

    /// Resolve a commit OID to its top-level tree OID.
    pub fn commit_tree(&self, oid: ObjectId) -> Result<ObjectId, ObjectStoreError> {
        let obj = self
            .repo
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
        let mut reference = self
            .repo
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
