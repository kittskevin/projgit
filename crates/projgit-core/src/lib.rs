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
//! - [`fetcher`]       — `Fetcher` trait, `GixFetcher`, `NoopFetcher`,
//!   single-flight `Coalescer`, and `HydratingObjectStore` that wraps
//!   an `ObjectStore` with a fetcher for transparent miss-then-fetch.
//! - [`error`]         — typed errors shared across the crate.
//! - [`clone`]         — one-time partial-clone helper.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clone;
pub mod error;
pub mod fetcher;
pub mod object_store;
pub mod overlay;
pub mod projection;
pub mod tree;

pub use error::{ObjectStoreError, ProjectionError};
pub use fetcher::{
    Fetcher, FetcherError, GixFetcher, GixFetcherError, HydrateError, HydratingObjectStore,
    NoopFetcher,
};
pub use object_store::{ObjectKind, ObjectStore};
pub use overlay::{RootOverlay, SyntheticEntry};
pub use projection::{Projection, ResolvedEntry};
pub use tree::{EntryMode, TreeEntry, TreeNavigator};

/// Crate version, exposed to the CLI and other consumers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
