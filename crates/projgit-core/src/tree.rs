//! `TreeNavigator` walks git tree objects to resolve `/`-separated
//! virtual paths inside a commit's tree.
//!
//! The navigator is **stateless** — it holds a borrow on the
//! [`crate::ObjectStore`] and computes everything from the OIDs it
//! receives. State (caches, projection metadata) lives in higher
//! layers.

use crate::error::{ObjectStoreError, ProjectionError};
use crate::object_store::{ObjectStore, RawTreeEntry};
use bstr::BString;
use gix::ObjectId;

/// A high-level classification of a git tree entry's mode.
///
/// Mirrors the five forms that appear in real-world git trees. Holds
/// the raw mode too so the FS frontends can preserve the executable
/// bit on POSIX without us re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMode {
    /// `100644` — regular file.
    RegularFile,
    /// `100755` — regular file with the executable bit set.
    ExecutableFile,
    /// `120000` — symlink. Blob content is the link target string.
    Symlink,
    /// `040000` — directory (its OID is a tree).
    Directory,
    /// `160000` — gitlink (submodule). Its OID is a commit in another repo.
    Gitlink,
}

impl EntryMode {
    /// Classify a raw git mode. Unknown values fall back to
    /// `RegularFile` so we never panic on malformed but readable trees.
    pub fn from_raw(mode_raw: u16) -> Self {
        match mode_raw {
            0o100644 => Self::RegularFile,
            0o100755 => Self::ExecutableFile,
            0o120000 => Self::Symlink,
            0o040000 => Self::Directory,
            0o160000 => Self::Gitlink,
            _ => Self::RegularFile,
        }
    }

    /// Is this entry a directory we can descend into via `TreeNavigator`?
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// One entry in a directory listing as exposed to the projection layer.
///
/// Distinct from [`RawTreeEntry`] so the projection layer can mix
/// real-tree entries with synthetic [`crate::SyntheticEntry`] entries
/// without callers caring which is which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Entry name (one path component, no slashes).
    pub name: BString,
    /// Classified mode.
    pub mode: EntryMode,
    /// Raw git mode.
    pub mode_raw: u16,
    /// Referenced OID.
    pub oid: ObjectId,
}

impl From<RawTreeEntry> for TreeEntry {
    fn from(raw: RawTreeEntry) -> Self {
        Self {
            mode: EntryMode::from_raw(raw.mode_raw),
            mode_raw: raw.mode_raw,
            name: raw.name,
            oid: raw.oid,
        }
    }
}

/// Navigates a commit's tree, resolving `/`-separated paths into
/// individual [`TreeEntry`] values.
pub struct TreeNavigator<'a> {
    store: &'a ObjectStore,
}

impl<'a> TreeNavigator<'a> {
    /// Bind a navigator to an object store.
    pub fn new(store: &'a ObjectStore) -> Self {
        Self { store }
    }

    /// List the entries of the tree at `tree_oid`.
    pub fn list(&self, tree_oid: ObjectId) -> Result<Vec<TreeEntry>, ObjectStoreError> {
        Ok(self
            .store
            .read_tree(tree_oid)?
            .into_iter()
            .map(TreeEntry::from)
            .collect())
    }

    /// Resolve `/`-separated `path` (relative to `tree_oid`) into a
    /// single `TreeEntry`.
    ///
    /// Empty path resolves to a synthetic directory entry naming the
    /// tree itself; this is what the projection root resolves to.
    pub fn lookup(
        &self,
        tree_oid: ObjectId,
        path: &str,
    ) -> Result<TreeEntry, ProjectionError> {
        let components = split_path(path)?;
        if components.is_empty() {
            return Ok(TreeEntry {
                name: BString::default(),
                mode: EntryMode::Directory,
                mode_raw: 0o040000,
                oid: tree_oid,
            });
        }

        let mut current_tree = tree_oid;
        let mut walked = String::new();
        for (i, component) in components.iter().enumerate() {
            let entries = self.store.read_tree(current_tree)?;
            let entry = entries
                .into_iter()
                .find(|e| e.name == component.as_bytes())
                .ok_or_else(|| ProjectionError::NotFound {
                    component: (*component).to_owned(),
                    parent: walked.clone(),
                })?;

            let is_last = i + 1 == components.len();
            let entry: TreeEntry = entry.into();
            if is_last {
                return Ok(entry);
            }

            if !entry.mode.is_dir() {
                return Err(ProjectionError::NotADirectory {
                    component: (*component).to_owned(),
                    parent: walked.clone(),
                });
            }
            current_tree = entry.oid;
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(component);
        }
        unreachable!("loop returns on the last component")
    }
}

/// Split a `/`-separated path into its non-empty components, rejecting
/// `.`, `..`, and embedded null bytes.
fn split_path(path: &str) -> Result<Vec<&str>, ProjectionError> {
    if path.contains('\0') {
        return Err(ProjectionError::InvalidPath {
            path: path.to_owned(),
            reason: "null byte in path",
        });
    }
    let mut out = Vec::new();
    for component in path.split('/') {
        if component.is_empty() {
            // Allow leading/trailing/duplicate slashes; just skip.
            continue;
        }
        if component == "." || component == ".." {
            return Err(ProjectionError::InvalidPath {
                path: path.to_owned(),
                reason: "'.' and '..' components are not allowed",
            });
        }
        out.push(component);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_path_basic() {
        assert_eq!(split_path("").unwrap(), Vec::<&str>::new());
        assert_eq!(split_path("/").unwrap(), Vec::<&str>::new());
        assert_eq!(split_path("a").unwrap(), vec!["a"]);
        assert_eq!(split_path("a/b/c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(split_path("/a/b/").unwrap(), vec!["a", "b"]);
        assert_eq!(split_path("a//b").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn split_path_rejects_dot_and_dotdot() {
        assert!(matches!(
            split_path("a/./b"),
            Err(ProjectionError::InvalidPath { .. })
        ));
        assert!(matches!(
            split_path("../etc"),
            Err(ProjectionError::InvalidPath { .. })
        ));
    }

    #[test]
    fn split_path_rejects_null_byte() {
        assert!(matches!(
            split_path("a\0b"),
            Err(ProjectionError::InvalidPath { .. })
        ));
    }

    #[test]
    fn entry_mode_classification() {
        assert_eq!(EntryMode::from_raw(0o100644), EntryMode::RegularFile);
        assert_eq!(EntryMode::from_raw(0o100755), EntryMode::ExecutableFile);
        assert_eq!(EntryMode::from_raw(0o120000), EntryMode::Symlink);
        assert_eq!(EntryMode::from_raw(0o040000), EntryMode::Directory);
        assert_eq!(EntryMode::from_raw(0o160000), EntryMode::Gitlink);
        // Unknown -> regular file (fallback).
        assert_eq!(EntryMode::from_raw(0o100600), EntryMode::RegularFile);

        assert!(EntryMode::Directory.is_dir());
        assert!(!EntryMode::RegularFile.is_dir());
        assert!(!EntryMode::Symlink.is_dir());
    }
}
