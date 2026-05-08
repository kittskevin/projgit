//! FUSE filesystem backend for projgit. Builds on Linux and macOS;
//! compiles to an empty crate on other targets so the workspace can build
//! everywhere.
//!
//! See `docs/initial-plan.md` Phase 3.

#![cfg(any(target_os = "linux", target_os = "macos"))]

/// Marker constant so the crate has something to compile on supported targets.
pub const SUPPORTED: bool = true;
