//! Tiny LRU cache for parsed trees.
//!
//! Phase 5 polish: every `readdir` call previously paid the cost of
//! parsing the same tree object again. This cache memoises parsed
//! `Vec<RawTreeEntry>`s by tree OID so warm `ls` calls become a hash
//! lookup + an `Arc::clone` instead of a `gix::Repository::find` +
//! `tree.iter()` walk.
//!
//! ## Why a hand-rolled LRU and not a crate
//!
//! Stdlib alone gets us a correct true-LRU in <100 lines, with no
//! extra build deps to drag through projgit-fuse / projgit-winfsp.
//! We use:
//!
//! - a `HashMap<ObjectId, Entry>` keyed by tree OID,
//! - a `BTreeMap<u64, ObjectId>` ordered by access generation so we
//!   can evict the least-recently-used entry in O(log n).
//!
//! All access goes through a single `Mutex` for simplicity; callers
//! that hammer this from many threads can revisit the locking in a
//! later phase.
//!
//! ## What this caches
//!
//! Only **parsed tree entry lists**, not blob bytes (Phase 5 may add
//! a small-blob cache later) and not raw object bytes. Tree objects
//! are tiny (≪1 KB each, even for huge directories) and immutable in
//! the OID-keyed sense, so the cache never has to invalidate.

use crate::object_store::RawTreeEntry;
use gix::ObjectId;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// Default cache capacity. 256 trees ≈ a few hundred KB at the
/// outside; comfortably covers any single directory walk we've
/// surfaced and most "browse around the tree" workloads.
pub(crate) const DEFAULT_CAPACITY: usize = 256;

/// One cached parsed tree.
struct Entry {
    payload: Arc<Vec<RawTreeEntry>>,
    /// Access generation; bumped on every hit so the entry moves to
    /// the "most recently used" end of [`Inner::order`].
    generation: u64,
}

struct Inner {
    capacity: usize,
    next_gen: u64,
    entries: HashMap<ObjectId, Entry>,
    /// Reverse index: `generation -> oid`. The smallest key is the
    /// LRU entry; pulling it is O(log n).
    order: BTreeMap<u64, ObjectId>,

    // -- stats; kept inline so they're naturally consistent under the
    //    same lock the cache state lives under.
    hits: u64,
    misses: u64,
    inserts: u64,
    evictions: u64,
}

/// LRU cache for parsed tree entries.
///
/// Internally synchronised; `&Self` is sufficient on the hot path.
#[derive(Debug)]
pub(crate) struct TreeCache {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeCacheInner")
            .field("capacity", &self.capacity)
            .field("len", &self.entries.len())
            .field("hits", &self.hits)
            .field("misses", &self.misses)
            .field("inserts", &self.inserts)
            .field("evictions", &self.evictions)
            .finish()
    }
}

/// Snapshot of cache counters; useful for tests and future metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeCacheStats {
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

impl TreeCache {
    /// Construct an empty cache with the given capacity. A capacity
    /// of zero disables caching: `get` always misses, `put` is a
    /// no-op (modulo stats).
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

    /// Look up a parsed tree by OID. Bumps the entry's recency on a
    /// hit so it doesn't get evicted next.
    pub(crate) fn get(&self, oid: &ObjectId) -> Option<Arc<Vec<RawTreeEntry>>> {
        let mut guard = self.inner.lock().unwrap();
        // Split-borrow trick: dereference once so the compiler sees
        // disjoint fields rather than separate `Deref` calls through
        // the `MutexGuard`.
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

    /// Insert a parsed tree under `oid`. If the cache is at capacity,
    /// evicts the least-recently-used entry.
    ///
    /// If `oid` is already cached the existing entry is replaced and
    /// promoted to most-recently-used; this matters mostly for
    /// pathological races where two threads parse the same tree.
    pub(crate) fn put(&self, oid: ObjectId, payload: Arc<Vec<RawTreeEntry>>) {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        if g.capacity == 0 {
            return;
        }

        if g.entries.contains_key(&oid) {
            // Update + promote in one go.
            let old_gen = g.entries[&oid].generation;
            g.order.remove(&old_gen);
            g.next_gen += 1;
            let new_gen = g.next_gen;
            g.order.insert(new_gen, oid);
            g.inserts += 1;
            let existing = g.entries.get_mut(&oid).expect("just checked");
            existing.generation = new_gen;
            existing.payload = payload;
            return;
        }

        // Evict if full.
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
                payload,
                generation: new_gen,
            },
        );
        g.inserts += 1;
    }

    /// Tally a miss against the stats. We separate this from `get`
    /// because callers may want to record a miss conditionally
    /// (e.g. only after a successful upstream parse).
    pub(crate) fn record_miss(&self) {
        self.inner.lock().unwrap().misses += 1;
    }

    /// Snapshot the cache counters.
    pub fn stats(&self) -> TreeCacheStats {
        let g = self.inner.lock().unwrap();
        TreeCacheStats {
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
    use bstr::BString;

    fn oid_n(n: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        ObjectId::from_bytes_or_panic(&bytes)
    }

    fn payload(name: &str) -> Arc<Vec<RawTreeEntry>> {
        Arc::new(vec![RawTreeEntry {
            name: BString::from(name),
            mode_raw: 0o100644,
            oid: oid_n(0xff),
        }])
    }

    #[test]
    fn miss_then_hit_then_evict() {
        let cache = TreeCache::with_capacity(2);

        // Cold misses.
        assert!(cache.get(&oid_n(1)).is_none());
        cache.record_miss();
        cache.put(oid_n(1), payload("a"));
        assert!(cache.get(&oid_n(2)).is_none());
        cache.record_miss();
        cache.put(oid_n(2), payload("b"));

        // Two hits.
        let a = cache.get(&oid_n(1)).expect("hit");
        assert_eq!(a[0].name, BString::from("a"));
        let _ = cache.get(&oid_n(2)).expect("hit");

        // A third tree should evict the LRU. Touching `1` first to
        // force `2` to be the LRU.
        let _ = cache.get(&oid_n(1));
        cache.put(oid_n(3), payload("c"));

        let s = cache.stats();
        assert_eq!(s.capacity, 2);
        assert_eq!(s.len, 2);
        assert_eq!(s.hits, 3);
        assert_eq!(s.misses, 2);
        assert_eq!(s.inserts, 3);
        assert_eq!(s.evictions, 1);

        // `2` should be gone, `1` and `3` present.
        assert!(cache.get(&oid_n(2)).is_none());
        assert!(cache.get(&oid_n(1)).is_some());
        assert!(cache.get(&oid_n(3)).is_some());
    }

    #[test]
    fn put_existing_oid_promotes_and_replaces() {
        let cache = TreeCache::with_capacity(2);
        cache.put(oid_n(1), payload("a"));
        cache.put(oid_n(2), payload("b"));

        // Re-put `1` with new payload. This should not count as an
        // eviction, and `1` should now be MRU so a third put evicts
        // `2`.
        cache.put(oid_n(1), payload("a-new"));
        cache.put(oid_n(3), payload("c"));

        let got = cache.get(&oid_n(1)).expect("present");
        assert_eq!(got[0].name, BString::from("a-new"));
        assert!(cache.get(&oid_n(2)).is_none(), "2 should be evicted");
        assert!(cache.get(&oid_n(3)).is_some());
    }

    #[test]
    fn capacity_zero_disables_caching() {
        let cache = TreeCache::with_capacity(0);
        cache.put(oid_n(1), payload("a"));
        assert!(cache.get(&oid_n(1)).is_none());
        let s = cache.stats();
        assert_eq!(s.len, 0);
        assert_eq!(s.capacity, 0);
        // `put` early-returns when capacity == 0, so even insert
        // attempts don't show up in the counters. Keeps the
        // semantics of `inserts` strictly "things that ended up in
        // the cache".
        assert_eq!(s.inserts, 0);
    }
}
