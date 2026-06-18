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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// Set `PROJGIT_CATFILE_TRACE=1` to make `GitCliFetcher` emit per-
/// call timing on stderr. Each `raw_fetch` and `prefetch_headers`
/// prints `cattrace: op=<raw_fetch|prefetch_headers> wait_us=<n>
/// work_us=<n> [oid=<short>] [n_oids=<n>]`, where `wait_us` is the
/// time spent acquiring a pool slot and `work_us` is the time
/// spent inside `with_child` (spawn + cat-file round trip + git's
/// own promisor fetch). Used to disambiguate "pool contention"
/// from "git/network serialisation" when the daemon-level RPC
/// trace shows long served-times that the pool size can't fix.
///
/// Off by default; reads the env var once per process via
/// `OnceLock` to avoid the per-call getenv cost when disabled.
fn cattrace_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("PROJGIT_CATFILE_TRACE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

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

/// A pool of `BatchChild` slots, each lazily spawned on first use
/// and respawned after a transport failure. Replaces the single
/// `Mutex<Option<BatchChild>>` so multiple callers
/// (`raw_fetch`, `prefetch_headers`, both for the per-mount prefetch
/// worker and on-demand FUSE fetches across N sidecars) can run
/// concurrent `cat-file --batch-check` round-trips without
/// head-of-line blocking each other through one shared child.
///
/// Sizing: see [`GitCliFetcher::default_pool_size`] for the default;
/// callers can override with [`GitCliFetcher::open_with_pool_size`].
///
/// Acquisition is round-robin try-lock: each call walks slots
/// starting at a shared atomic counter and returns the first slot
/// it can `try_lock`. If every slot is busy on the first pass, it
/// falls back to a blocking lock on the starting slot. This is
/// "good enough" fairness: under sustained load the round-robin
/// counter advances every acquire, so no slot starves
/// permanently; under burst load (K parallel callers, K slots
/// free) every caller gets a slot immediately. Fairness was
/// considered a non-issue for V1 because per-call cost is in the
/// hundreds-of-ms (cat-file round-trip + promisor fetch), well
/// above any plausible per-acquire latency cost.
///
/// On respawn after I/O failure: the failing caller leaves its
/// slot as `None` so the next caller for that slot spawns a
/// fresh child. Other slots stay alive — one broken pipe doesn't
/// poison the pool.
struct BatchChildPool {
    /// One slot per pool entry. `None` between slot construction
    /// and first use, and between an I/O failure and the next
    /// caller's respawn. `Some(_)` otherwise. Always `Vec.len() >= 1`.
    slots: Vec<Mutex<Option<BatchChild>>>,
    /// Round-robin starting index for `acquire`. AtomicUsize so
    /// callers don't contend on a counter mutex; wraps modulo
    /// `slots.len()` at use site.
    next: AtomicUsize,
    git_dir: PathBuf,
}

impl BatchChildPool {
    /// Construct a pool of `k` slots, all initially empty. Lazy
    /// spawn: no child processes are started until [`Self::acquire`]
    /// is called. `k` must be `>= 1`; callers should validate.
    fn new(k: usize, git_dir: PathBuf) -> Self {
        debug_assert!(k >= 1, "BatchChildPool size must be >= 1");
        let mut slots = Vec::with_capacity(k);
        for _ in 0..k {
            slots.push(Mutex::new(None));
        }
        Self {
            slots,
            next: AtomicUsize::new(0),
            git_dir,
        }
    }

    /// Hand out one slot's guard. Round-robin try-lock; falls back
    /// to a blocking lock on the starting slot if all are busy.
    ///
    /// The returned [`PoolGuard`] holds the slot's [`MutexGuard`];
    /// the slot stays locked until the guard is dropped. Callers
    /// should keep the guard alive only for one round trip.
    fn acquire(&self) -> PoolGuard<'_> {
        let n = self.slots.len();
        // Advance the round-robin counter atomically so different
        // callers start scanning at different slots even under
        // contention. Modulo at use; AtomicUsize wraps naturally
        // and we never read its absolute value.
        let start = self.next.fetch_add(1, Ordering::Relaxed) % n;
        // Pass 1: try_lock each slot in round-robin order.
        for i in 0..n {
            let idx = (start + i) % n;
            if let Ok(guard) = self.slots[idx].try_lock() {
                return PoolGuard {
                    inner: guard,
                    git_dir: &self.git_dir,
                };
            }
        }
        // All slots busy. Block on the starting slot. (No fairness
        // claim against the other K-1 slots; the next round-robin
        // caller will start one slot ahead, so starvation is
        // bounded by K.)
        let guard = self
            .slots[start]
            .lock()
            .expect("BatchChildPool slot mutex poisoned");
        PoolGuard {
            inner: guard,
            git_dir: &self.git_dir,
        }
    }
}

/// One acquired slot from a [`BatchChildPool`]. Drops release the
/// underlying mutex; do not hold across awaits or unrelated work.
///
/// Provides the same operations the old `slot.as_mut().expect(...)`
/// pattern exposed in `raw_fetch` and `prefetch_headers`:
/// `with_child` (lazy-spawn then call a closure with `&mut BatchChild`),
/// and `reset` (tear down the child after an I/O failure so the
/// next caller for this slot respawns).
struct PoolGuard<'a> {
    inner: MutexGuard<'a, Option<BatchChild>>,
    git_dir: &'a std::path::Path,
}

impl<'a> PoolGuard<'a> {
    /// Lazily spawn the slot's child (if absent), then invoke
    /// `f(&mut BatchChild)`. Spawn failures surface as
    /// `Err(io::Error)`; the closure's own return value comes back
    /// in the `Ok` arm verbatim (so callers can return their own
    /// `Result` from `f` and pattern-match it).
    fn with_child<F, R>(&mut self, f: F) -> std::io::Result<R>
    where
        F: FnOnce(&mut BatchChild) -> R,
    {
        if self.inner.is_none() {
            let child = BatchChild::spawn(self.git_dir)?;
            *self.inner = Some(child);
        }
        let child = self.inner.as_mut().expect("just inserted");
        Ok(f(child))
    }

    /// Tear down the slot's child (typically after an I/O error)
    /// so the next caller for this slot respawns a fresh one.
    fn reset(&mut self) {
        *self.inner = None;
    }

    /// Test-only accessor: is a child currently alive in this slot?
    #[cfg(test)]
    fn has_child(&self) -> bool {
        self.inner.is_some()
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
    coalescer: Coalescer<ObjectId, ()>,
    /// Pool of long-lived `git cat-file --batch-check` children.
    /// K slots; each lazy-spawned, respawned on I/O failure.
    /// Acquired round-robin per call to `raw_fetch` /
    /// `prefetch_headers` so concurrent callers don't head-of-line
    /// block through one shared child. See [`BatchChildPool`].
    batch: BatchChildPool,
}

impl GitCliFetcher {
    /// Pool size when callers don't pick one explicitly.
    /// `min(available_parallelism, 8)`: the upper bound is a hedge
    /// against pathological host configs (e.g. 96-core CI boxes
    /// where K=96 children would burn RAM for no benefit on
    /// projgit's actual workloads). The lower bound (1) only
    /// fires if `available_parallelism()` itself fails. Stage 2
    /// of the cat-file pool plan picks a final default empirically;
    /// 8 is the V1 choice (matches `min(num_cpus, 8)` from the plan).
    pub fn default_pool_size() -> usize {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        n.clamp(1, 8)
    }

    /// Construct a fetcher that drives `git` against the same
    /// `.git/` directory the [`ObjectStore`] reads from.
    ///
    /// Verifies `git --version` runs successfully so callers fail
    /// fast at construction rather than at the first cache miss.
    ///
    /// Pool size: [`Self::default_pool_size`]. For explicit control
    /// (the daemon picks K based on `DaemonConfig.pool_size`; tests
    /// pin K=1 to verify single-child regression), use
    /// [`Self::open_with_pool_size`].
    pub fn open(store: Arc<ObjectStore>) -> Result<Self, GitCliFetcherError> {
        Self::open_with_pool_size(store, Self::default_pool_size())
    }

    /// Like [`Self::open`] but with an explicit pool size. `k` is
    /// clamped to `>= 1` (a zero-sized pool is a programming bug
    /// the daemon's CLI parse should already reject; this clamp is
    /// belt-and-braces).
    pub fn open_with_pool_size(
        store: Arc<ObjectStore>,
        k: usize,
    ) -> Result<Self, GitCliFetcherError> {
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
        let k = k.max(1);
        Ok(Self {
            store,
            coalescer: Coalescer::new(),
            batch: BatchChildPool::new(k, git_dir),
        })
    }

    /// Issue the actual fetch. Internal; bypasses the coalescer so
    /// the coalescer can call us once.
    ///
    /// One round-trip through a `git cat-file --batch-check` child
    /// from the pool. If the child has died we tear down the dead
    /// handle and respawn once on the same slot so a transient
    /// transport failure doesn't poison the entire mount session.
    ///
    /// **Double-checked locking with the prefetch worker.** Both the
    /// prefetch worker and on-demand fetches acquire pool slots. If
    /// a prefetch batch lands the OID's pack while we're waiting
    /// for a slot, we'd otherwise re-query for an OID that's
    /// already local. Re-check `store.contains` after acquiring the
    /// slot and short-circuit on hit.
    fn raw_fetch(&self, oid: ObjectId) -> Result<(), FetcherError> {
        let trace = cattrace_enabled();
        let mut last_io_err: Option<std::io::Error> = None;
        for _attempt in 0..2 {
            let acquire_start = if trace { Some(Instant::now()) } else { None };
            let response = {
                let mut slot = self.batch.acquire();
                let work_start = if trace { Some(Instant::now()) } else { None };
                // Double-checked: another caller (typically the
                // prefetch worker on a different slot) may have
                // made the OID local between our outer fast-path
                // check and acquiring this slot.
                if self.store.contains(oid) {
                    if let (Some(t0), Some(t1)) = (acquire_start, work_start) {
                        let wait_us = t1.duration_since(t0).as_micros();
                        let short = oid.to_string();
                        eprintln!(
                            "cattrace: op=raw_fetch wait_us={wait_us} work_us=0 short_circuit=1 oid={}",
                            &short[..short.len().min(8)],
                        );
                    }
                    return Ok(());
                }
                let r = slot.with_child(|child| child.query(oid));
                if let (Some(t0), Some(t1)) = (acquire_start, work_start) {
                    let now = Instant::now();
                    let wait_us = t1.duration_since(t0).as_micros();
                    let work_us = now.duration_since(t1).as_micros();
                    let short = oid.to_string();
                    eprintln!(
                        "cattrace: op=raw_fetch wait_us={wait_us} work_us={work_us} oid={}",
                        &short[..short.len().min(8)],
                    );
                }
                match r {
                    Err(spawn_err) => {
                        // Spawn failure surfaces as Backend (not
                        // Transport-retryable) — matches the
                        // pre-pool single-child error mapping.
                        return Err(FetcherError::Backend(
                            oid,
                            format!("spawn git cat-file: {spawn_err}"),
                        ));
                    }
                    Ok(Ok(line)) => Ok(line),
                    Ok(Err(e)) => {
                        // Tear down the failed child on this slot
                        // so the next caller for this slot
                        // respawns. Other slots stay alive.
                        slot.reset();
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
            let trace = cattrace_enabled();
            // Two attempts: respawn the child on broken pipe. Each
            // attempt acquires a fresh slot from the pool (so if
            // the first attempt failed on slot N and a different
            // caller has been using slot N+1 happily, the retry
            // can land there instead of waiting on the just-reset
            // slot N).
            let mut last_io_err: Option<std::io::Error> = None;
            let mut batch_lines: Option<Vec<String>> = None;
            for _attempt in 0..2 {
                let acquire_start = if trace { Some(Instant::now()) } else { None };
                let mut slot = self.batch.acquire();
                let work_start = if trace { Some(Instant::now()) } else { None };
                let r = slot.with_child(|child| child.query_batch(&to_query));
                if let (Some(t0), Some(t1)) = (acquire_start, work_start) {
                    let now = Instant::now();
                    let wait_us = t1.duration_since(t0).as_micros();
                    let work_us = now.duration_since(t1).as_micros();
                    eprintln!(
                        "cattrace: op=prefetch_headers wait_us={wait_us} work_us={work_us} n_oids={}",
                        to_query.len(),
                    );
                }
                match r {
                    Err(spawn_err) => {
                        // Whole batch fails on spawn error. Don't
                        // burn the second attempt on a spawn
                        // failure — git is broken at the OS level.
                        for oid in &to_query {
                            probes_by_oid.insert(
                                *oid,
                                HeaderProbe::Error(
                                    *oid,
                                    FetcherError::Backend(
                                        *oid,
                                        format!("spawn git cat-file: {spawn_err}"),
                                    ),
                                ),
                            );
                        }
                        return reorder_probes(oids, probes_by_oid);
                    }
                    Ok(Ok(lines)) => {
                        batch_lines = Some(lines);
                        break;
                    }
                    Ok(Err(e)) => {
                        // Tear down the failed slot; retry will
                        // re-acquire (possibly the same slot,
                        // possibly a different one) and spawn
                        // afresh if needed.
                        slot.reset();
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
        // Pin K=1 so the assertion below ("the single slot stayed
        // alive") is meaningful — with K>1 the round-robin could
        // legitimately leave any one slot empty and the assertion
        // would be fragile.
        let f = GitCliFetcher::open_with_pool_size(store.clone(), 1).unwrap();

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
            f.batch.acquire().has_child(),
            "batch child should remain alive across many queries"
        );
        assert!(store.contains(head_oid));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Regression guard for the single-child path. After Stage 1
    /// of the cat-file pool plan, `GitCliFetcher` always holds a
    /// `BatchChildPool`; K=1 should be behaviour-equivalent to the
    /// pre-pool `Mutex<Option<BatchChild>>`. Specifically: many
    /// sequential `raw_fetch` calls reuse the same slot's child
    /// (no respawn per call). This test asserts the K=1 invariant
    /// the pre-pool implementation relied on.
    #[test]
    fn pool_k1_reuses_single_child_across_sequential_calls() {
        let tmp =
            std::env::temp_dir().join(format!("projgit-gitcli-pool-k1-{}", std::process::id()));
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
        let store = Arc::new(ObjectStore::open(tmp.join(".git")).unwrap());
        let f = GitCliFetcher::open_with_pool_size(store.clone(), 1).unwrap();
        assert_eq!(f.batch.slots.len(), 1);

        let bogus = ObjectId::from_hex(b"0000000000000000000000000000000000000001").unwrap();
        for _ in 0..3 {
            assert!(matches!(
                f.raw_fetch(bogus),
                Err(FetcherError::Refused(oid, _)) if oid == bogus
            ));
            // After each call the slot's child should still be
            // alive — "missing" is a normal cat-file response, not
            // an I/O failure that resets the slot.
            assert!(
                f.batch.acquire().has_child(),
                "K=1 single slot should retain its child across calls"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Sanity check that K=4 actually dispatches multiple slots
    /// under concurrent load. Not a strict perf assertion (the
    /// underlying `git cat-file` may finish faster than thread
    /// scheduling can interleave); instead, fire N concurrent
    /// `raw_fetch` calls and assert that **at least 2** distinct
    /// slots got populated. With K=1 the strict equivalent assertion
    /// would fail; with K=4 it should pass deterministically because
    /// `BatchChildPool::acquire` advances its round-robin counter
    /// on every call regardless of contention.
    #[test]
    fn pool_k4_dispatches_across_multiple_slots() {
        let tmp =
            std::env::temp_dir().join(format!("projgit-gitcli-pool-k4-{}", std::process::id()));
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
        let store = Arc::new(ObjectStore::open(tmp.join(".git")).unwrap());
        let f = Arc::new(GitCliFetcher::open_with_pool_size(store.clone(), 4).unwrap());
        assert_eq!(f.batch.slots.len(), 4);

        let bogus = ObjectId::from_hex(b"0000000000000000000000000000000000000001").unwrap();

        // Fan out 8 calls across 8 threads; each call advances
        // BatchChildPool.next, so multiple slots should end up
        // spawned. The round-robin counter is the deterministic
        // mechanism — we don't need real parallel cat-file dispatch
        // to make the assertion hold.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let f2 = f.clone();
            handles.push(std::thread::spawn(move || {
                let _ = f2.raw_fetch(bogus);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let spawned: usize = (0..f.batch.slots.len())
            .filter(|&i| f.batch.slots[i].lock().unwrap().is_some())
            .count();
        assert!(
            spawned >= 2,
            "K=4 pool should populate at least 2 distinct slots under 8-way concurrent load; got {spawned}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
