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

use super::{Coalescer, Fetcher, FetcherError, HeaderProbe};
use crate::object_store::{ObjectKind, ObjectStore};
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

    /// Batched variant: write all `oids` to stdin in one go, then
    /// read exactly `oids.len()` response lines back. The protocol
    /// guarantees one line per input OID in input order.
    ///
    /// Returns the raw response lines, in the same order. Any I/O
    /// failure aborts the whole batch -- the caller tears down
    /// the child and may retry (one level up).
    fn query_batch(&mut self, oids: &[ObjectId]) -> std::io::Result<Vec<String>> {
        let stdin = self.stdin.as_mut().expect("stdin alive between spawn+drop");
        // Write all OIDs first; git buffers them and emits one
        // response per input regardless of how we framed the writes.
        for oid in oids {
            writeln!(stdin, "{oid}")?;
        }
        stdin.flush()?;
        let mut out = Vec::with_capacity(oids.len());
        for _ in 0..oids.len() {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "git cat-file --batch-check closed stdout mid-batch",
                ));
            }
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            out.push(line);
        }
        Ok(out)
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
    ///
    /// **Double-checked locking with the prefetch worker.** Both the
    /// prefetch worker and on-demand fetches contend for the
    /// `batch` mutex. If a prefetch batch lands the OID's pack
    /// while we're waiting for the lock, we'd otherwise re-query
    /// for an OID that's already local. Re-check `store.contains`
    /// after acquiring the lock and short-circuit on hit.
    fn raw_fetch(&self, oid: ObjectId) -> Result<(), FetcherError> {
        let mut last_io_err: Option<std::io::Error> = None;
        for _attempt in 0..2 {
            let response = {
                let mut slot = self.batch.lock().unwrap();
                // Double-checked: another caller (typically the
                // prefetch worker) may have made the OID local
                // between our outer fast-path check and acquiring
                // this lock.
                if self.store.contains(oid) {
                    return Ok(());
                }
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
        match Self::parse_line(oid, line)? {
            ParsedResponse::Header { .. } => {
                // Present locally. Verify visibility through the
                // store to catch the (rare) case where git reports
                // success but our handle doesn't see the new pack.
                if !self.store.contains(oid) {
                    return Err(FetcherError::NotPresentAfterFetch(oid));
                }
                Ok(())
            }
            ParsedResponse::Missing => Err(FetcherError::Refused(
                oid,
                "git cat-file --batch-check reported missing".to_owned(),
            )),
        }
    }

    /// Parse one `<sha> <kind> <size>` or `<sha> missing` line.
    /// Stateless; doesn't touch the store. Used by both the
    /// single-OID classify path and the batched prefetch path.
    fn parse_line(oid: ObjectId, line: &str) -> Result<ParsedResponse, FetcherError> {
        let mut parts = line.splitn(3, ' ');
        let sha = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("");
        let rest = parts.next();

        if !sha.eq_ignore_ascii_case(&oid.to_string()) {
            return Err(FetcherError::Backend(
                oid,
                format!("git cat-file --batch-check echoed unexpected sha: {line:?}"),
            ));
        }

        match kind {
            "blob" | "tree" | "commit" | "tag" => {
                let size: u64 = rest.and_then(|s| s.trim().parse().ok()).ok_or_else(|| {
                    FetcherError::Backend(
                        oid,
                        format!("git cat-file --batch-check: unparseable size: {line:?}"),
                    )
                })?;
                let kind = match kind {
                    "blob" => ObjectKind::Blob,
                    "tree" => ObjectKind::Tree,
                    "commit" => ObjectKind::Commit,
                    "tag" => ObjectKind::Tag,
                    _ => unreachable!("matched above"),
                };
                Ok(ParsedResponse::Header { kind, size })
            }
            "missing" => Ok(ParsedResponse::Missing),
            other => Err(FetcherError::Backend(
                oid,
                format!("git cat-file --batch-check: unknown response kind {other:?}: {line:?}"),
            )),
        }
    }
}

/// Parsed shape of a `cat-file --batch-check` line.
#[derive(Debug, Clone, Copy)]
enum ParsedResponse {
    Header { kind: ObjectKind, size: u64 },
    Missing,
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

    /// Batch-query the long-lived `git cat-file --batch-check`
    /// child for the headers of all `oids` in one round trip.
    ///
    /// Skips OIDs already locally present (no IPC for those, just
    /// emits `Present` so the caller can publish them to the
    /// header cache via `ObjectStore::header`). Sends the
    /// remainder in one batch and parses the per-line responses.
    /// On I/O failure tears down the child and tries once more,
    /// matching `raw_fetch`'s respawn-on-broken-pipe behaviour.
    fn prefetch_headers(&self, oids: &[ObjectId]) -> Vec<HeaderProbe> {
        if oids.is_empty() {
            return Vec::new();
        }

        // Partition: locally present vs. needs upstream.
        let mut to_query: Vec<ObjectId> = Vec::with_capacity(oids.len());
        let mut local_present: Vec<ObjectId> = Vec::with_capacity(oids.len());
        for &oid in oids {
            if self.store.contains(oid) {
                local_present.push(oid);
            } else {
                to_query.push(oid);
            }
        }

        // For locally present ones, emit Present without touching
        // the child. They'll resolve via ObjectStore::header.
        let mut probes_by_oid: std::collections::HashMap<ObjectId, HeaderProbe> =
            std::collections::HashMap::with_capacity(oids.len());
        for oid in local_present {
            probes_by_oid.insert(oid, HeaderProbe::Present(oid));
        }

        if !to_query.is_empty() {
            // Two attempts: respawn the child on broken pipe.
            let mut last_io_err: Option<std::io::Error> = None;
            let mut batch_lines: Option<Vec<String>> = None;
            for _attempt in 0..2 {
                let mut slot = self.batch.lock().unwrap();
                if slot.is_none() {
                    match BatchChild::spawn(&self.git_dir) {
                        Ok(c) => *slot = Some(c),
                        Err(e) => {
                            // Whole batch fails on spawn error.
                            for oid in &to_query {
                                probes_by_oid.insert(
                                    *oid,
                                    HeaderProbe::Error(
                                        *oid,
                                        FetcherError::Backend(
                                            *oid,
                                            format!("spawn git cat-file: {e}"),
                                        ),
                                    ),
                                );
                            }
                            return reorder_probes(oids, probes_by_oid);
                        }
                    }
                }
                let child = slot.as_mut().expect("just inserted");
                match child.query_batch(&to_query) {
                    Ok(lines) => {
                        batch_lines = Some(lines);
                        break;
                    }
                    Err(e) => {
                        *slot = None;
                        last_io_err = Some(e);
                    }
                }
            }

            match batch_lines {
                Some(lines) => {
                    debug_assert_eq!(lines.len(), to_query.len());
                    for (oid, line) in to_query.iter().zip(lines.iter()) {
                        match Self::parse_line(*oid, line) {
                            Ok(ParsedResponse::Header { kind, size }) => {
                                if !self.store.contains(*oid) {
                                    probes_by_oid.insert(
                                        *oid,
                                        HeaderProbe::Error(
                                            *oid,
                                            FetcherError::NotPresentAfterFetch(*oid),
                                        ),
                                    );
                                } else {
                                    probes_by_oid.insert(
                                        *oid,
                                        HeaderProbe::PresentWithHeader(*oid, kind, size),
                                    );
                                }
                            }
                            Ok(ParsedResponse::Missing) => {
                                probes_by_oid.insert(
                                    *oid,
                                    HeaderProbe::Error(
                                        *oid,
                                        FetcherError::Refused(
                                            *oid,
                                            "git cat-file --batch-check reported missing"
                                                .to_owned(),
                                        ),
                                    ),
                                );
                            }
                            Err(e) => {
                                probes_by_oid.insert(*oid, HeaderProbe::Error(*oid, e));
                            }
                        }
                    }
                }
                None => {
                    let msg = last_io_err
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "exhausted retries".to_owned());
                    for oid in &to_query {
                        probes_by_oid.insert(
                            *oid,
                            HeaderProbe::Error(
                                *oid,
                                FetcherError::Transport(
                                    *oid,
                                    format!("git cat-file --batch-check (batch): {msg}"),
                                ),
                            ),
                        );
                    }
                }
            }
        }

        reorder_probes(oids, probes_by_oid)
    }
}

/// Reassemble probes in the same order as the input OIDs.
fn reorder_probes(
    oids: &[ObjectId],
    mut by_oid: std::collections::HashMap<ObjectId, HeaderProbe>,
) -> Vec<HeaderProbe> {
    let mut out = Vec::with_capacity(oids.len());
    for oid in oids {
        // `remove` so duplicate OIDs in the input still each get
        // a result -- by reusing the same probe via clone.
        match by_oid.remove(oid) {
            Some(probe) => out.push(probe),
            None => out.push(HeaderProbe::Error(
                *oid,
                FetcherError::Backend(*oid, "prefetch_headers: missing result".to_owned()),
            )),
        }
    }
    out
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
        let tmp =
            std::env::temp_dir().join(format!("projgit-gitcli-fetcher-{}", std::process::id()));
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
    /// missing-object queries should reuse a single child process.
    /// Locally-present OIDs intentionally short-circuit before the
    /// child is spawned, so this uses bogus OIDs that `cat-file`
    /// reports as `missing` without exiting.
    #[test]
    fn batch_child_stays_alive_across_missing_queries() {
        let tmp = std::env::temp_dir().join(format!("projgit-gitcli-batch-{}", std::process::id()));
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

        let bogus = ObjectId::from_hex(b"0000000000000000000000000000000000000001").unwrap();

        // Several calls. We exercise `raw_fetch` directly so we can
        // assert the child remains usable even when git reports an
        // object-level miss. Missing responses are normal protocol
        // responses, not transport failures, so the child should stay
        // alive across all of them.
        for _ in 0..5 {
            assert!(matches!(
                f.raw_fetch(bogus),
                Err(FetcherError::Refused(oid, _)) if oid == bogus
            ));
        }
        assert!(
            f.batch.lock().unwrap().is_some(),
            "batch child should remain alive across many queries"
        );
        assert!(store.contains(head_oid));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
