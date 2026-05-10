//! Mount helpers wrapping `fuser::mount2` and the `Config` type.
//!
//! Phase 3b ships a single read-only mount path. Future phases will
//! add `BackgroundSession` support for letting the CLI manage many
//! mounts from one process.

use crate::adapter::ProjgitFuse;
use fuser::{mount2, spawn_mount2, BackgroundSession, Config, MountOption, SessionACL};
use projgit_core::FsProvider;
use std::path::Path;
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
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            name: "projgit".to_owned(),
            subtype: "projgit".to_owned(),
            acl: SessionACL::Owner,
            n_threads: None,
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
    let fs = ProjgitFuse::new(provider);
    mount2(fs, mountpoint, &config.to_fuser_config())
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
    let fs = ProjgitFuse::new(provider);
    spawn_mount2(fs, mountpoint, &config.to_fuser_config())
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
