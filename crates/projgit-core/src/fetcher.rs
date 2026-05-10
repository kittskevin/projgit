//! On-demand object hydration.
//!
//! See `docs/initial-plan.md` §5.4 and Phase 0a results
//! (`spikes/ondemand-fetch/RESULTS.md`).
//!
//! ## Design choices for MVP
//!
//! - **Sync trait, not async.** FS frontend callbacks (FUSE / WinFsp)
//!   are synchronous, gix's transport is blocking under our chosen
//!   feature set, and going async would force every downstream consumer
//!   to also be async for no immediate benefit. We can add an async
//!   companion later without breaking the sync trait.
//! - **Single-flight built on stdlib primitives.** No tokio dep on the
//!   read path; concurrent calls for the same OID share one underlying
//!   fetch via a small [`Coalescer`].
//! - **Two layers.** [`ObjectStore`] stays pure read-only;
//!   [`HydratingObjectStore`] wraps it with a [`Fetcher`] and turns
//!   `MissingObject(oid)` into a hydrate-then-retry on the read path.
//!   Preserves the §5.3 invariant that the store never networks.

use crate::error::ObjectStoreError;
use crate::object_store::{ObjectKind, ObjectStore};
use gix::ObjectId;
use std::sync::Arc;

mod coalesce;
mod git_cli;
#[cfg(feature = "gix-fetcher")]
mod gix_fetcher;
mod noop;

pub use coalesce::Coalescer;
pub use git_cli::{GitCliFetcher, GitCliFetcherError};
#[cfg(feature = "gix-fetcher")]
pub use gix_fetcher::{GixFetcher, GixFetcherError};
pub use noop::NoopFetcher;

/// Hydrates a single git object by OID into the local store.
///
/// Implementations are responsible for:
/// 1. Consulting whatever remote the projection was set up against.
/// 2. Writing the resulting object(s) into the same on-disk store the
///    [`ObjectStore`] reads from.
/// 3. Returning **after** the object is durable enough that a
///    subsequent [`ObjectStore::contains`] returns `true`.
///
/// Concurrency: implementations may be called from many threads
/// concurrently for the same OID. Use [`Coalescer`] (or a similar
/// single-flight wrapper) to avoid duplicate network round-trips.
pub trait Fetcher: Send + Sync {
    /// Fetch `oid` from the remote and ensure it lands in the local
    /// object store. Returns immediately with `Ok(())` if the object
    /// is already present.
    fn fetch_object(&self, oid: ObjectId) -> Result<(), FetcherError>;

    /// Prefetch a batch of object headers in one upstream round
    /// trip if the implementation can; otherwise a per-OID fallback.
    ///
    /// Used by the T1 readdir-time prefetch worker (see
    /// `docs/design/prefetch.md`). On success, the implementation
    /// **must** ensure each present OID's header is decodable via
    /// [`crate::ObjectStore::header`] without a further upstream
    /// round trip. Implementations that can answer multiple OIDs in
    /// one round trip (notably [`GitCliFetcher`] via
    /// `git cat-file --batch-check`) override this; the default
    /// falls back to one [`Self::fetch_object`] per OID, which is
    /// correct but defeats the purpose of the optimisation.
    ///
    /// Returns one [`HeaderProbe`] per input OID, in the same order.
    /// Errors per OID don't abort the batch; they're surfaced in
    /// the corresponding `HeaderProbe::Error` variant so callers can
    /// keep going for the rest. Hard transport-level failures may
    /// be reported by returning an `Error` variant for every OID.
    fn prefetch_headers(&self, oids: &[ObjectId]) -> Vec<HeaderProbe> {
        oids.iter()
            .map(|oid| match self.fetch_object(*oid) {
                Ok(()) => HeaderProbe::Present(*oid),
                Err(e) => HeaderProbe::Error(*oid, e),
            })
            .collect()
    }
}

/// One result from a [`Fetcher::prefetch_headers`] batch.
///
/// The `(kind, size)` payload is optional because the default
/// trait impl can prove the object is present (via `fetch_object`)
/// but not cheaply read its header -- callers that need the
/// header should fall through to [`crate::ObjectStore::header`]
/// after seeing `Present`. Specialised impls like
/// [`GitCliFetcher`]'s `cat-file --batch-check` get the
/// `(kind, size)` for free and report it directly via
/// `PresentWithHeader`, which the prefetch worker can publish
/// straight to the header cache without re-reading via gix.
///
/// Not `Clone`: the `Error` variant carries a [`FetcherError`],
/// which is intentionally non-`Clone` to keep the error payload
/// honest about uniqueness.
#[derive(Debug)]
pub enum HeaderProbe {
    /// The OID is locally present after this call. The header can
    /// be read via `ObjectStore::header` without another upstream
    /// round trip.
    Present(ObjectId),
    /// The OID is present and the implementation also got the
    /// header for free. The prefetch worker can publish this
    /// directly to the header cache.
    PresentWithHeader(ObjectId, ObjectKind, u64),
    /// The OID could not be made present (server refused, no
    /// network, etc.). The on-demand path will retry naturally.
    Error(ObjectId, FetcherError),
}

/// Errors a [`Fetcher`] may surface.
///
/// Distinct from [`ObjectStoreError`] so the FS frontends can decide
/// (e.g.) whether to retry, surface as `EIO`, or surface as `ENOENT`.
#[derive(Debug, thiserror::Error)]
pub enum FetcherError {
    /// The remote refused the object (e.g. it doesn't exist or the
    /// server lacks `allow-tip-sha1-in-want`).
    #[error("remote refused object {0}: {1}")]
    Refused(ObjectId, String),

    /// Network or transport-layer failure.
    #[error("transport error fetching {0}: {1}")]
    Transport(ObjectId, String),

    /// Object was fetched without protocol error but is somehow still
    /// not present in the local store.
    #[error("post-fetch verification failed for {0}: object still absent")]
    NotPresentAfterFetch(ObjectId),

    /// This Fetcher cannot hydrate (e.g., [`NoopFetcher`]).
    #[error("fetcher cannot hydrate {0}")]
    NotHydratable(ObjectId),

    /// Catch-all for backend-specific errors.
    #[error("fetch backend error for {0}: {1}")]
    Backend(ObjectId, String),
}

/// Composes an [`ObjectStore`] with a [`Fetcher`] so reads transparently
/// hydrate missing objects.
///
/// Architectural note: this is a separate type rather than methods on
/// `ObjectStore` so the read-only invariant on `ObjectStore` stays
/// intact and tests / callers that explicitly want "no network ever"
/// can still use a bare `ObjectStore`.
pub struct HydratingObjectStore<F> {
    store: Arc<ObjectStore>,
    fetcher: F,
}

impl<F: Fetcher> HydratingObjectStore<F> {
    /// Compose a store with a fetcher.
    pub fn new(store: Arc<ObjectStore>, fetcher: F) -> Self {
        Self { store, fetcher }
    }

    /// Borrow the underlying store.
    pub fn store(&self) -> &ObjectStore {
        &self.store
    }

    /// Read a blob. On `MissingObject`, call the fetcher and retry once.
    /// Any further misses propagate as `NotPresentAfterFetch`.
    pub fn read_blob(&self, oid: ObjectId) -> Result<Vec<u8>, HydrateError> {
        match self.store.read_blob(oid) {
            Ok(bytes) => Ok(bytes),
            Err(ObjectStoreError::MissingObject(_)) => {
                self.fetcher.fetch_object(oid)?;
                self.store.read_blob(oid).map_err(|e| match e {
                    ObjectStoreError::MissingObject(o) => {
                        HydrateError::Fetcher(FetcherError::NotPresentAfterFetch(o))
                    }
                    other => HydrateError::Store(other),
                })
            }
            Err(e) => Err(HydrateError::Store(e)),
        }
    }

    /// Read and parse a tree. Same hydrate-on-miss policy as
    /// [`Self::read_blob`].
    pub fn read_tree(
        &self,
        oid: ObjectId,
    ) -> Result<Vec<crate::object_store::RawTreeEntry>, HydrateError> {
        match self.store.read_tree(oid) {
            Ok(entries) => Ok(entries),
            Err(ObjectStoreError::MissingObject(_)) => {
                self.fetcher.fetch_object(oid)?;
                self.store.read_tree(oid).map_err(|e| match e {
                    ObjectStoreError::MissingObject(o) => {
                        HydrateError::Fetcher(FetcherError::NotPresentAfterFetch(o))
                    }
                    other => HydrateError::Store(other),
                })
            }
            Err(e) => Err(HydrateError::Store(e)),
        }
    }

    /// Read header (kind + size). Same hydrate-on-miss policy.
    pub fn header(&self, oid: ObjectId) -> Result<(ObjectKind, u64), HydrateError> {
        match self.store.header(oid) {
            Ok(h) => Ok(h),
            Err(ObjectStoreError::MissingObject(_)) => {
                self.fetcher.fetch_object(oid)?;
                self.store.header(oid).map_err(|e| match e {
                    ObjectStoreError::MissingObject(o) => {
                        HydrateError::Fetcher(FetcherError::NotPresentAfterFetch(o))
                    }
                    other => HydrateError::Store(other),
                })
            }
            Err(e) => Err(HydrateError::Store(e)),
        }
    }

    /// Prefetch a batch of object headers via the fetcher and
    /// publish results to the underlying [`ObjectStore`]'s header
    /// cache so subsequent on-demand `header()` calls are warm.
    ///
    /// Skips OIDs whose header is already cached -- no IPC, no
    /// upstream call, no fetcher invocation. Used by the T1
    /// readdir-time prefetch worker (see `docs/design/prefetch.md`).
    ///
    /// Returns the underlying [`HeaderProbe`] results so callers
    /// can update their own counters / surface errors. Errors are
    /// per-OID; one failure does not poison the rest of the batch.
    pub fn prefetch_headers(&self, oids: &[ObjectId]) -> Vec<HeaderProbe> {
        // Cache-hit pre-pass: avoid even constructing a query for
        // OIDs whose header is already resolvable locally. Each
        // `store.header()` call hits the header LRU first, then
        // gix's local odb (mmap'd packs / loose objects). Both are
        // microseconds; we trade them against avoiding an upstream
        // RTT.
        let mut to_query: Vec<ObjectId> = Vec::with_capacity(oids.len());
        for &oid in oids {
            if self.store.header(oid).is_err() {
                to_query.push(oid);
            }
        }

        if to_query.is_empty() {
            // Synthesise Present results so the caller still sees a
            // probe per input OID.
            return oids.iter().map(|oid| HeaderProbe::Present(*oid)).collect();
        }

        let probes = self.fetcher.prefetch_headers(&to_query);

        // Publish PresentWithHeader results to the cache; for
        // bare Present results, do a one-shot store.header() to
        // populate the cache via the normal path.
        for probe in &probes {
            match probe {
                HeaderProbe::PresentWithHeader(oid, kind, size) => {
                    self.store.put_header_cache(*oid, *kind, *size);
                }
                HeaderProbe::Present(oid) => {
                    let _ = self.store.header(*oid);
                }
                HeaderProbe::Error(_, _) => {}
            }
        }

        // Reassemble: preserve the original order. `probes` only
        // covers `to_query`; the rest were already-cached and get
        // synthesised Present results.
        let mut by_oid: std::collections::HashMap<ObjectId, HeaderProbe> =
            std::collections::HashMap::with_capacity(probes.len());
        for probe in probes {
            let oid = match &probe {
                HeaderProbe::Present(o) => *o,
                HeaderProbe::PresentWithHeader(o, _, _) => *o,
                HeaderProbe::Error(o, _) => *o,
            };
            by_oid.insert(oid, probe);
        }
        oids.iter()
            .map(|oid| by_oid.remove(oid).unwrap_or(HeaderProbe::Present(*oid)))
            .collect()
    }
}

/// Errors that can arise when reading through a [`HydratingObjectStore`].
#[derive(Debug, thiserror::Error)]
pub enum HydrateError {
    /// The object store layer failed (kind mismatch, backend error, etc.).
    #[error(transparent)]
    Store(#[from] ObjectStoreError),

    /// The fetcher could not hydrate the object.
    #[error(transparent)]
    Fetcher(#[from] FetcherError),
}
