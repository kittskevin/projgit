//! One-time partial-clone helper.
//!
//! Pragmatic choice for MVP: shell out to the system `git` for the
//! initial `git clone --filter=blob:none --no-checkout`. The
//! *hot path* (per-blob hydration) goes through gitoxide for full
//! control, but the one-time setup path is much shorter to write
//! correctly via `git` and inherits the user's credential helpers
//! without us having to plumb anything.
//!
//! Documented as a deliberate trade-off in `docs/initial-plan.md` §5.4.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Errors from the partial-clone helper.
#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    /// `git` is not on PATH or otherwise unrunnable.
    #[error("could not invoke git: {0}")]
    GitUnavailable(String),

    /// `git clone` exited non-zero.
    #[error("git clone failed (exit code {code:?}): {stderr}")]
    GitFailed {
        /// Process exit code, if any.
        code: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },

    /// I/O error when preparing the destination directory.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Options for [`partial_clone`].
#[derive(Debug, Clone)]
pub struct CloneOptions {
    /// Remote URL to clone from.
    pub url: String,
    /// Local destination path.
    pub destination: PathBuf,
    /// Object filter spec for `--filter=`. Defaults to `blob:none`,
    /// which skips all blobs (we'll fetch them on demand).
    pub filter: String,
    /// If `true`, pass `--no-checkout` so git doesn't materialize a
    /// working tree we'll never use. Defaults to `true`.
    pub no_checkout: bool,
    /// If `true`, pass `--bare`. Defaults to `false` so the resulting
    /// directory is a normal `<dest>/.git` layout that gix opens
    /// without further hints.
    pub bare: bool,
}

impl CloneOptions {
    /// Construct with the projgit defaults: blob:none, --no-checkout, non-bare.
    pub fn new(url: impl Into<String>, destination: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            destination: destination.into(),
            filter: "blob:none".to_owned(),
            no_checkout: true,
            bare: false,
        }
    }
}

/// Run `git clone` with the partial-clone options needed to back a
/// projgit object store.
///
/// Returns the destination path on success.
pub fn partial_clone(opts: &CloneOptions) -> Result<PathBuf, CloneError> {
    if let Some(parent) = opts.destination.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut cmd = Command::new("git");
    cmd.arg("clone");
    cmd.arg(format!("--filter={}", opts.filter));
    if opts.no_checkout {
        cmd.arg("--no-checkout");
    }
    if opts.bare {
        cmd.arg("--bare");
    }
    cmd.arg(&opts.url);
    cmd.arg(&opts.destination);

    let output = cmd
        .output()
        .map_err(|e| CloneError::GitUnavailable(e.to_string()))?;
    if !output.status.success() {
        return Err(CloneError::GitFailed {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(opts.destination.clone())
}

/// Resolve the on-disk `.git` directory for a path returned by
/// [`partial_clone`]. Hides the bare-vs-non-bare distinction from
/// callers that just want the path to hand to [`crate::ObjectStore::open`].
pub fn git_dir_for(clone_dest: &Path) -> PathBuf {
    let dot_git = clone_dest.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        clone_dest.to_path_buf()
    }
}
