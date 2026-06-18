//! Background prefetch worker for the T1 readdir-time header
//! batching tier (see `docs/design/prefetch.md` §4).
//!
//! Owned by `ProjectionFsProvider`. One worker thread per provider;
//! one bounded mpsc channel feeding it. `readdir` is the producer:
//! after it returns directory entries to the FUSE adapter, it posts
//! a `PrefetchTask::Headers(...)` with all the entry blob OIDs. The
//! worker drains the channel, batches up to `MAX_BATCH` OIDs per
//! query, and calls
//! [`crate::HydratingObjectStore::prefetch_headers`] which:
//!
//! 1. Skips OIDs already present locally.
//! 2. Sends the rest in one `git cat-file --batch-check` round
//!    trip via [`crate::GitCliFetcher::prefetch_headers`].
//! 3. Publishes the resulting `(kind, size)` tuples to the
//!    underlying [`crate::ObjectStore`]'s header cache.
//!
//! By the time the kernel walks back through the directory and
//! issues per-entry `lookup`s, the headers are warm.
//!
//! ## Bounded resource use
//!
//! - Channel capacity: `CHANNEL_CAPACITY` (256 outstanding tasks).
//!   `readdir` uses `try_send`; on full, the post is dropped
//!   silently and the on-demand path will fetch correctly when
//!   `lookup` runs.
//! - One worker thread per provider, joined on `Drop`.
//! - The worker holds no state across iterations; it's safe to
//!   drop the provider mid-batch.
//!
//! ## Cancellation
//!
//! `Drop` of `PrefetchHandle` closes the channel, which wakes the
//! worker out of its `recv()` and lets it exit cleanly. We then
//! join the thread.

use crate::fetcher::{Fetcher, HeaderProbe, HydratingObjectStore};
use crate::object_store::ObjectKind;
use gix::ObjectId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

/// Maximum number of outstanding tasks the channel will hold.
/// Producers (`readdir`) `try_send`; on full, drop silently.
const CHANNEL_CAPACITY: usize = 256;

/// Maximum OIDs per upstream batch query. Sized so a typical
/// directory walk lands in one round trip but pathologically
/// huge directories don't push git's stdin buffer.
const MAX_BATCH: usize = 64;

/// Tasks the worker accepts. Currently only T1 headers; T2/T3
/// will add more variants here without breaking T1.
pub(crate) enum PrefetchTask {
    Headers(Vec<ObjectId>),
    /// Bulk-warm blob *bytes* for a directory's files (Architecture
    /// B). Gated by `PROJGIT_PREFETCH_BLOBS`; the worker applies a
    /// size cap so only small files are speculatively hydrated.
    Blobs(Vec<ObjectId>),
}

/// Counters surfaced via [`PrefetchHandle::stats`] and ultimately
/// the CLI's `--stats` output. All cumulative; no reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefetchStats {
    /// Tasks the producer successfully posted.
    pub posted: u64,
    /// Tasks dropped because the channel was full.
    pub dropped: u64,
    /// Batches the worker actually shipped upstream (or short-
    /// circuited entirely on cache hit).
    pub batches_sent: u64,
    /// OIDs the worker resolved (regardless of probe outcome).
    pub oids_resolved: u64,
    /// OIDs that resolved with `PresentWithHeader` -- the cheap
    /// path where we got `(kind, size)` directly from the upstream
    /// query.
    pub headers_published: u64,
    /// OIDs that ended up in `HeaderProbe::Error`.
    pub oids_failed: u64,
    /// Blob OIDs whose bytes were warmed via bulk fetch (Architecture
    /// B blob prefetch).
    pub blobs_warmed: u64,
    /// Blob OIDs skipped by the prefetch size cap (too large, or size
    /// unknown, to speculatively hydrate).
    pub blobs_skipped: u64,
}

#[derive(Default)]
struct AtomicStats {
    posted: AtomicU64,
    dropped: AtomicU64,
    batches_sent: AtomicU64,
    oids_resolved: AtomicU64,
    headers_published: AtomicU64,
    oids_failed: AtomicU64,
    blobs_warmed: AtomicU64,
    blobs_skipped: AtomicU64,
}

impl AtomicStats {
    fn snapshot(&self) -> PrefetchStats {
        PrefetchStats {
            posted: self.posted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            batches_sent: self.batches_sent.load(Ordering::Relaxed),
            oids_resolved: self.oids_resolved.load(Ordering::Relaxed),
            headers_published: self.headers_published.load(Ordering::Relaxed),
            oids_failed: self.oids_failed.load(Ordering::Relaxed),
            blobs_warmed: self.blobs_warmed.load(Ordering::Relaxed),
            blobs_skipped: self.blobs_skipped.load(Ordering::Relaxed),
        }
    }
}

/// The producer-side handle owned by `ProjectionFsProvider`.
///
/// Holds the channel sender + the worker's join handle. Drop
/// gracefully shuts down by dropping the sender (which makes the
/// worker's `recv()` return `Err`), then joining.
pub(crate) struct PrefetchHandle {
    tx: Option<SyncSender<PrefetchTask>>,
    worker: Option<JoinHandle<()>>,
    stats: Arc<AtomicStats>,
}

impl PrefetchHandle {
    /// Spawn a worker bound to the given hydrating store.
    pub(crate) fn spawn<F>(store: Arc<HydratingObjectStore<F>>) -> Self
    where
        F: Fetcher + 'static,
    {
        let (tx, rx) = sync_channel::<PrefetchTask>(CHANNEL_CAPACITY);
        let stats = Arc::new(AtomicStats::default());
        let worker_stats = stats.clone();

        let worker = std::thread::Builder::new()
            .name("projgit-prefetch".to_owned())
            .spawn(move || worker_loop(rx, store, worker_stats))
            .expect("spawn projgit-prefetch worker");

        Self {
            tx: Some(tx),
            worker: Some(worker),
            stats,
        }
    }

    /// Post a batch of OIDs for header prefetching. Non-blocking:
    /// drops the post if the channel is full. Safe to call from
    /// hot paths.
    pub(crate) fn post_headers(&self, oids: Vec<ObjectId>) {
        if oids.is_empty() {
            return;
        }
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        match tx.try_send(PrefetchTask::Headers(oids)) {
            Ok(()) => {
                self.stats.posted.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Post a batch of blob OIDs for byte-warming (Architecture B).
    /// Non-blocking; drops on a full channel. Only meaningful when
    /// [`blob_prefetch_enabled`]; callers gate before posting. Post
    /// **after** the matching `post_headers` so the worker's size cap
    /// sees warm sizes (the worker is single-threaded + FIFO).
    pub(crate) fn post_blobs(&self, oids: Vec<ObjectId>) {
        if oids.is_empty() {
            return;
        }
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        match tx.try_send(PrefetchTask::Blobs(oids)) {
            Ok(()) => {
                self.stats.posted.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Snapshot the prefetch counters.
    pub fn stats(&self) -> PrefetchStats {
        self.stats.snapshot()
    }
}

impl Drop for PrefetchHandle {
    fn drop(&mut self) {
        // Close the sender side; worker's recv() will return Err
        // and the loop will exit.
        self.tx = None;
        if let Some(worker) = self.worker.take() {
            // Best-effort; if the worker panicked we don't want to
            // double-panic in Drop.
            let _ = worker.join();
        }
    }
}

fn worker_loop<F: Fetcher + 'static>(
    rx: std::sync::mpsc::Receiver<PrefetchTask>,
    store: Arc<HydratingObjectStore<F>>,
    stats: Arc<AtomicStats>,
) {
    while let Ok(task) = rx.recv() {
        match task {
            PrefetchTask::Headers(oids) => {
                // Chunk into MAX_BATCH-sized slices so an oversized
                // post doesn't force a giant single query.
                for chunk in oids.chunks(MAX_BATCH) {
                    let probes = store.prefetch_headers(chunk);
                    stats.batches_sent.fetch_add(1, Ordering::Relaxed);
                    stats
                        .oids_resolved
                        .fetch_add(probes.len() as u64, Ordering::Relaxed);
                    for probe in &probes {
                        match probe {
                            HeaderProbe::PresentWithHeader(_, _, _)
                            | HeaderProbe::HeaderOnly(_, _, _) => {
                                stats.headers_published.fetch_add(1, Ordering::Relaxed);
                            }
                            HeaderProbe::Present(_) => { /* cache populated via store.header */ }
                            HeaderProbe::Error(_, _) => {
                                stats.oids_failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
            PrefetchTask::Blobs(oids) => {
                let cap = blob_size_cap_bytes();
                for chunk in oids.chunks(MAX_BATCH) {
                    let (keep, skipped) = blobs_under_cap(&store, chunk, cap);
                    stats.blobs_skipped.fetch_add(skipped, Ordering::Relaxed);
                    if keep.is_empty() {
                        continue;
                    }
                    let probes = store.fetch_objects(&keep);
                    stats.batches_sent.fetch_add(1, Ordering::Relaxed);
                    let warmed = probes
                        .iter()
                        .filter(|p| !matches!(p, HeaderProbe::Error(_, _)))
                        .count();
                    stats.blobs_warmed.fetch_add(warmed as u64, Ordering::Relaxed);
                }
            }
        }
    }
    // Sender dropped; exit cleanly.
}

/// Blob OIDs whose known size is within `cap`. Consults the (warm)
/// header cache via `ObjectStore::header`; the FIFO worker processes
/// a directory's `Headers` task before its `Blobs` task, so sizes are
/// cached by the time this runs. Blobs with unknown size or over the
/// cap are skipped (counted in the returned total) and left to the
/// on-demand path.
fn blobs_under_cap<F: Fetcher>(
    store: &HydratingObjectStore<F>,
    oids: &[ObjectId],
    cap: u64,
) -> (Vec<ObjectId>, u64) {
    let mut keep = Vec::with_capacity(oids.len());
    let mut skipped = 0u64;
    for &oid in oids {
        match store.store().header(oid) {
            Ok((ObjectKind::Blob, size)) if size <= cap => keep.push(oid),
            _ => skipped += 1,
        }
    }
    (keep, skipped)
}

/// Whether Architecture-B blob prefetch is enabled. Off by default;
/// set `PROJGIT_PREFETCH_BLOBS=1` to warm a directory's small-file
/// blob bytes on `readdir`. Read once per process.
pub(crate) fn blob_prefetch_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("PROJGIT_PREFETCH_BLOBS")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

/// Size cap (bytes) for blob prefetch; blobs larger than this are not
/// speculatively hydrated. Default 1 MiB; override with
/// `PROJGIT_PREFETCH_BLOB_CAP_BYTES`. Read once per process.
fn blob_size_cap_bytes() -> u64 {
    static CAP: OnceLock<u64> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("PROJGIT_PREFETCH_BLOB_CAP_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024 * 1024)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetcher::NoopFetcher;
    use crate::ObjectStore;

    #[test]
    fn handle_post_empty_is_noop() {
        // We can't easily build a real HydratingObjectStore in a
        // unit test without a fixture repo, so use a minimal one
        // (NoopFetcher requires no network). The store still needs
        // a real .git directory.
        let tmp =
            std::env::temp_dir().join(format!("projgit-prefetch-handle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        if std::process::Command::new("git")
            .args(["init", "-q", "-b", "main", tmp.to_str().unwrap()])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("SKIP: git CLI not available");
            return;
        }

        let store = Arc::new(ObjectStore::open(tmp.join(".git")).unwrap());
        let h = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
        let handle = PrefetchHandle::spawn(h);

        handle.post_headers(Vec::new());
        let s = handle.stats();
        assert_eq!(s.posted, 0);
        assert_eq!(s.dropped, 0);

        drop(handle);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn blobs_under_cap_filters_large() {
        let tmp =
            std::env::temp_dir().join(format!("projgit-prefetch-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        if std::process::Command::new("git")
            .args(["init", "-q", "-b", "main", tmp.to_str().unwrap()])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("SKIP: git CLI not available");
            return;
        }
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&tmp)
                .args(args)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed");
        };
        git(&["config", "user.email", "t@e.invalid"]);
        git(&["config", "user.name", "T"]);
        std::fs::write(tmp.join("small.txt"), b"hi\n").unwrap();
        std::fs::write(tmp.join("big.bin"), vec![0u8; 100_000]).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "x"]);
        let head_hex = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&tmp)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let head = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).unwrap();

        let store = Arc::new(ObjectStore::open(tmp.join(".git")).unwrap());
        let root = store.commit_tree(head).unwrap();
        let entries = store.read_tree(root).unwrap();
        let oid_of = |n: &str| {
            entries
                .iter()
                .find(|e| String::from_utf8_lossy(&e.name) == n)
                .unwrap()
                .oid
        };
        let small = oid_of("small.txt");
        let big = oid_of("big.bin");

        let hydrating = HydratingObjectStore::new(store, NoopFetcher);
        let (keep, skipped) = blobs_under_cap(&hydrating, &[small, big], 50_000);
        assert_eq!(keep, vec![small], "only the small blob is within the cap");
        assert_eq!(skipped, 1, "the 100KB blob is skipped");

        drop(hydrating);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
