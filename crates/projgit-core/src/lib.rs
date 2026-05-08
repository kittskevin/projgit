//! `projgit-core` — projection engine, object store, fetcher abstractions,
//! and the [`RootOverlay`] mechanism described in
//! `docs/design/dotgit-synthesis.md`.
//!
//! Pre-implementation placeholder. See `docs/initial-plan.md` Phase 1 for
//! the planned modules.

/// Crate version, exposed to the CLI and other consumers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
