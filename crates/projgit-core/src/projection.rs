//! `Projection` — the user-visible view of an [`crate::ObjectStore`].
//!
//! Three kinds, all read-only:
//!
//! - [`Projection::Ref`] — the tip of a named ref (e.g. `refs/heads/main`).
//! - [`Projection::Commit`] — a specific commit OID.
//! - [`Projection::Subtree`] — a subtree of either of the above.
//!
//! Resolution always consults the [`crate::RootOverlay`] **first** so
//! synthetic top-level entries shadow real tree entries. The overlay
//! is empty in MVP; the wiring is what matters (see
//! `docs/design/dotgit-synthesis.md` §10).

use crate::error::{ObjectStoreError, ProjectionError};
use crate::object_store::ObjectStore;
use crate::overlay::{RootOverlay, SyntheticEntry};
use crate::tree::{EntryMode, TreeEntry, TreeNavigator};
use bstr::BString;
use gix::ObjectId;

/// What a path resolves to. The projection layer hands one of these
/// back to FS frontends, which then convert it into the right OS
/// representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedEntry {
    /// A real git tree entry (file, dir, symlink, gitlink).
    Tree(TreeEntry),
    /// A synthetic entry from the root overlay.
    Synthetic {
        /// The path component name as resolved.
        name: BString,
        /// The full synthetic entry definition.
        entry: SyntheticEntry,
    },
}

/// A projection of an [`ObjectStore`]. Read-only, projection-agnostic,
/// composable with a [`RootOverlay`].
#[derive(Debug, Clone)]
pub enum Projection {
    /// Mount the tip of a named ref.
    Ref(String),
    /// Mount a specific commit OID.
    Commit(ObjectId),
    /// Mount a subtree of a commit. `path` is `/`-separated and must
    /// resolve to a directory inside the commit.
    Subtree {
        /// Underlying commit.
        commit: ObjectId,
        /// `/`-separated path to the subtree root.
        path: String,
    },
}

impl Projection {
    /// Resolve the projection to its top-level tree OID.
    ///
    /// For `Ref` this looks up the ref then peels to the commit's tree.
    /// For `Commit` it peels the commit. For `Subtree` it peels the
    /// commit and walks `path`, requiring the result to be a directory.
    pub fn root_tree(&self, store: &ObjectStore) -> Result<ObjectId, ProjectionError> {
        match self {
            Self::Ref(name) => {
                let commit = store.resolve_ref(name)?;
                Ok(store.commit_tree(commit)?)
            }
            Self::Commit(oid) => Ok(store.commit_tree(*oid)?),
            Self::Subtree { commit, path } => {
                let tree = store.commit_tree(*commit)?;
                let nav = TreeNavigator::new(store);
                let entry = nav.lookup(tree, path)?;
                if !entry.mode.is_dir() {
                    return Err(ProjectionError::NotADirectory {
                        component: path
                            .rsplit('/')
                            .find(|s| !s.is_empty())
                            .unwrap_or("")
                            .to_owned(),
                        parent: path
                            .rsplit_once('/')
                            .map(|(p, _)| p.to_owned())
                            .unwrap_or_default(),
                    });
                }
                Ok(entry.oid)
            }
        }
    }

    /// Resolve a virtual `path` (relative to the projection root) into
    /// either a synthetic overlay entry or a real tree entry.
    ///
    /// Empty path resolves to the projection root tree as a directory.
    pub fn lookup(
        &self,
        store: &ObjectStore,
        overlay: &RootOverlay,
        path: &str,
    ) -> Result<ResolvedEntry, ProjectionError> {
        let trimmed = path.trim_matches('/');
        if !trimmed.is_empty() {
            // Check whether the first path component is shadowed by the overlay.
            let first = trimmed.split('/').next().unwrap();
            if let Some(entry) = overlay.get(first.as_bytes()) {
                return resolve_in_synthetic(first, entry, &trimmed[first.len()..]);
            }
        }
        let root_tree = self.root_tree(store)?;
        let nav = TreeNavigator::new(store);
        let entry = nav.lookup(root_tree, path)?;
        Ok(ResolvedEntry::Tree(entry))
    }

    /// List the entries at the projection root: synthetic overlay
    /// entries first (sorted), then real tree entries that are not
    /// shadowed.
    ///
    /// Returns `(name, ResolvedEntry)` pairs. Use this for `readdir`
    /// at the projection root.
    pub fn read_root(
        &self,
        store: &ObjectStore,
        overlay: &RootOverlay,
    ) -> Result<Vec<(BString, ResolvedEntry)>, ProjectionError> {
        let root_tree = self.root_tree(store)?;
        let mut out: Vec<(BString, ResolvedEntry)> = Vec::new();

        // Synthetic entries (already sorted by BTreeMap).
        for (name, entry) in overlay.iter() {
            out.push((
                name.clone(),
                ResolvedEntry::Synthetic {
                    name: name.clone(),
                    entry: entry.clone(),
                },
            ));
        }

        // Real-tree entries, skipping any name shadowed by the overlay.
        let nav = TreeNavigator::new(store);
        for entry in nav.list(root_tree)? {
            if overlay.would_collide(entry.name.as_ref()) {
                continue;
            }
            out.push((entry.name.clone(), ResolvedEntry::Tree(entry)));
        }
        Ok(out)
    }
}

/// Walk `rest` (a `/`-separated path) inside a synthetic root entry
/// previously matched by `head`.
fn resolve_in_synthetic(
    head: &str,
    entry: &SyntheticEntry,
    rest: &str,
) -> Result<ResolvedEntry, ProjectionError> {
    let trimmed = rest.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(ResolvedEntry::Synthetic {
            name: BString::from(head),
            entry: entry.clone(),
        });
    }
    let mut current = entry;
    let mut walked = head.to_owned();
    let components: Vec<&str> = trimmed.split('/').filter(|c| !c.is_empty()).collect();
    for (i, component) in components.iter().enumerate() {
        match current {
            SyntheticEntry::Directory { children } => {
                let child = children
                    .get(bstr::BStr::new(component.as_bytes()))
                    .ok_or_else(|| ProjectionError::NotFound {
                        component: (*component).to_owned(),
                        parent: walked.clone(),
                    })?;
                if i + 1 == components.len() {
                    return Ok(ResolvedEntry::Synthetic {
                        name: BString::from(*component),
                        entry: child.clone(),
                    });
                }
                current = child;
                walked.push('/');
                walked.push_str(component);
            }
            _ => {
                return Err(ProjectionError::NotADirectory {
                    component: (*component).to_owned(),
                    parent: walked.clone(),
                });
            }
        }
    }
    unreachable!("loop returns on the last component")
}

/// Convenience: classify a `ResolvedEntry`'s mode for callers that
/// don't want to pattern-match on the variant first.
pub fn entry_mode(entry: &ResolvedEntry) -> EntryMode {
    match entry {
        ResolvedEntry::Tree(t) => t.mode,
        ResolvedEntry::Synthetic { entry, .. } => match entry {
            SyntheticEntry::File { .. } => EntryMode::RegularFile,
            SyntheticEntry::Directory { .. } => EntryMode::Directory,
            SyntheticEntry::Symlink { .. } => EntryMode::Symlink,
        },
    }
}

// Internal "ObjectStoreError -> ProjectionError" plumbing. Most call
// sites use the `?` operator + `From` impl already on
// `ProjectionError::Store`. This helper is here in case future code
// needs to construct one explicitly.
#[allow(dead_code)]
fn store_err(e: ObjectStoreError) -> ProjectionError {
    e.into()
}
