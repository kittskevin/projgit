//! `projgit-core` — projection engine, object store, fetcher abstractions,
//! and the [`RootOverlay`] mechanism described in
//! `docs/design/dotgit-synthesis.md`.
//!
//! Modules:
//!
//! - [`object_store`] — read-only wrapper over `gix-odb` with a
//!   `MissingObject(oid)` error variant the Fetcher intercepts.
//! - [`tree`]          — `TreeNavigator` walks git tree objects to
//!   resolve `/`-separated virtual paths.
//! - [`overlay`]       — `RootOverlay` injects synthetic top-level
//!   entries on top of the real tree (empty in MVP; mechanism only).
//! - [`projection`]    — the `Projection` enum (`Ref`, `Commit`,
//!   `Subtree`) and the resolver that turns a virtual path into a
//!   git object lookup, consulting the overlay first.
//! - [`fetcher`]       — `Fetcher` trait, `GixFetcher`, `GitCliFetcher`,
//!   `NoopFetcher`, single-flight `Coalescer`, and
//!   `HydratingObjectStore` that wraps an `ObjectStore` with a
//!   fetcher for transparent miss-then-fetch.
//!   `GixFetcher` is gated behind the `gix-fetcher` Cargo feature
//!   (default-on); consumers that only need the trait,
//!   `GitCliFetcher`, or `NoopFetcher` can disable it to avoid
//!   pulling reqwest + rustls + ring.
//! - [`fs_provider`]   — OS-agnostic read-only `FsProvider` trait that
//!   FUSE / WinFsp backends implement, plus inode allocator and an
//!   in-memory provider for testing.
//! - [`error`]         — typed errors shared across the crate.
//! - [`clone`]         — one-time partial-clone helper backed by the
//!   system `git` executable.
//! - [`dotgit`]        — builds an A1 `.git/` [`RootOverlay`] so tools
//!   that look upward for `.git/` see the projection as a real git
//!   repository in detached-HEAD mode.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod blob_cache;
pub mod clone;
pub mod dotgit;
pub mod error;
pub mod fetcher;
pub mod fs_provider;
mod header_cache;
pub mod maintenance;
pub mod object_store;
pub mod overlay;
pub mod prefetch;
pub mod projection;
pub mod projection_fs;
pub mod tree;
mod tree_cache;

pub use blob_cache::BlobCacheStats;
pub use error::{ObjectStoreError, ProjectionError};
pub use maintenance::{run_maintenance, MaintenanceError};
pub use fetcher::{
    Fetcher, FetcherError, GitCliFetcher, GitCliFetcherError, HeaderProbe, HydrateError,
    HydratingObjectStore, NoopFetcher, WarmTreeStats,
};
#[cfg(feature = "gix-fetcher")]
pub use fetcher::{GixFetcher, GixFetcherError};
#[cfg(feature = "gvfs-fetcher")]
pub use fetcher::{GvfsFetcher, GvfsFetcherError};
pub use fs_provider::{
    Attr, DirEntry, FileType, FsError, FsProvider, InMemoryFsProvider, InodeAllocator, InodeKind,
    ROOT_INODE,
};
pub use header_cache::HeaderCacheStats;
pub use object_store::{ObjectKind, ObjectStore};
pub use overlay::{RootOverlay, SyntheticEntry};
pub use prefetch::PrefetchStats;
pub use projection::{Projection, ResolvedEntry};
pub use projection_fs::ProjectionFsProvider;
pub use tree::{EntryMode, TreeEntry, TreeNavigator};
pub use tree_cache::TreeCacheStats;

/// Crate version, exposed to the CLI and other consumers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Git blob object id (SHA-1, hex) of `content` — i.e.
/// `sha1("blob " + len + "\0" + content)`.
///
/// Used to content-address materialized upper-layer blobs (writable
/// worktree crash journal) so identical content dedups and the names
/// line up with git's own object database.
pub fn blob_oid_hex(content: &[u8]) -> String {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, content)
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod blob_oid_tests {
    use super::blob_oid_hex;

    #[test]
    fn matches_git_hash_object() {
        // `git hash-object` of the empty blob and of "hello\n".
        assert_eq!(
            blob_oid_hex(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            blob_oid_hex(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}
