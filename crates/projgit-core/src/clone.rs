//! One-time partial-clone helper.
//!
//! Pragmatic choice for MVP: shell out to the system `git` for the
//! initial `git clone --filter=blob:none --no-checkout`. The
//! URL-mount hot path also shells out through [`crate::GitCliFetcher`]
//! so it can use Git's native partial-clone promisor behavior. The
//! experimental [`crate::GixFetcher`] stays available behind the
//! `gix-fetcher` feature for callers that want a native-Rust transport
//! path.
//!
//! Documented as a deliberate trade-off in `docs/implementation/initial-plan.md` §5.4.

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
    /// If `Some(N)`, pass `--depth=N` to truncate the local history
    /// to the last `N` commits. `Some(1)` is the load-bearing case:
    /// a shallow partial clone that pulls only metadata for the
    /// current snapshot (trees + commit at tip), no history. Orders
    /// of magnitude smaller on deep-history repos (years of commits
    /// + millions of trees).
    ///
    /// **Tradeoff:** shallow clones can serve `cat` / `ls` of the
    /// current snapshot fine, but `git log`, `git blame`,
    /// `git diff <older>`, and `git checkout <older>` won't work
    /// inside a projection built on top — there's no history to
    /// walk. Right default for Harbor-style eval/build agents that
    /// only need the current snapshot; wrong for agents that need
    /// to walk history.
    ///
    /// Defaults to `None` (full history) so existing tests and
    /// history-walking workloads keep working. Harbor-style
    /// deployments opt in via `--depth=1` on the CLI / daemon.
    pub depth: Option<u32>,
}

impl CloneOptions {
    /// Construct with the projgit defaults: blob:none, --no-checkout, non-bare,
    /// full history.
    pub fn new(url: impl Into<String>, destination: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            destination: destination.into(),
            filter: "blob:none".to_owned(),
            no_checkout: true,
            bare: false,
            depth: None,
        }
    }

    /// Builder: pull only `n` commits of history (`--depth=n`).
    /// `n` must be > 0 (git rejects `--depth=0`).
    pub fn with_depth(mut self, n: u32) -> Self {
        assert!(n > 0, "CloneOptions::with_depth(0) is not supported; git rejects --depth=0");
        self.depth = Some(n);
        self
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

    let mut cmd = build_clone_command(opts);

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

/// Build the `git clone` command for `opts`. Extracted from
/// [`partial_clone`] so tests can inspect the constructed argv
/// without spawning a child process.
fn build_clone_command(opts: &CloneOptions) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("clone");
    cmd.arg(format!("--filter={}", opts.filter));
    if opts.no_checkout {
        cmd.arg("--no-checkout");
    }
    if opts.bare {
        cmd.arg("--bare");
    }
    if let Some(n) = opts.depth {
        cmd.arg(format!("--depth={n}"));
    }
    cmd.arg(&opts.url);
    cmd.arg(&opts.destination);
    cmd
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect a `Command`'s argv into a `Vec<String>` for assertion.
    fn argv(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn default_clone_command_is_full_history() {
        let opts = CloneOptions::new("https://example.invalid/repo.git", "/tmp/x");
        let cmd = build_clone_command(&opts);
        let args = argv(&cmd);
        assert_eq!(
            args,
            vec![
                "clone".to_owned(),
                "--filter=blob:none".to_owned(),
                "--no-checkout".to_owned(),
                "https://example.invalid/repo.git".to_owned(),
                "/tmp/x".to_owned(),
            ]
        );
        // No --depth in the default args.
        assert!(!args.iter().any(|a| a.starts_with("--depth")));
    }

    #[test]
    fn with_depth_emits_depth_flag() {
        let opts =
            CloneOptions::new("https://example.invalid/repo.git", "/tmp/x").with_depth(1);
        let cmd = build_clone_command(&opts);
        let args = argv(&cmd);
        assert!(
            args.contains(&"--depth=1".to_owned()),
            "expected --depth=1 in argv, got {args:?}"
        );
    }

    #[test]
    fn with_depth_n_emits_corresponding_flag() {
        let opts =
            CloneOptions::new("https://example.invalid/repo.git", "/tmp/x").with_depth(42);
        let cmd = build_clone_command(&opts);
        let args = argv(&cmd);
        assert!(
            args.contains(&"--depth=42".to_owned()),
            "expected --depth=42 in argv, got {args:?}"
        );
    }

    #[test]
    #[should_panic(expected = "with_depth(0)")]
    fn with_depth_zero_panics() {
        // git rejects `--depth=0`; catch the mistake at the call
        // site rather than producing a confusing git error later.
        let _ = CloneOptions::new("https://example.invalid/repo.git", "/tmp/x").with_depth(0);
    }
}
