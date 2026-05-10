//! Planned WinFsp filesystem backend for projgit.
//!
//! The production Windows backend is deferred. The implementation plan lives in
//! `docs/design/winfsp-implementation-plan.md`. This crate stays in the
//! workspace as the future backend boundary and compiles only on Windows.
//!
//! See `docs/initial-plan.md` Phase 3 and
//! `docs/design/windows-symlinks.md` for the Windows symlink policy.

#![cfg(target_os = "windows")]

/// Marker constant so the crate has something to compile on Windows.
pub const SUPPORTED: bool = true;
