//! Small-blob LRU cache for [`crate::ObjectStore::read_blob`].
//!
//! Phase 5b polish: every `read` op on a hot file used to re-decode
//! the blob through gix even when the contents had been read seconds
//! before. The kernel page cache covers a lot of this on the FUSE
//! side, but only within the lifetime of a given inode; the same git
//! blob can resurface under a different inode when projections share
//! deduped content. A shared bounded LRU on the read side closes
//! that gap and is the obvious sibling to [`crate::tree_cache`].
//!
//! Shape decisions:
//!
//! - **Bounded by bytes, not by entry count.** Trees are uniformly
//!   tiny; blobs are not. We cap the *total* cached bytes (default
//!   16 MiB) and skip any single blob over a per-entry threshold
//!   (default 64 KiB) so a `cat` of a multi-megabyte file doesn't
//!   evict everything else.
//! - **`Arc<Vec<u8>>` payloads.** `read_blob` already returns
//!   `Vec<u8>`, so a cache hit pays only the cost of cloning the
//!   `Vec` contents. The `Arc` keeps the in-cache copy cheap to
//!   share across threads but is opaque to callers — they always
//!   get a fresh `Vec<u8>` matching the existing API.
//! - **One `Mutex` for everything.** Mirrors `tree_cache`; can be
//!   revisited if profiling shows contention.
//! - **OID-keyed.** Blobs are immutable in their OID-keyed sense, so
//!   the cache never has to invalidate.

use gix::ObjectId;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// Default total-bytes capacity. Generous-but-bounded; ~16 MiB is a
/// few hundred typical source files comfortably.
pub(crate) const DEFAULT_CAPACITY_BYTES: usize = 16 * 1024 * 1024;

/// Default per-entry size cap. Anything above this (large binary
/// assets, generated files) is served fresh through gix and skipped
/// at insert time so it doesn't dominate the cache.
pub(crate) const DEFAULT_PER_ENTRY_MAX_BYTES: usize = 64 * 1024;

/// One cached blob.
struct Entry {
    payload: Arc<Vec<u8>>,
    /// Access generation; bumped on every hit so the entry moves to
    /// the most-recently-used end of [`Inner::order`].
    generation: u64,
}

struct Inner {
    capacity_bytes: usize,
    per_entry_max_bytes: usize,
    bytes_used: usize,
    next_gen: u64,
    entries: HashMap<ObjectId, Entry>,
    /// Reverse index: `generation -> oid`. The smallest key is the
    /// LRU entry; eviction is O(log n).
    order: BTreeMap<u64, ObjectId>,

    // -- stats; co-located so they're consistent under the same lock
    //    as the cache state itself.
    hits: u64,
    misses: u64,
    inserts: u64,
    evictions: u64,
    /// `put` calls that were skipped because the candidate exceeded
    /// `per_entry_max_bytes`. Useful for tuning that threshold.
    skipped_too_large: u64,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobCacheInner")
            .field("capacity_bytes", &self.capacity_bytes)
            .field("per_entry_max_bytes", &self.per_entry_max_bytes)
            .field("bytes_used", &self.bytes_used)
            .field("len", &self.entries.len())
            .field("hits", &self.hits)
            .field("misses", &self.misses)
            .field("inserts", &self.inserts)
            .field("evictions", &self.evictions)
            .field("skipped_too_large", &self.skipped_too_large)
            .finish()
    }
}

/// LRU cache for blob bytes.
///
/// Internally synchronised; `&Self` is sufficient on the hot path.
#[derive(Debug)]
pub(crate) struct BlobCache {
    inner: Mutex<Inner>,
}

/// Snapshot of cache counters; useful for tests, the `--stats` CLI
/// flag, and future metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlobCacheStats {
    /// Number of `get` calls that returned a cached entry.
    pub hits: u64,
    /// Number of `get` calls that missed.
    pub misses: u64,
    /// Number of entries inserted via `put`.
    pub inserts: u64,
    /// Number of entries evicted to honour the byte budget.
    pub evictions: u64,
    /// Number of `put` calls dropped because the payload exceeded
    /// `per_entry_max_bytes`.
    pub skipped_too_large: u64,
    /// Current number of cached entries.
    pub len: usize,
    /// Current cached bytes.
    pub bytes_used: usize,
    /// Total-bytes capacity (`0` => caching disabled).
    pub capacity_bytes: usize,
    /// Per-entry size cap.
    pub per_entry_max_bytes: usize,
}

impl BlobCache {
    /// Construct an empty cache. A `capacity_bytes` of zero disables
    /// caching: `get` always misses, `put` is a no-op (modulo stats).
    pub(crate) fn new(capacity_bytes: usize, per_entry_max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                capacity_bytes,
                per_entry_max_bytes,
                bytes_used: 0,
                next_gen: 0,
                entries: HashMap::new(),
                order: BTreeMap::new(),
                hits: 0,
                misses: 0,
                inserts: 0,
                evictions: 0,
                skipped_too_large: 0,
            }),
        }
    }

    /// Look up a cached blob by OID. Bumps the entry's recency on a
    /// hit so it doesn't get evicted next.
    pub(crate) fn get(&self, oid: &ObjectId) -> Option<Arc<Vec<u8>>> {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let old_gen = g.entries.get(oid).map(|e| e.generation)?;
        g.order.remove(&old_gen);
        g.next_gen += 1;
        let new_gen = g.next_gen;
        g.order.insert(new_gen, *oid);
        let entry = g.entries.get_mut(oid).expect("just looked up");
        entry.generation = new_gen;
        let payload = Arc::clone(&entry.payload);
        g.hits += 1;
        Some(payload)
    }

    /// Insert a blob under `oid`. Skips if the payload exceeds
    /// `per_entry_max_bytes`; otherwise evicts least-recently-used
    /// entries until the new payload fits inside `capacity_bytes`.
    pub(crate) fn put(&self, oid: ObjectId, payload: Arc<Vec<u8>>) {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        if g.capacity_bytes == 0 {
            return;
        }
        let payload_len = payload.len();
        if payload_len > g.per_entry_max_bytes {
            g.skipped_too_large += 1;
            return;
        }

        // If already present, replace + promote.
        if g.entries.contains_key(&oid) {
            let old_gen = g.entries[&oid].generation;
            let old_len = g.entries[&oid].payload.len();
            g.order.remove(&old_gen);
            g.bytes_used -= old_len;
            g.next_gen += 1;
            let new_gen = g.next_gen;
            g.order.insert(new_gen, oid);
            let existing = g.entries.get_mut(&oid).expect("just checked");
            existing.generation = new_gen;
            existing.payload = payload;
            g.bytes_used += payload_len;
            g.inserts += 1;
            return;
        }

        // Evict LRU entries until the new payload fits.
        while g.bytes_used + payload_len > g.capacity_bytes {
            let Some((&lru_gen, &lru_oid)) = g.order.iter().next() else {
                // Cache empty but the payload still doesn't fit;
                // capacity must be smaller than the entry. We
                // already gated on per_entry_max_bytes <=
                // capacity_bytes elsewhere (the public constructor on
                // ObjectStore enforces it), so this is unreachable in
                // practice. Bail safely.
                g.skipped_too_large += 1;
                return;
            };
            g.order.remove(&lru_gen);
            if let Some(removed) = g.entries.remove(&lru_oid) {
                g.bytes_used -= removed.payload.len();
                g.evictions += 1;
            }
        }

        g.next_gen += 1;
        let new_gen = g.next_gen;
        g.order.insert(new_gen, oid);
        g.entries.insert(
            oid,
            Entry {
                payload,
                generation: new_gen,
            },
        );
        g.bytes_used += payload_len;
        g.inserts += 1;
    }

    /// Tally a miss against the stats. Separated from `get` so
    /// callers may decide to record a miss conditionally (e.g. only
    /// after a successful upstream parse).
    pub(crate) fn record_miss(&self) {
        self.inner.lock().unwrap().misses += 1;
    }

    /// Snapshot the cache counters.
    pub fn stats(&self) -> BlobCacheStats {
        let g = self.inner.lock().unwrap();
        BlobCacheStats {
            hits: g.hits,
            misses: g.misses,
            inserts: g.inserts,
            evictions: g.evictions,
            skipped_too_large: g.skipped_too_large,
            len: g.entries.len(),
            bytes_used: g.bytes_used,
            capacity_bytes: g.capacity_bytes,
            per_entry_max_bytes: g.per_entry_max_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid_n(n: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        ObjectId::from_bytes_or_panic(&bytes)
    }

    fn payload(bytes: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![0u8; bytes])
    }

    #[test]
    fn miss_then_hit_promotes_recency() {
        let cache = BlobCache::new(/* cap */ 64, /* per */ 64);
        assert!(cache.get(&oid_n(1)).is_none());
        cache.record_miss();
        cache.put(oid_n(1), payload(16));
        cache.put(oid_n(2), payload(16));
        cache.put(oid_n(3), payload(16));
        cache.put(oid_n(4), payload(16));
        // Cache is now exactly full (4 * 16 = 64).

        // Touching `1` makes `2` the LRU.
        let _ = cache.get(&oid_n(1)).unwrap();

        // Insert `5` — should evict `2`.
        cache.put(oid_n(5), payload(16));

        let s = cache.stats();
        assert_eq!(s.bytes_used, 64);
        assert_eq!(s.evictions, 1);
        assert!(cache.get(&oid_n(2)).is_none());
        assert!(cache.get(&oid_n(1)).is_some());
        assert!(cache.get(&oid_n(5)).is_some());
    }

    #[test]
    fn over_per_entry_cap_is_skipped() {
        let cache = BlobCache::new(/* cap */ 1024, /* per */ 16);
        cache.put(oid_n(1), payload(32)); // > 16, skipped
        let s = cache.stats();
        assert_eq!(s.skipped_too_large, 1);
        assert_eq!(s.len, 0);
        assert_eq!(s.bytes_used, 0);
        assert!(cache.get(&oid_n(1)).is_none());
    }

    #[test]
    fn put_existing_replaces_and_promotes() {
        let cache = BlobCache::new(/* cap */ 64, /* per */ 64);
        cache.put(oid_n(1), payload(16));
        cache.put(oid_n(2), payload(16));
        // Replace `1` with a same-size payload; bytes_used should
        // stay correct and `1` should be promoted to MRU.
        cache.put(oid_n(1), payload(16));
        cache.put(oid_n(3), payload(16));
        cache.put(oid_n(4), payload(16));
        // Capacity 64 = exactly four entries. One more eviction
        // beyond `2` (the only LRU) should drop only `2`, not `1`.
        cache.put(oid_n(5), payload(16));
        assert!(cache.get(&oid_n(2)).is_none(), "2 evicted");
        assert!(cache.get(&oid_n(1)).is_some(), "1 promoted, retained");
    }

    #[test]
    fn capacity_zero_disables_caching() {
        let cache = BlobCache::new(0, 16);
        cache.put(oid_n(1), payload(8));
        assert!(cache.get(&oid_n(1)).is_none());
        let s = cache.stats();
        assert_eq!(s.len, 0);
        assert_eq!(s.bytes_used, 0);
        assert_eq!(s.inserts, 0);
    }
}
