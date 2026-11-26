//! WinFsp filesystem backend for projgit on Windows.
//!
//! Translates WinFsp dispatcher callbacks (`FSP_FILE_SYSTEM_INTERFACE`) into
//! [`projgit_core::FsProvider`] operations using custom, self-contained FFI
//! bindings (`src/ffi.rs`) against the official WinFsp C ABI (`fsctl.h` /
//! `winfsp.h`).
//!
//! # Licensing & WinFsp FLOSS Exception
//!
//! `projgit-winfsp` is licensed under `MIT OR Apache-2.0` and dynamically
//! loads `winfsp-x64.dll` at runtime under the **WinFsp FLOSS Exception**:
//!
//! > WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
//! > (<https://github.com/winfsp/winfsp>)
//!
//! No GPL-3.0 Rust wrapper crates are used in the dependency tree.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod symlinks;

pub use symlinks::{
    build_symlink_reparse_buffer, classify_symlink_target, parse_symlink_reparse_buffer,
    SymlinkKind, SymlinkMode, IO_REPARSE_TAG_SYMLINK,
};

/// Attribution notice required by the WinFsp FLOSS Exception in user-facing
/// interfaces and documentation.
pub const WINFSP_FLOSS_NOTICE: &str =
    "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos (https://github.com/winfsp/winfsp)";

#[cfg(target_os = "windows")]
mod ffi;

#[cfg(target_os = "windows")]
mod adapter;

#[cfg(target_os = "windows")]
pub use adapter::{fs_error_to_ntstatus, ProjgitWinFsp, WinFspReadStats};

#[cfg(target_os = "windows")]
pub use ffi::{is_winfsp_available, query_winfsp_version};

/// Marker constant: `true` on Windows where the custom WinFsp backend is
/// compiled in; `false` on non-Windows targets.
///
/// Use [`is_winfsp_available`] on Windows to check at runtime whether the
/// WinFsp driver/DLL (`winfsp-x64.dll`) is installed on the host.
#[cfg(target_os = "windows")]
pub const SUPPORTED: bool = true;

/// Marker constant: `false` on non-Windows targets.
#[cfg(not(target_os = "windows"))]
pub const SUPPORTED: bool = false;

/// Configuration for mounting a [`projgit_core::FsProvider`] via WinFsp.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// Filesystem name reported in WinFsp volume parameters (`FileSystemName`).
    pub name: String,
    /// Volume label reported by `GetVolumeInfo`.
    pub volume_label: String,
    /// How git symlinks (`0120000`) are exposed on Windows (`NativeReparse` by
    /// default).
    pub symlink_mode: SymlinkMode,
    /// Number of WinFsp dispatcher threads (`0` = WinFsp default).
    pub n_threads: u32,
}

#[cfg(target_os = "windows")]
impl Default for MountConfig {
    fn default() -> Self {
        Self {
            name: "projgit".to_owned(),
            volume_label: "projgit".to_owned(),
            symlink_mode: SymlinkMode::NativeReparse,
            n_threads: 0,
        }
    }
}

/// Active background WinFsp mount session.
///
/// The mount remains active until this handle is dropped. On `Drop`, the
/// WinFsp dispatcher is stopped, the mountpoint is removed, and the
/// underlying `FSP_FILE_SYSTEM` is destroyed.
#[cfg(target_os = "windows")]
pub struct WinFspMountSession<P: projgit_core::FsProvider + 'static> {
    raw: ffi::RawWinFspMount,
    adapter: std::sync::Arc<ProjgitWinFsp<std::sync::Arc<P>>>,
}

#[cfg(target_os = "windows")]
impl<P: projgit_core::FsProvider + 'static> WinFspMountSession<P> {
    /// Path where this WinFsp filesystem is mounted.
    #[must_use]
    pub fn mountpoint(&self) -> &std::path::Path {
        self.raw.mountpoint()
    }

    /// Borrow the [`ProjgitWinFsp`] adapter backing this mount.
    #[must_use]
    pub fn adapter(&self) -> &ProjgitWinFsp<std::sync::Arc<P>> {
        &self.adapter
    }

    /// Snapshot of direct host-file vs provider read counters.
    #[must_use]
    pub fn read_stats(&self) -> WinFspReadStats {
        self.adapter.read_stats()
    }

    /// Explicitly unmount and stop the WinFsp dispatcher.
    pub fn unmount(self) {
        drop(self);
    }
}

/// Mount `provider` at `mountpoint` in the background using WinFsp and return
/// a [`WinFspMountSession`] RAII guard.
///
/// `mountpoint` may be an unused drive letter (e.g. `Z:`), a non-existent
/// directory path (WinFsp creates the mountpoint directory junction), or an
/// existing **empty** directory (matching Linux FUSE convention; `projgit-winfsp`
/// temporarily removes the empty directory before `FspFileSystemSetMountPoint`
/// and restores it when [`WinFspMountSession`] is dropped).
#[cfg(target_os = "windows")]
pub fn mount_background<P: projgit_core::FsProvider + 'static>(
    provider: std::sync::Arc<P>,
    mountpoint: impl AsRef<std::path::Path>,
    config: &MountConfig,
) -> std::io::Result<WinFspMountSession<P>> {
    let adapter = std::sync::Arc::new(
        ProjgitWinFsp::new(provider, config.symlink_mode, config.volume_label.clone())
            .map_err(std::io::Error::other)?,
    );
    let backend: std::sync::Arc<dyn ffi::WinFspBackend> = adapter.clone();
    let raw = ffi::RawWinFspMount::start(
        backend,
        mountpoint.as_ref(),
        &config.name,
        config.n_threads,
        config.symlink_mode == SymlinkMode::NativeReparse,
    )
    .map_err(std::io::Error::other)?;
    Ok(WinFspMountSession { raw, adapter })
}

/// Mount `provider` at `mountpoint` and block the calling thread until the
/// process receives an interrupt or the caller unmounts it.
#[cfg(target_os = "windows")]
pub fn mount<P: projgit_core::FsProvider + 'static>(
    provider: std::sync::Arc<P>,
    mountpoint: impl AsRef<std::path::Path>,
    config: &MountConfig,
) -> std::io::Result<()> {
    let _session = mount_background(provider, mountpoint, config)?;
    loop {
        std::thread::park();
    }
}
