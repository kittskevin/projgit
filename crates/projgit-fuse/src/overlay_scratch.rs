//! Kernel `overlayfs` writable scratch layer configuration and mount helpers.
//!
//! Pairs a read-only `projgit` FUSE projection (`lowerdir`) with a writable
//! local directory (`upperdir` + `workdir`) so builds, tests, and agents can
//! create, modify, rename, and delete files (`A3` writable workspace semantics)
//! while keeping `projgit-core` and the underlying git object store strictly
//! immutable.
//!
//! On Linux kernels with `metacopy=on` enabled, `chmod`, `chown`, `utimensat`,
//! and `rename` only write tiny metadata inodes to `upperdir` without copying
//! file contents; `open(O_WRONLY | O_RDWR)` triggers standard kernel copy-up
//! from the FUSE lower layer (or directly from the `<git-dir>/blobs/<xx>/<oid>`
//! backing file when `FUSE_PASSTHROUGH` is active).

use std::path::{Path, PathBuf};

/// Materialized directory layout under a writable scratch root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayScratchDirs {
    /// Internal mountpoint where the read-only `projgit` FUSE projection is mounted.
    pub lower_dir: PathBuf,
    /// Writable upper layer receiving copy-ups, new files, and whiteouts.
    pub upper_dir: PathBuf,
    /// Kernel `overlayfs` atomic work directory (must reside on the same filesystem as `upper_dir`).
    pub work_dir: PathBuf,
}

/// Configuration for stacking a Linux `overlayfs` writable layer on top of a
/// read-only `projgit` mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayScratchConfig {
    /// Root scratch directory containing `lower/`, `upper/`, and `work/`.
    pub scratch_dir: PathBuf,
    /// Request `metacopy=on` so metadata-only mutations (`chmod`, `mv`) avoid data copy-up.
    pub metacopy: bool,
    /// Use `user.overlay.*` xattrs (`userxattr`) for unprivileged user-namespace mounts (Linux 5.11+).
    pub userxattr: bool,
}

impl OverlayScratchConfig {
    /// Create a new writable scratch configuration rooted at `scratch_dir`.
    ///
    /// Defaults to `metacopy: true` and `userxattr: false`.
    pub fn new(scratch_dir: impl Into<PathBuf>) -> Self {
        Self {
            scratch_dir: scratch_dir.into(),
            metacopy: true,
            userxattr: false,
        }
    }

    /// Compute the [`OverlayScratchDirs`] paths under [`Self::scratch_dir`] without touching disk.
    pub fn dirs(&self) -> OverlayScratchDirs {
        OverlayScratchDirs {
            lower_dir: self.scratch_dir.join("lower"),
            upper_dir: self.scratch_dir.join("upper"),
            work_dir: self.scratch_dir.join("work"),
        }
    }

    /// Create `lower/`, `upper/`, and `work/` under [`Self::scratch_dir`] on disk.
    pub fn prepare_dirs(&self) -> std::io::Result<OverlayScratchDirs> {
        let dirs = self.dirs();
        std::fs::create_dir_all(&dirs.lower_dir)?;
        std::fs::create_dir_all(&dirs.upper_dir)?;
        std::fs::create_dir_all(&dirs.work_dir)?;
        Ok(dirs)
    }

    /// Format the `-o` option string for `mount -t overlay overlay -o <opts> <mountpoint>`.
    pub fn overlay_mount_options(&self, lower_dir: &Path) -> String {
        let dirs = self.dirs();
        let mut opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            escape_mount_opt_path(lower_dir),
            escape_mount_opt_path(&dirs.upper_dir),
            escape_mount_opt_path(&dirs.work_dir),
        );
        if self.metacopy {
            opts.push_str(",metacopy=on");
        }
        if self.userxattr {
            opts.push_str(",userxattr");
        }
        opts
    }

    /// Return the ordered fallback sequence of `overlayfs` `-o` option strings to attempt on Linux:
    ///
    /// 1. Configured options (including `metacopy=on` when enabled).
    /// 2. Standard `lowerdir=...,upperdir=...,workdir=...` (for hosts where the `overlay` module
    ///    has `metacopy=N` and rejects `metacopy=on`).
    /// 3. `lowerdir=...,upperdir=...,workdir=...,userxattr` (for rootless user-namespace mounts).
    pub fn fallback_mount_options_chain(&self, lower_dir: &Path) -> Vec<String> {
        let mut chain = Vec::with_capacity(3);
        let primary = self.overlay_mount_options(lower_dir);
        chain.push(primary);

        let plain = Self {
            scratch_dir: self.scratch_dir.clone(),
            metacopy: false,
            userxattr: false,
        }
        .overlay_mount_options(lower_dir);
        if !chain.contains(&plain) {
            chain.push(plain);
        }

        let userns = Self {
            scratch_dir: self.scratch_dir.clone(),
            metacopy: false,
            userxattr: true,
        }
        .overlay_mount_options(lower_dir);
        if !chain.contains(&userns) {
            chain.push(userns);
        }
        chain
    }

    /// Format a `composefs`-style `overlayfs` option string combining a metadata lower layer
    /// (`manifest_dir`) with a data-only lower layer (`::` + `blobs_cas_dir`) and this scratch
    /// directory's `upperdir` / `workdir`.
    pub fn composefs_mount_options(&self, manifest_dir: &Path, blobs_cas_dir: &Path) -> String {
        let dirs = self.dirs();
        let mut opts = format!(
            "metacopy=on,redirect_dir=follow,lowerdir={}::{},upperdir={},workdir={}",
            escape_mount_opt_path(manifest_dir),
            escape_mount_opt_path(blobs_cas_dir),
            escape_mount_opt_path(&dirs.upper_dir),
            escape_mount_opt_path(&dirs.work_dir),
        );
        if self.userxattr {
            opts.push_str(",userxattr");
        }
        opts
    }
}

fn escape_mount_opt_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace(':', "\\:")
        .replace(',', "\\,")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_dirs_creates_lower_upper_and_work() {
        let tmp = std::env::temp_dir().join(format!(
            "projgit-overlay-scratch-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let cfg = OverlayScratchConfig::new(&tmp);
        let dirs = cfg.prepare_dirs().expect("prepare_dirs");
        assert!(dirs.lower_dir.is_dir());
        assert!(dirs.upper_dir.is_dir());
        assert!(dirs.work_dir.is_dir());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn mount_options_and_fallback_chain_are_well_formed() {
        let cfg = OverlayScratchConfig::new("/tmp/scratch");
        let dirs = cfg.dirs();
        let opts = cfg.overlay_mount_options(&dirs.lower_dir);
        assert_eq!(
            opts,
            "lowerdir=/tmp/scratch/lower,upperdir=/tmp/scratch/upper,workdir=/tmp/scratch/work,metacopy=on"
        );

        let chain = cfg.fallback_mount_options_chain(&dirs.lower_dir);
        assert_eq!(chain.len(), 3);
        assert!(chain[0].ends_with(",metacopy=on"));
        assert!(!chain[1].contains("metacopy"));
        assert!(chain[2].ends_with(",userxattr"));

        let cfs = cfg.composefs_mount_options(Path::new("/mnt/manifest"), Path::new("/git/blobs"));
        assert_eq!(
            cfs,
            "metacopy=on,redirect_dir=follow,lowerdir=/mnt/manifest::/git/blobs,upperdir=/tmp/scratch/upper,workdir=/tmp/scratch/work"
        );
    }
}
