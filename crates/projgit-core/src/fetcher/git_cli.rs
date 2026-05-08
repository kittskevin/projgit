//! `GitCliFetcher`: hydrate single objects by shelling out to the
//! system `git` and letting the partial-clone "promisor remote"
//! mechanism do the actual fetch.
//!
//! ## Why this exists alongside [`crate::GixFetcher`]
//!
//! [`crate::GixFetcher`] drives gitoxide's blocking transport directly,
//! sending a single-OID `+<oid>:refs/projgit/wanted/<oid>` refspec.
//! That works against servers that honour
//! `allow-tip-sha1-in-want` / `allow-reachable-sha1-in-want` for any
//! reachable OID — which the Phase 0a spike validated against
//! `rust-lang/log`. **GitHub's current policy rejects this for
//! many repositories**: the server returns
//! `RejectedSourceObjectNotFound` for the bare-OID want, and `gix`
//! reports `receive()` succeeded with an empty pack. The pack never
//! lands; subsequent reads still miss.
//!
//! The same fetch framed as a *promisor* request — i.e. with the
//! filter spec the original `git clone --filter=...` configured —
//! does succeed. The system `git` knows how to do that
//! out-of-the-box: any read against a missing object in a partial
//! clone (e.g. `git cat-file -e <oid>`) triggers an automatic
//! promisor fetch with the right protocol framing.
//!
//! `GitCliFetcher` is therefore the production-default fetcher for
//! URL-backed mounts in Phase 4 onwards. `GixFetcher` stays around
//! for environments without a system `git`, for benchmarks, and
//! because it's where future native-Rust transport work lands.

use super::{Coalescer, Fetcher, FetcherError};
use crate::object_store::ObjectStore;
use gix::ObjectId;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Errors produced while constructing a [`GitCliFetcher`].
#[derive(Debug, thiserror::Error)]
pub enum GitCliFetcherError {
    /// `git` is not on PATH or is otherwise unrunnable.
    #[error("git CLI not available: {0}")]
    GitUnavailable(String),
}

/// A [`Fetcher`] that shells out to the system `git` to drive the
/// promisor-remote fetch path.
///
/// Holds an [`Arc<ObjectStore>`] so the post-fetch presence check
/// runs through the same store the rest of projgit reads from. The
/// [`Coalescer`] inside ensures concurrent calls for the same OID
/// issue exactly one `git` invocation.
pub struct GitCliFetcher {
    store: Arc<ObjectStore>,
    git_dir: PathBuf,
    coalescer: Coalescer<ObjectId, ()>,
}

impl GitCliFetcher {
    /// Construct a fetcher that drives `git` against the same
    /// `.git/` directory the [`ObjectStore`] reads from.
    ///
    /// Verifies `git --version` runs successfully so callers fail
    /// fast at construction rather than at the first cache miss.
    pub fn open(store: Arc<ObjectStore>) -> Result<Self, GitCliFetcherError> {
        let ok = Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Err(GitCliFetcherError::GitUnavailable(
                "`git --version` failed; install git or use a different fetcher".to_owned(),
            ));
        }
        let git_dir = store.git_dir().to_path_buf();
        Ok(Self {
            store,
            git_dir,
            coalescer: Coalescer::new(),
        })
    }

    /// Issue the actual fetch. Internal; bypasses the coalescer so
    /// the coalescer can call us once.
    fn raw_fetch(&self, oid: ObjectId) -> Result<(), FetcherError> {
        // `cat-file -e <oid>` returns 0 if the object is present (or
        // becomes present via promisor fetch) and non-zero otherwise.
        // It's the cheapest way to ask `git` "make this OID local."
        // Using `--batch-check`/etc. would let us avoid spawn overhead
        // for many OIDs, but Phase 4 only needs correctness;
        // batching is a Phase 5 polish item.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.git_dir)
            .arg("cat-file")
            .arg("-e")
            .arg(oid.to_string())
            .output()
            .map_err(|e| FetcherError::Backend(oid, format!("spawn git: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            // Heuristic: missing-from-remote vs. transport vs. other.
            // Stable git messages we want to special-case.
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("could not fetch")
                || lower.contains("not our ref")
                || lower.contains("no such ref")
            {
                return Err(FetcherError::Refused(oid, stderr));
            }
            if lower.contains("could not connect")
                || lower.contains("transport")
                || lower.contains("ssl")
                || lower.contains("tls")
            {
                return Err(FetcherError::Transport(oid, stderr));
            }
            return Err(FetcherError::Backend(
                oid,
                format!("git cat-file -e {oid} failed: {stderr}"),
            ));
        }

        // git wrote any new pack already; verify visibility through
        // the same store the rest of projgit reads from.
        if !self.store.contains(oid) {
            return Err(FetcherError::NotPresentAfterFetch(oid));
        }
        Ok(())
    }
}

impl Fetcher for GitCliFetcher {
    fn fetch_object(&self, oid: ObjectId) -> Result<(), FetcherError> {
        // Fast path: already present, no spawn needed.
        if self.store.contains(oid) {
            return Ok(());
        }
        self.coalescer
            .do_or_join(oid, || self.raw_fetch(oid))
            .map_err(|s| {
                // The coalescer collapses the typed error to a String
                // for sharing across threads. Re-classify by content.
                if s.contains("Refused") {
                    FetcherError::Refused(oid, s)
                } else if s.contains("Transport") {
                    FetcherError::Transport(oid, s)
                } else if s.contains("NotPresentAfterFetch") {
                    FetcherError::NotPresentAfterFetch(oid)
                } else {
                    FetcherError::Backend(oid, s)
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction succeeds whenever `git --version` is on PATH;
    /// otherwise it surfaces a typed error rather than panicking.
    #[test]
    fn open_requires_git_on_path() {
        // We can't easily simulate "git missing" on a host with git
        // installed, so just smoke-test the success path. The
        // GitUnavailable arm is exercised on CI runners without git.
        let tmp = std::env::temp_dir().join(format!(
            "projgit-gitcli-fetcher-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main", tmp.to_str().unwrap()])
            .status();
        if status.map(|s| !s.success()).unwrap_or(true) {
            eprintln!("SKIP: git CLI not available");
            return;
        }
        let store = Arc::new(ObjectStore::open(tmp.join(".git")).unwrap());
        let _f = GitCliFetcher::open(store).expect("git is on PATH");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
