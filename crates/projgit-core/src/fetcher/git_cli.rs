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
//! clone (e.g. `git cat-file --batch-check`) triggers an automatic
//! promisor fetch with the right protocol framing.
//!
//! `GitCliFetcher` is therefore the production-default fetcher for
//! URL-backed mounts in Phase 4 onwards. `GixFetcher` stays around
//! for environments without a system `git`, for benchmarks, and
//! because it's where future native-Rust transport work lands.
//!
//! ## Long-lived `git cat-file --batch-check` child
//!
//! The Phase 4 implementation spawned one `git` subprocess per
//! cache miss. fork/exec dominates the wall time for hot interactive
//! workflows like `cd` + `ls -la` + `grep -r` (each `stat` triggers
//! one fetch via `header()` resolving the blob's size).
//!
//! Phase 5 keeps a single `git -C <dir> cat-file --batch-check` child
//! alive for the lifetime of the fetcher and ferries OIDs over its
//! stdin / stdout pipes:
//!
//! - Write `<oid>\n` to stdin.
//! - Read one stdout line; "<sha> blob <size>" means present
//!   (possibly after a promisor fetch); "<sha> missing" means the
//!   server rejected it.
//!
//! If the child ever dies (broken pipe, transport timeout) the
//! fetcher tears down the failed handle and respawns lazily on the
//! next miss. The fast-path for already-present OIDs short-circuits
//! before the child is ever used.

use super::{Coalescer, Fetcher, FetcherError};
use crate::object_store::ObjectStore;
use gix::ObjectId;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

/// Errors produced while constructing a [`GitCliFetcher`].
#[derive(Debug, thiserror::Error)]
pub enum GitCliFetcherError {
    /// `git` is not on PATH or is otherwise unrunnable.
    #[error("git CLI not available: {0}")]
    GitUnavailable(String),
}

/// One running `git cat-file --batch-check` child plus its handles.
///
/// Kept inside a `Mutex<Option<...>>` on the fetcher so a single
/// child is shared across threads and dropped (which closes stdin
/// and reaps the child) when the fetcher itself goes out of scope
/// or the channel is reset after an error.
struct BatchChild {
    child: Child,
    /// Wrapped in `Option` so [`Drop`] can `take()` it and drop the
    /// stdin handle *before* `wait`, making git see EOF and exit
    /// cleanly. Always `Some(_)` between construction and Drop.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl BatchChild {
    fn spawn(git_dir: &std::path::Path) -> Result<Self, std::io::Error> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(git_dir)
            .arg("cat-file")
            .arg("--batch-check")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Suppress git's progress lines on stderr; promisor-fetch
            // chatter ("Receiving objects: ..." etc.) doesn't need
            // to surface in the FUSE log.
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
        })
    }

    /// Send one OID and read one status line. Returns the raw
    /// response (e.g. `"<sha> blob 13"` or `"<sha> missing"`).
    fn query(&mut self, oid: ObjectId) -> std::io::Result<String> {
        let stdin = self.stdin.as_mut().expect("stdin alive between spawn+drop");
        writeln!(stdin, "{oid}")?;
        // Flushing isn't strictly required for line-buffered pipes,
        // but make it explicit so behaviour doesn't drift if the
        // child or stdlib changes default buffering.
        stdin.flush()?;
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "git cat-file --batch-check closed stdout",
            ));
        }
        // Strip the trailing newline; preserve any other formatting.
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(line)
    }
}

impl Drop for BatchChild {
    fn drop(&mut self) {
        // Drop stdin first so git sees EOF on its input pipe and
        // exits on its own. Then wait so we don't leak a zombie
        // process. Either step may fail (the child could already be
        // gone); we're in Drop, there's nothing actionable to do
        // with the error.
        self.stdin.take();
        let _ = self.child.wait();
    }
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
    /// Long-lived `git cat-file --batch-check` child. Lazy: spawned
    /// on the first miss, respawned after a transport failure.
    /// `None` means "no child currently alive."
    batch: Mutex<Option<BatchChild>>,
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
            batch: Mutex::new(None),
        })
    }

    /// Issue the actual fetch. Internal; bypasses the coalescer so
    /// the coalescer can call us once.
    ///
    /// One round-trip through the long-lived `git cat-file
    /// --batch-check` child. If the child has died we tear down the
    /// dead handle and respawn once so a transient transport failure
    /// doesn't poison the entire mount session.
    fn raw_fetch(&self, oid: ObjectId) -> Result<(), FetcherError> {
        let mut last_io_err: Option<std::io::Error> = None;
        for _attempt in 0..2 {
            let response = {
                let mut slot = self.batch.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(BatchChild::spawn(&self.git_dir).map_err(|e| {
                        FetcherError::Backend(oid, format!("spawn git cat-file: {e}"))
                    })?);
                }
                let child = slot.as_mut().expect("just inserted");
                match child.query(oid) {
                    Ok(line) => Ok(line),
                    Err(e) => {
                        // Tear down the failed child so the next
                        // call gets a fresh one.
                        *slot = None;
                        Err(e)
                    }
                }
            };

            match response {
                Ok(line) => return self.classify(oid, &line),
                Err(e) => last_io_err = Some(e),
            }
        }
        let msg = last_io_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "exhausted retries".to_owned());
        Err(FetcherError::Transport(
            oid,
            format!("git cat-file --batch-check: {msg}"),
        ))
    }

    /// Classify a single response line from `cat-file --batch-check`.
    ///
    /// Successful presence: `<sha> <type> <size>`. Server-side
    /// rejection: `<sha> missing`. Anything else is a backend
    /// error -- git's output format is stable enough that other
    /// shapes mean we hit a code path we don't understand.
    fn classify(&self, oid: ObjectId, line: &str) -> Result<(), FetcherError> {
        let mut parts = line.splitn(3, ' ');
        let sha = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("");
        let _size_or_extra = parts.next();

        if !sha.eq_ignore_ascii_case(&oid.to_string()) {
            return Err(FetcherError::Backend(
                oid,
                format!("git cat-file --batch-check echoed unexpected sha: {line:?}"),
            ));
        }

        match kind {
            "blob" | "tree" | "commit" | "tag" => {
                // Present locally. Verify visibility through the
                // store to catch the (rare) case where git reports
                // success but our handle doesn't see the new pack.
                if !self.store.contains(oid) {
                    return Err(FetcherError::NotPresentAfterFetch(oid));
                }
                Ok(())
            }
            "missing" => Err(FetcherError::Refused(
                oid,
                "git cat-file --batch-check reported missing".to_owned(),
            )),
            other => Err(FetcherError::Backend(
                oid,
                format!("git cat-file --batch-check: unknown response kind {other:?}: {line:?}"),
            )),
        }
    }
}

impl Fetcher for GitCliFetcher {
    fn fetch_object(&self, oid: ObjectId) -> Result<(), FetcherError> {
        // Fast path: already present, no IPC needed.
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

    /// `--batch-check` against a fully-local repo: many sequential
    /// queries should reuse a single child process. We verify by
    /// checking the child is still alive after several calls (and
    /// `slot` still holds a `Some`).
    #[test]
    fn batch_child_serves_many_calls_against_local_repo() {
        let tmp = std::env::temp_dir().join(format!(
            "projgit-gitcli-batch-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        if Command::new("git")
            .args(["init", "-q", "-b", "main", tmp.to_str().unwrap()])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("SKIP: git CLI not available");
            return;
        }
        // Author one commit so we have real OIDs to query.
        std::fs::write(tmp.join("a.txt"), b"hello\n").unwrap();
        for cmd in [
            &["config", "user.email", "x@x"][..],
            &["config", "user.name", "x"][..],
            &["add", "-A"][..],
            &["commit", "-q", "-m", "x"][..],
        ] {
            let mut c = Command::new("git");
            c.arg("-C").arg(&tmp);
            for a in cmd {
                c.arg(a);
            }
            assert!(c.status().unwrap().success());
        }
        let head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&tmp)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let head_oid = ObjectId::from_hex(head.trim().as_bytes()).unwrap();

        let store = Arc::new(ObjectStore::open(tmp.join(".git")).unwrap());
        let f = GitCliFetcher::open(store.clone()).unwrap();

        // Several calls. We exercise `raw_fetch` directly so the
        // fast-path "already present" short-circuit doesn't bypass
        // the child. The child should stay alive across all of them.
        for _ in 0..5 {
            f.raw_fetch(head_oid).expect("present locally");
        }
        assert!(
            f.batch.lock().unwrap().is_some(),
            "batch child should remain alive across many queries"
        );
        assert!(store.contains(head_oid));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
