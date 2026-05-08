//! WinFsp filesystem backend for projgit. Builds on Windows only;
//! compiles to an empty crate on other targets so the workspace can build
//! everywhere.
//!
//! See `docs/initial-plan.md` Phase 3 and
//! `docs/design/windows-symlinks.md` for the Windows symlink policy.

#![cfg(target_os = "windows")]

/// Marker constant so the crate has something to compile on Windows.
pub const SUPPORTED: bool = true;
