//! Planned WinFsp filesystem backend for projgit.
//!
//! **Status: stub.** The production Windows backend is deliberately
//! deferred; this crate exists so the workspace builds on Windows and so
//! the future backend boundary stays visible in `Cargo.toml`. It compiles
//! to a single marker constant on Windows and to nothing on other targets.
//!
//! The implementation plan — `FspService*` lifecycle, the symlink
//! classifier per [`docs/design/windows-symlinks.md`](../../../docs/design/windows-symlinks.md),
//! per-user volume ownership for git's `safe.directory` check, and a
//! WinFsp adapter over [`projgit_core::FsProvider`](../projgit_core/fs_provider/trait.FsProvider.html) —
//! lives in [`docs/design/winfsp-implementation-plan.md`](../../../docs/design/winfsp-implementation-plan.md).
//!
//! The shared projection engine, caches, and fetchers it will sit on top
//! of are in [`projgit_core`](../projgit_core/index.html). The same
//! `FsProvider` trait powers the working FUSE backend in
//! [`projgit_fuse`](../projgit_fuse/index.html) today, so the WinFsp
//! adapter is intentionally a thin translation layer rather than a
//! parallel implementation.

#![cfg(target_os = "windows")]

/// Marker constant. Same meaning as [`projgit_fuse::SUPPORTED`]:
/// `true` when this crate's backend can actually serve a mount on
/// this target. The WinFsp backend is not yet implemented (see the
/// crate-level doc for status), so this is `false` even on Windows.
pub const SUPPORTED: bool = false;
