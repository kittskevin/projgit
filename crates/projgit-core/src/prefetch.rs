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
use gix::ObjectId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
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
}

#[derive(Default)]
struct AtomicStats {
    posted: AtomicU64,
    dropped: AtomicU64,
    batches_sent: AtomicU64,
    oids_resolved: AtomicU64,
    headers_published: AtomicU64,
    oids_failed: AtomicU64,
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
        }
    }
    // Sender dropped; exit cleanly.
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
}
