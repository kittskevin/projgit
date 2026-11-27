//! Mount helpers wrapping `fuser::mount2` and the `Config` type.
//!
//! Phase 3b ships a single read-only mount path. Future phases will
//! add `BackgroundSession` support for letting the CLI manage many
//! mounts from one process.

use crate::adapter::{
    PassthroughHandle, PassthroughStats, ProjgitFuse, DEFAULT_PASSTHROUGH_IDLE_CACHE_CAPACITY,
    DEFAULT_PASSTHROUGH_MAX_BYTES,
};
use crate::overlay_scratch::OverlayScratchConfig;
use fuser::{mount2, spawn_mount2, BackgroundSession, Config, MountOption, SessionACL};
use projgit_core::FsProvider;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Per-mount knobs the CLI exposes today.
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// Reported volume name (`fsname=...`).
    pub name: String,
    /// Reported filesystem type (`subtype=...`).
    pub subtype: String,
    /// Who can access the mount. Defaults to [`SessionACL::Owner`]
    /// (only the user who started the mount).
    pub acl: SessionACL,
    /// Number of FUSE event-loop threads. `None` => single-threaded.
    pub n_threads: Option<usize>,
    /// Negotiate `FUSE_PASSTHROUGH` (Linux 6.9+) when available so `read()` / `mmap()`
    /// bypass userspace and go directly to `<git-dir>/blobs/<xx>/<oid>` backing files.
    pub passthrough: bool,
    /// Maximum blob size (bytes) that `open()` will synchronously hydrate into
    /// `<git-dir>/blobs/<xx>/<oid>` for `FOPEN_PASSTHROUGH`. Larger blobs still use
    /// passthrough once already materialized on disk (or when projected as a
    /// `SyntheticHostFile`), but unmaterialized large blobs stay on `read_blob_range()`.
    pub passthrough_max_bytes: u64,
    /// Maximum number of idle (`open_count == 0`) kernel `BackingId`s kept in the
    /// bounded LRU cache (`0` closes `BackingId` immediately on final `release()`).
    pub passthrough_idle_cache_capacity: usize,
    /// Optional writable scratch directory (`--writable-scratch <DIR>`).
    ///
    /// When set on Linux, the read-only FUSE projection is mounted at `<DIR>/lower`
    /// and kernel `overlayfs` (`upperdir=<DIR>/upper,workdir=<DIR>/work,metacopy=on`)
    /// is stacked onto the user-visible `mountpoint`.
    pub writable_scratch: Option<PathBuf>,
    /// Optional path to a FSMonitor **write-log** file. When set on a
    /// writable mount ([`crate::mount_writable_background`]), the overlay
    /// rewrites this file on every change with a monotonic token + the
    /// NUL-terminated worktree-relative paths modified since mount; a
    /// `core.fsmonitor` hook streams it to git so `status` skips
    /// scanning. No effect on read-only mounts.
    pub fsmonitor_file: Option<std::path::PathBuf>,
    /// Cone-mode sparse-checkout directories (worktree-relative, no
    /// trailing slash). When non-empty on a writable mount, the overlay
    /// hides out-of-cone directories from `readdir`/`lookup` so git's
    /// sparse-index stays collapsed instead of expanding to a full index
    /// (see `docs/implementation/writable-worktrees-plan.md` Stage 5 /
    /// R2). Empty => everything is visible.
    pub sparse_cone: Vec<String>,
    /// Optional directory for the writable **upper** crash journal. When
    /// set on a writable mount, materialized/created files + whiteouts
    /// are persisted here (append-only journal + content-addressed
    /// blobs) and replayed on the next mount, so *uncommitted* edits
    /// survive an unmount (design §10.3). No effect on read-only mounts.
    pub upper_dir: Option<std::path::PathBuf>,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            name: "projgit".to_owned(),
            subtype: "projgit".to_owned(),
            acl: SessionACL::Owner,
            n_threads: None,
            passthrough: true,
            passthrough_max_bytes: DEFAULT_PASSTHROUGH_MAX_BYTES,
            passthrough_idle_cache_capacity: DEFAULT_PASSTHROUGH_IDLE_CACHE_CAPACITY,
            writable_scratch: None,
            fsmonitor_file: None,
            sparse_cone: Vec::new(),
            upper_dir: None,
        }
    }
}

impl MountConfig {
    /// Convert into the [`fuser::Config`] that `mount2` consumes.
    fn to_fuser_config(&self) -> Config {
        // `Config` is `#[non_exhaustive]`; build via `default()` then
        // mutate the public fields we care about.
        let mut fc = Config::default();
        fc.mount_options = vec![
            MountOption::RO,
            MountOption::FSName(self.name.clone()),
            MountOption::Subtype(self.subtype.clone()),
            // Default-deny anything dangerous; we're a read-only
            // projection of git data, not a place to run binaries
            // or set-uid.
            MountOption::NoExec,
            MountOption::NoSuid,
            MountOption::NoDev,
        ];
        fc.acl = self.acl;
        fc.n_threads = self.n_threads;
        fc
    }
}

/// Mount `provider` at `mountpoint` and run the FUSE event loop until
/// the mount is unmounted.
///
/// **Blocks the calling thread** for the duration of the mount.
/// Background mounting (returning a handle the caller can park) lands
/// in a later phase when the CLI grows multi-mount management.
pub fn mount<F: FsProvider + 'static>(
    provider: Arc<F>,
    mountpoint: impl AsRef<Path>,
    config: &MountConfig,
) -> std::io::Result<()> {
    let fs = ProjgitFuse::with_passthrough_config(
        provider,
        config.passthrough,
        config.passthrough_max_bytes,
        config.passthrough_idle_cache_capacity,
    );
    mount2(fs, mountpoint, &config.to_fuser_config())
}

/// Active background mount session that optionally manages a stacked Linux
/// `overlayfs` writable upper layer on top of the read-only FUSE projection.
///
/// On `Drop`, if an `overlayfs` upper mount was stacked onto `overlay_mountpoint`,
/// it is unmounted first before the underlying FUSE `BackgroundSession` is dropped.
pub struct ManagedMountSession {
    fuse_session: Option<BackgroundSession>,
    overlay_mountpoint: Option<PathBuf>,
    passthrough_handle: Option<PassthroughHandle>,
}

impl ManagedMountSession {
    /// Wrap a plain read-only [`BackgroundSession`].
    pub fn from_fuse(session: BackgroundSession) -> Self {
        Self {
            fuse_session: Some(session),
            overlay_mountpoint: None,
            passthrough_handle: None,
        }
    }

    /// Wrap a read-only [`BackgroundSession`] along with its [`PassthroughHandle`].
    pub fn from_fuse_with_passthrough(
        session: BackgroundSession,
        passthrough_handle: PassthroughHandle,
    ) -> Self {
        Self {
            fuse_session: Some(session),
            overlay_mountpoint: None,
            passthrough_handle: Some(passthrough_handle),
        }
    }

    /// Return the stacked `overlayfs` mountpoint if this session was mounted
    /// with [`MountConfig::writable_scratch`].
    pub fn overlay_mountpoint(&self) -> Option<&Path> {
        self.overlay_mountpoint.as_deref()
    }

    /// Return the [`PassthroughHandle`] associated with this mount, if any.
    pub fn passthrough_handle(&self) -> Option<&PassthroughHandle> {
        self.passthrough_handle.as_ref()
    }

    /// Return a snapshot of FUSE passthrough counters for this mount, if available.
    pub fn passthrough_stats(&self) -> Option<PassthroughStats> {
        self.passthrough_handle.as_ref().map(|h| h.stats())
    }

    /// Release all idle (`open_count == 0`) kernel `BackingId`s for this mount so
    /// unlinked L2 blob cache files can be reclaimed immediately on disk.
    pub fn evict_idle_backing_files(&self) -> usize {
        self.passthrough_handle
            .as_ref()
            .map(|h| h.evict_idle())
            .unwrap_or(0)
    }
}

impl Drop for ManagedMountSession {
    fn drop(&mut self) {
        if let Some(ref mp) = self.overlay_mountpoint {
            let _ = unmount_overlayfs(mp);
        }
        // Drop the underlying FUSE session after the overlay on top of it is unmounted.
        let _ = self.fuse_session.take();
    }
}

/// Mount `provider` at `mountpoint` and return immediately.
///
/// Spawns the FUSE event loop on a background thread and returns a
/// [`BackgroundSession`] handle. The mount stays alive until either:
///
/// - the returned `BackgroundSession` is dropped (graceful unmount), or
/// - the caller explicitly calls [`BackgroundSession::join`] to wait
///   for the loop to exit.
///
/// This is the API the future CLI mount manager + the runtime FUSE
/// smoke test build on. Prefer it over [`mount`] whenever the caller
/// needs to do anything else on the same thread.
///
/// Errors mirror [`mount`]: missing mountpoint, EPERM, missing
/// `/dev/fuse` etc. all surface as `io::Error` from `spawn_mount2`.
pub fn mount_background<F: FsProvider + 'static>(
    provider: Arc<F>,
    mountpoint: impl AsRef<Path>,
    config: &MountConfig,
) -> std::io::Result<BackgroundSession> {
    let (session, _handle) = mount_background_with_passthrough(provider, mountpoint, config)?;
    Ok(session)
}

/// Like [`mount_background`], but also returns a [`PassthroughHandle`] for
/// inspecting [`PassthroughStats`] and evicting idle kernel `BackingId`s.
pub fn mount_background_with_passthrough<F: FsProvider + 'static>(
    provider: Arc<F>,
    mountpoint: impl AsRef<Path>,
    config: &MountConfig,
) -> std::io::Result<(BackgroundSession, PassthroughHandle)> {
    let fs = ProjgitFuse::with_passthrough_config(
        provider,
        config.passthrough,
        config.passthrough_max_bytes,
        config.passthrough_idle_cache_capacity,
    );
    let handle = fs.passthrough_handle();
    let session = spawn_mount2(fs, mountpoint, &config.to_fuser_config())?;
    Ok((session, handle))
}

/// Mount `provider` at `mountpoint` in the background, stacking a writable
/// Linux `overlayfs` layer when [`MountConfig::writable_scratch`] is `Some`.
///
/// When `config.writable_scratch` is `None`, this behaves identically to
/// [`mount_background`]. When `Some(scratch_dir)`:
/// 1. Creates `<scratch_dir>/{lower,upper,work}`.
/// 2. Mounts the read-only FUSE projection at `<scratch_dir>/lower`.
/// 3. Mounts kernel `overlayfs` onto `mountpoint` using
///    [`OverlayScratchConfig::fallback_mount_options_chain`].
pub fn mount_managed_background<F: FsProvider + 'static>(
    provider: Arc<F>,
    mountpoint: impl AsRef<Path>,
    config: &MountConfig,
) -> std::io::Result<ManagedMountSession> {
    let mountpoint = mountpoint.as_ref();
    let Some(ref scratch_root) = config.writable_scratch else {
        let (session, handle) = mount_background_with_passthrough(provider, mountpoint, config)?;
        return Ok(ManagedMountSession::from_fuse_with_passthrough(
            session, handle,
        ));
    };

    if !mountpoint.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("mountpoint `{}` does not exist", mountpoint.display()),
        ));
    }

    let scratch_cfg = OverlayScratchConfig::new(scratch_root);
    let dirs = scratch_cfg.prepare_dirs()?;
    let (fuse_session, handle) =
        mount_background_with_passthrough(provider, &dirs.lower_dir, config)?;

    if let Err(e) = mount_overlayfs(&scratch_cfg, &dirs.lower_dir, mountpoint) {
        drop(fuse_session);
        return Err(e);
    }

    Ok(ManagedMountSession {
        fuse_session: Some(fuse_session),
        overlay_mountpoint: Some(mountpoint.to_path_buf()),
        passthrough_handle: Some(handle),
    })
}

fn mount_overlayfs(
    scratch_cfg: &OverlayScratchConfig,
    lower_dir: &Path,
    mountpoint: &Path,
) -> std::io::Result<()> {
    let mut last_err = String::new();
    for opts in scratch_cfg.fallback_mount_options_chain(lower_dir) {
        match Command::new("mount")
            .arg("-t")
            .arg("overlay")
            .arg("overlay")
            .arg("-o")
            .arg(&opts)
            .arg(mountpoint)
            .output()
        {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                last_err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "failed to mount overlayfs at `{}` (lowerdir `{}`): {}",
            mountpoint.display(),
            lower_dir.display(),
            last_err
        ),
    ))
}

fn unmount_overlayfs(mountpoint: &Path) -> std::io::Result<()> {
    if let Ok(out) = Command::new("umount").arg(mountpoint).output() {
        if out.status.success() {
            return Ok(());
        }
    }
    let out = Command::new("umount").arg("-l").arg(mountpoint).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use projgit_core::InMemoryFsProvider;

    #[test]
    fn mount_config_defaults_are_safe_and_read_only() {
        let cfg = MountConfig::default();
        let fc = cfg.to_fuser_config();
        assert!(fc
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::RO)));
        assert!(fc
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::NoExec)));
        assert!(fc
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::NoSuid)));
        assert!(fc
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::NoDev)));
        assert_eq!(fc.acl, SessionACL::Owner);
        assert_eq!(fc.n_threads, None);
    }

    #[test]
    fn mount_config_reports_fsname_and_subtype() {
        let cfg = MountConfig {
            name: "my-mount".to_owned(),
            subtype: "projgit-ref".to_owned(),
            ..MountConfig::default()
        };
        let fc = cfg.to_fuser_config();
        assert!(fc
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::FSName(s) if s == "my-mount")));
        assert!(fc
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::Subtype(s) if s == "projgit-ref")));
    }

    #[test]
    fn allow_other_via_session_acl_all() {
        let cfg = MountConfig {
            acl: SessionACL::All,
            ..MountConfig::default()
        };
        let fc = cfg.to_fuser_config();
        assert_eq!(fc.acl, SessionACL::All);
    }

    /// Construction smoke test: ProjgitFuse over an in-memory provider
    /// must be `Send + Sync + 'static` (required by fuser::Filesystem).
    #[test]
    fn projgit_fuse_is_send_sync_static() {
        fn assert_send_sync_static<T: Send + Sync + 'static>() {}
        assert_send_sync_static::<ProjgitFuse<InMemoryFsProvider>>();
    }

    /// `mount_background` must return `Err` (not panic) when the
    /// mountpoint doesn't exist. Exercises the API surface without
    /// requiring `/dev/fuse` to be available, so it's safe to run as
    /// a non-ignored test on any Linux / macOS host.
    #[test]
    fn mount_background_errors_on_missing_mountpoint() {
        let provider = Arc::new(InMemoryFsProvider::new());
        let cfg = MountConfig::default();
        // Path under temp_dir that is guaranteed not to exist.
        let bogus = std::env::temp_dir().join(format!(
            "projgit-mount-bg-nonexistent-{}",
            std::process::id()
        ));
        let result = mount_background(provider, &bogus, &cfg);
        assert!(
            result.is_err(),
            "expected Err on missing mountpoint, got Ok"
        );
    }
}
