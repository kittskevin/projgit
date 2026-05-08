//! `projgit-core` — projection engine, object store, fetcher abstractions,
//! and the [`RootOverlay`] mechanism described in
//! `docs/design/dotgit-synthesis.md`.
//!
//! Phase 1 modules (no FS I/O, no network):
//!
//! - [`object_store`] — read-only wrapper over `gix-odb` with a
//!   `MissingObject(oid)` error variant the Fetcher will intercept.
//! - [`tree`]          — `TreeNavigator` walks git tree objects to
//!   resolve `/`-separated virtual paths.
//! - [`overlay`]       — `RootOverlay` injects synthetic top-level
//!   entries on top of the real tree (empty in MVP; mechanism only).
//! - [`projection`]    — the `Projection` enum (`Ref`, `Commit`,
//!   `Subtree`) and the resolver that turns a virtual path into a
//!   git object lookup, consulting the overlay first.
//! - [`error`]         — typed errors shared across the crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod object_store;
pub mod overlay;
pub mod projection;
pub mod tree;

pub use error::{ObjectStoreError, ProjectionError};
pub use object_store::{ObjectKind, ObjectStore};
pub use overlay::{RootOverlay, SyntheticEntry};
pub use projection::{Projection, ResolvedEntry};
pub use tree::{EntryMode, TreeEntry, TreeNavigator};

/// Crate version, exposed to the CLI and other consumers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
