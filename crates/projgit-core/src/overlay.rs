//! `RootOverlay` — synthetic top-level entries spliced onto the
//! projection root.
//!
//! See `docs/design/dotgit-synthesis.md` §10 for the full design. In
//! Phase 1 we ship the **mechanism only**, not any default content;
//! the overlay starts empty and the projection layer treats it as a
//! pure pass-through.
//!
//! Architectural rules:
//!
//! - **Read-only.** [`SyntheticEntry`] has no write operations.
//! - **Collision policy.** When a synthetic entry name collides with a
//!   real tree entry, the synthetic entry wins; consumers may emit a
//!   warn-once log. (The mechanism just reports the collision via
//!   [`RootOverlay::would_collide`]; the projection layer decides what
//!   to do with the warning.)
//! - **Reserved inode namespace.** Synthetic-derived inodes set the
//!   high bit (`1 << 63`) so they never collide with tree-derived
//!   inodes (which are produced from blob OIDs and always fit in the
//!   low 63 bits). The FS frontends consume this rule when allocating
//!   their own `FileId` values.

use bstr::{BStr, BString};
use std::collections::BTreeMap;

/// A synthetic root-level entry that the projection layer surfaces
/// **before** falling through to the underlying git tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntheticEntry {
    /// A regular file whose content is fully owned in memory.
    File {
        /// Bytes returned on read.
        content: Vec<u8>,
        /// Git-style mode (typically `0o100644`). Stored as `u16` to
        /// match [`crate::tree::TreeEntry::mode_raw`].
        mode_raw: u16,
    },
    /// A directory containing further synthetic entries.
    ///
    /// Nested synthetic directories are supported by the data model so
    /// future content (e.g. `.projgit/info.json`) can be added without
    /// changing the API.
    Directory {
        /// Direct children, keyed by name.
        children: BTreeMap<BString, SyntheticEntry>,
    },
    /// A symlink whose target is the given byte string.
    Symlink {
        /// Verbatim link target (POSIX-style path; not resolved here).
        target: BString,
    },
}

impl SyntheticEntry {
    /// Construct a `0o100644` synthetic file from inline bytes.
    pub fn file(content: impl Into<Vec<u8>>) -> Self {
        Self::File {
            content: content.into(),
            mode_raw: 0o100644,
        }
    }

    /// Construct an empty synthetic directory.
    pub fn directory() -> Self {
        Self::Directory {
            children: BTreeMap::new(),
        }
    }

    /// Construct a synthetic symlink pointing at `target`.
    pub fn symlink(target: impl Into<BString>) -> Self {
        Self::Symlink {
            target: target.into(),
        }
    }

    /// Insert a child into a `Directory` entry. Panics on `File`/`Symlink`.
    pub fn insert_child(&mut self, name: impl Into<BString>, entry: SyntheticEntry) {
        match self {
            Self::Directory { children } => {
                children.insert(name.into(), entry);
            }
            _ => panic!("insert_child on a non-directory SyntheticEntry"),
        }
    }
}

/// Synthetic top-level entries spliced onto the projection root.
///
/// Empty in MVP; the existence of the type is the architectural
/// commitment.
#[derive(Debug, Default, Clone)]
pub struct RootOverlay {
    entries: BTreeMap<BString, SyntheticEntry>,
}

impl RootOverlay {
    /// An empty overlay (the MVP default).
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Number of top-level synthetic entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the overlay has any entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert (or replace) a top-level synthetic entry.
    pub fn insert(&mut self, name: impl Into<BString>, entry: SyntheticEntry) {
        self.entries.insert(name.into(), entry);
    }

    /// Look up a top-level entry by name.
    pub fn get(&self, name: &[u8]) -> Option<&SyntheticEntry> {
        self.entries.get(BStr::new(name))
    }

    /// Iterate over (name, entry) pairs in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&BString, &SyntheticEntry)> {
        self.entries.iter()
    }

    /// Sorted iterator over names; useful for `readdir` splicing.
    pub fn names(&self) -> impl Iterator<Item = &BString> {
        self.entries.keys()
    }

    /// Returns true iff the overlay would shadow `real_name` at the
    /// projection root. Lets the projection layer emit a warn-once log
    /// when it observes a collision.
    pub fn would_collide(&self, real_name: &[u8]) -> bool {
        self.entries.contains_key(BStr::new(real_name))
    }
}

/// Returns true if `inode` was allocated from the synthetic namespace.
///
/// FS frontends allocate synthetic inode values with the high bit set
/// and tree-derived inode values with the high bit clear. This helper
/// lets either side check which space an inode came from without
/// having to consult an external table.
pub const SYNTHETIC_INODE_BIT: u64 = 1 << 63;

/// Mark `inode` as belonging to the synthetic namespace.
pub const fn mark_synthetic_inode(inode: u64) -> u64 {
    inode | SYNTHETIC_INODE_BIT
}

/// Returns true iff the inode is in the synthetic namespace.
pub const fn is_synthetic_inode(inode: u64) -> bool {
    inode & SYNTHETIC_INODE_BIT != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_overlay_is_empty() {
        let o = RootOverlay::new();
        assert!(o.is_empty());
        assert_eq!(o.len(), 0);
        assert_eq!(o.iter().count(), 0);
        assert!(o.get(b".git").is_none());
        assert!(!o.would_collide(b"src"));
    }

    #[test]
    fn insert_and_lookup() {
        let mut o = RootOverlay::new();
        o.insert(BString::from(".projgit"), {
            let mut dir = SyntheticEntry::directory();
            dir.insert_child(
                BString::from("info.json"),
                SyntheticEntry::file(b"{\"v\":1}".to_vec()),
            );
            dir
        });
        assert_eq!(o.len(), 1);
        assert!(o.get(b".projgit").is_some());
        assert!(o.get(b"missing").is_none());
        assert!(o.would_collide(b".projgit"));
    }

    #[test]
    fn iteration_is_sorted() {
        let mut o = RootOverlay::new();
        o.insert(BString::from("zebra"), SyntheticEntry::file(b"z".to_vec()));
        o.insert(BString::from("apple"), SyntheticEntry::file(b"a".to_vec()));
        o.insert(BString::from("middle"), SyntheticEntry::file(b"m".to_vec()));
        let names: Vec<&[u8]> = o.names().map(|n| n.as_slice()).collect();
        assert_eq!(names, vec![b"apple".as_ref(), b"middle".as_ref(), b"zebra".as_ref()]);
    }

    #[test]
    fn synthetic_inode_namespace() {
        let real_inode: u64 = 0x0123_4567_89AB_CDEF;
        assert!(!is_synthetic_inode(real_inode));

        let synth_inode = mark_synthetic_inode(real_inode);
        assert!(is_synthetic_inode(synth_inode));

        // Marking is idempotent.
        assert_eq!(mark_synthetic_inode(synth_inode), synth_inode);

        // Real-tree inodes can use the full low-63 space without ever
        // entering the synthetic space.
        assert!(!is_synthetic_inode(u64::MAX >> 1));
        assert!(is_synthetic_inode(u64::MAX));
    }
}
