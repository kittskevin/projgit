//! Per-blob header LRU cache for [`crate::ObjectStore::header`].
//!
//! Mirrors [`crate::tree_cache`]: bounded LRU keyed by `ObjectId`,
//! HashMap + BTreeMap reverse index for O(log n) eviction, single
//! `Mutex` over both. Same shape; different payload.
//!
//! ## What this caches
//!
//! `(ObjectKind, u64)` per OID — the same tuple `header()` returns.
//! Header data is tiny (a few bytes per entry), so the cache can be
//! generously sized without worrying about memory. A capacity of
//! a few thousand entries comfortably covers any single directory
//! walk's worth of `lookup` calls.
//!
//! ## Why a separate cache from blobs and trees
//!
//! `header()` is what `lookup` calls to get a file's size at `stat`
//! time. The previous on-demand path was: kernel `lookup` →
//! `HydratingObjectStore::header` → `ObjectStore::header` →
//! `gix::try_find_header` (one promisor-fetch round trip on miss).
//!
//! Phase 5b's `read_blob` has its own LRU on full bytes, but
//! `header` was uncached: a directory of N files would pay N
//! `try_find_header` calls + N upstream RTTs. The header cache, in
//! combination with the T1 prefetch worker, lets us pay one
//! batched RTT for a whole directory's headers up front and serve
//! the per-entry `lookup` calls from cache.

use crate::object_store::ObjectKind;
use gix::ObjectId;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// Default cache capacity. A few thousand headers is comfortably
/// larger than the working set of any single directory walk we
/// realistically expect to see.
pub(crate) const DEFAULT_CAPACITY: usize = 4096;

/// One cached header.
#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: ObjectKind,
    size: u64,
    /// Access generation; bumped on every hit so the entry moves
    /// to the most-recently-used end of [`Inner::order`].
    generation: u64,
}

struct Inner {
    capacity: usize,
    next_gen: u64,
    entries: HashMap<ObjectId, Entry>,
    /// Reverse index: `generation -> oid`. Smallest key = LRU.
    order: BTreeMap<u64, ObjectId>,

    // -- stats; co-located so they're consistent under the same
    //    lock as the cache state itself.
    hits: u64,
    misses: u64,
    inserts: u64,
    evictions: u64,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeaderCacheInner")
            .field("capacity", &self.capacity)
            .field("len", &self.entries.len())
            .field("hits", &self.hits)
            .field("misses", &self.misses)
            .field("inserts", &self.inserts)
            .field("evictions", &self.evictions)
            .finish()
    }
}

/// LRU cache for blob/tree/commit headers.
#[derive(Debug)]
pub(crate) struct HeaderCache {
    inner: Mutex<Inner>,
}

/// Snapshot of cache counters; useful for tests, the `--stats`
/// CLI flag, and future metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderCacheStats {
    /// Number of `get` calls that returned a cached entry.
    pub hits: u64,
    /// Number of `get` calls that missed.
    pub misses: u64,
    /// Number of entries inserted via `put`.
    pub inserts: u64,
    /// Number of entries evicted to honour the capacity bound.
    pub evictions: u64,
    /// Current number of cached entries.
    pub len: usize,
    /// Cache capacity.
    pub capacity: usize,
}

impl HeaderCache {
    /// Construct an empty cache. `capacity` of zero disables
    /// caching: `get` always misses, `put` is a no-op (modulo
    /// stats).
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                capacity,
                next_gen: 0,
                entries: HashMap::with_capacity(capacity.min(64)),
                order: BTreeMap::new(),
                hits: 0,
                misses: 0,
                inserts: 0,
                evictions: 0,
            }),
        }
    }

    /// Look up a cached header by OID.
    pub(crate) fn get(&self, oid: &ObjectId) -> Option<(ObjectKind, u64)> {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let old_gen = g.entries.get(oid).map(|e| e.generation)?;
        g.order.remove(&old_gen);
        g.next_gen += 1;
        let new_gen = g.next_gen;
        g.order.insert(new_gen, *oid);
        let entry = g.entries.get_mut(oid).expect("just looked up");
        entry.generation = new_gen;
        let payload = (entry.kind, entry.size);
        g.hits += 1;
        Some(payload)
    }

    /// Insert a header under `oid`. Evicts LRU on capacity.
    pub(crate) fn put(&self, oid: ObjectId, kind: ObjectKind, size: u64) {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        if g.capacity == 0 {
            return;
        }

        if g.entries.contains_key(&oid) {
            let old_gen = g.entries[&oid].generation;
            g.order.remove(&old_gen);
            g.next_gen += 1;
            let new_gen = g.next_gen;
            g.order.insert(new_gen, oid);
            let existing = g.entries.get_mut(&oid).expect("just checked");
            existing.generation = new_gen;
            existing.kind = kind;
            existing.size = size;
            g.inserts += 1;
            return;
        }

        while g.entries.len() >= g.capacity {
            let Some((&lru_gen, &lru_oid)) = g.order.iter().next() else {
                break;
            };
            g.order.remove(&lru_gen);
            g.entries.remove(&lru_oid);
            g.evictions += 1;
        }

        g.next_gen += 1;
        let new_gen = g.next_gen;
        g.order.insert(new_gen, oid);
        g.entries.insert(
            oid,
            Entry {
                kind,
                size,
                generation: new_gen,
            },
        );
        g.inserts += 1;
    }

    /// Tally a miss against the stats. Separated from `get` so
    /// callers may decide to record a miss conditionally.
    pub(crate) fn record_miss(&self) {
        self.inner.lock().unwrap().misses += 1;
    }

    /// Snapshot the cache counters.
    pub fn stats(&self) -> HeaderCacheStats {
        let g = self.inner.lock().unwrap();
        HeaderCacheStats {
            hits: g.hits,
            misses: g.misses,
            inserts: g.inserts,
            evictions: g.evictions,
            len: g.entries.len(),
            capacity: g.capacity,
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

    #[test]
    fn miss_then_hit_then_evict() {
        let cache = HeaderCache::with_capacity(2);

        assert!(cache.get(&oid_n(1)).is_none());
        cache.record_miss();
        cache.put(oid_n(1), ObjectKind::Blob, 11);
        assert!(cache.get(&oid_n(2)).is_none());
        cache.record_miss();
        cache.put(oid_n(2), ObjectKind::Blob, 22);

        // Touch 1 so 2 is LRU, then inserting 3 evicts 2.
        let got = cache.get(&oid_n(1)).unwrap();
        assert_eq!(got, (ObjectKind::Blob, 11));
        cache.put(oid_n(3), ObjectKind::Blob, 33);

        let s = cache.stats();
        assert_eq!(s.capacity, 2);
        assert_eq!(s.len, 2);
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 2);
        assert_eq!(s.inserts, 3);
        assert_eq!(s.evictions, 1);

        assert!(cache.get(&oid_n(2)).is_none());
        assert_eq!(cache.get(&oid_n(1)), Some((ObjectKind::Blob, 11)));
        assert_eq!(cache.get(&oid_n(3)), Some((ObjectKind::Blob, 33)));
    }

    #[test]
    fn put_existing_promotes_and_replaces() {
        let cache = HeaderCache::with_capacity(2);
        cache.put(oid_n(1), ObjectKind::Blob, 1);
        cache.put(oid_n(2), ObjectKind::Blob, 2);

        // Update payload and promote 1 to MRU.
        cache.put(oid_n(1), ObjectKind::Blob, 99);
        cache.put(oid_n(3), ObjectKind::Blob, 3);

        assert!(cache.get(&oid_n(2)).is_none(), "2 should be evicted");
        assert_eq!(cache.get(&oid_n(1)), Some((ObjectKind::Blob, 99)));
        assert_eq!(cache.get(&oid_n(3)), Some((ObjectKind::Blob, 3)));
    }

    #[test]
    fn capacity_zero_disables_caching() {
        let cache = HeaderCache::with_capacity(0);
        cache.put(oid_n(1), ObjectKind::Blob, 1);
        assert!(cache.get(&oid_n(1)).is_none());
        let s = cache.stats();
        assert_eq!(s.len, 0);
        assert_eq!(s.capacity, 0);
        assert_eq!(s.inserts, 0);
    }
}
