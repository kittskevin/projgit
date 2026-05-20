//! `projgitd` — long-lived projgit daemon. Stage 2 of the projgitd plan
//! (see [`docs/implementation/projgitd-plan.md`]).
//!
//! This crate has two layers:
//!
//! - **`protocol`** — platform-agnostic request/response types and
//!   length-prefixed JSON framing. Always-compile so future Windows
//!   clients can build against it.
//! - **`server`** — Linux/macOS-only server implementation that drives
//!   the existing `projgit-core` projection stack and the `projgit-fuse`
//!   `mount_background` API in response to control-plane RPCs. Hosts a
//!   shared `Arc<ObjectStore>` + `Arc<HydratingObjectStore<F>>` so every
//!   served mount shares one set of in-memory caches and one fetcher.
//!
//! See [`docs/design/projgitd.md`] for the architecture this implements.

#![forbid(unsafe_code)]

pub mod protocol;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod server;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use server::{run, DaemonConfig};

/// `true` when this build can actually serve a mount (i.e. has the
/// FUSE backend compiled in). Mirrors `projgit_fuse::SUPPORTED`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub const SUPPORTED: bool = true;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub const SUPPORTED: bool = false;
