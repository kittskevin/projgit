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
mod gix_fetcher;
mod noop;

pub use coalesce::Coalescer;
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
