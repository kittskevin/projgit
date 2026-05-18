//! FUSE filesystem backend for projgit.
//!
//! Builds on Linux and macOS. On other targets (notably Windows) this
//! crate compiles to an empty crate so the workspace can build
//! everywhere; the WinFsp backend lives in `projgit-winfsp`.
//!
//! # Architecture
//!
//! The backend is intentionally thin: it adapts the OS-agnostic
//! [`projgit_core::FsProvider`] trait to fuser's [`fuser::Filesystem`]
//! trait, translating types (inode/handle newtypes, `FileAttr`,
//! `FileType`, `Errno`) and reply patterns. All "what does the
//! projection contain" logic lives in `projgit-core`.
//!
//! # Verification on non-Linux hosts
//!
//! The crate uses `cfg(any(target_os = "linux", target_os = "macos"))`
//! so the FUSE-specific code only exists on supported targets. On
//! Windows it compiles down to nothing. To compile-check the FUSE
//! adapter from a Windows or macOS dev machine, use:
//!
//! ```sh
//! rustup target add x86_64-unknown-linux-gnu
//! cargo check -p projgit-fuse --target x86_64-unknown-linux-gnu
//! ```

#![cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(unused))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod adapter;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod mount;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use adapter::ProjgitFuse;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use mount::{mount, mount_background, MountConfig};

/// Re-export of [`fuser::BackgroundSession`] so callers don't need
/// a direct dependency on `fuser` to keep a [`mount_background`]
/// handle alive.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use fuser::BackgroundSession;

/// Re-export of [`fuser::SessionACL`] so callers can configure
/// `MountConfig::acl` without taking a direct dependency on `fuser`.
/// `SessionACL::All` enables the kernel `allow_other` mount option
/// (required for any cross-UID access, including bind-mounts into
/// containers).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use fuser::SessionACL;

/// Marker constant: `true` when the FUSE backend is built for this
/// target and `mount`/`mount_background` actually work; `false`
/// elsewhere. Mirrored by `projgit_winfsp::SUPPORTED` on Windows,
/// where the meaning is the same: "this backend can serve a mount\
/// today." Useful for tests / runtime feature detection.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub const SUPPORTED: bool = true;

/// Marker constant: `false` on this target. See the
/// `target_os = "linux"`/`"macos"` arm above for the meaning.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub const SUPPORTED: bool = false;
