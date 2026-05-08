//! `GixFetcher`: hydrates single objects via the same gitoxide path
//! Phase 0a validated.
//!
//! See `spikes/ondemand-fetch/RESULTS.md` for the working spike. The
//! technique here is identical: build a single-refspec fetch
//! `+<oid>:refs/projgit/wanted/<oid>` and run the gix
//! `Remote::connect → prepare_fetch → receive` lifecycle.

use super::{Coalescer, Fetcher, FetcherError};
use crate::object_store::ObjectStore;
use gix::ObjectId;
use std::sync::Arc;

/// A [`Fetcher`] backed by gitoxide's blocking transport.
///
/// Drives `gix`'s blocking `Remote` lifecycle through the very same
/// [`gix::ThreadSafeRepository`] the [`ObjectStore`] reads from. That
/// shared handle is what makes a freshly-written pack visible to
/// subsequent reads on the store side: a separate `gix::open` would
/// snapshot its own odb state and never see new packs written by us.
/// The [`Coalescer`] inside ensures concurrent calls for the same
/// OID issue exactly one network fetch.
pub struct GixFetcher {
    store: Arc<ObjectStore>,
    remote_name: String,
    coalescer: Coalescer<ObjectId, ()>,
}

impl GixFetcher {
    /// Open a Fetcher against the same git directory the
    /// [`ObjectStore`] uses, configured to fetch from `remote_name`
    /// (typically `"origin"`).
    pub fn open(
        store: Arc<ObjectStore>,
        remote_name: impl Into<String>,
    ) -> Result<Self, GixFetcherError> {
        Ok(Self {
            store,
            remote_name: remote_name.into(),
            coalescer: Coalescer::new(),
        })
    }

    /// Issue a single fetch via the gix Remote API. Internal; bypasses
    /// the coalescer so the coalescer can call us once.
    fn raw_fetch(&self, oid: ObjectId) -> Result<(), FetcherError> {
        use gix::remote::Direction;

        // Per-thread Repository handle (cheap, doesn't carry shared
        // mutable state). Critically, this comes from the same
        // `ThreadSafeRepository` the store uses, so the pack we're
        // about to write becomes visible to subsequent reads.
        let repo = self.store.handle_for_fetch();

        let refspec = format!("+{oid}:refs/projgit/wanted/{oid}");

        let mut remote = repo
            .find_remote(self.remote_name.as_str())
            .map_err(|e| FetcherError::Backend(oid, format!("find_remote: {e}")))?;
        remote
            .replace_refspecs([refspec.as_str()], Direction::Fetch)
            .map_err(|e| FetcherError::Backend(oid, format!("replace_refspecs: {e}")))?;

        let conn = remote
            .connect(Direction::Fetch)
            .map_err(|e| FetcherError::Transport(oid, e.to_string()))?;

        let prep = conn
            .prepare_fetch(&mut gix::progress::Discard, Default::default())
            .map_err(|e| FetcherError::Transport(oid, format!("prepare_fetch: {e}")))?;

        let _outcome = prep
            .receive(&mut gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
            .map_err(|e| {
                // The server may legitimately refuse an OID (unknown
                // / unreachable). Surface that distinctly.
                let msg = e.to_string();
                if msg.contains("not our ref") || msg.contains("upload-pack: not our ref") {
                    FetcherError::Refused(oid, msg)
                } else {
                    FetcherError::Transport(oid, msg)
                }
            })?;

        // gix wrote a new pack but the in-memory `ObjectStore`'s
        // pack list might still be stale. The store's `try_find_object`
        // refreshes lazily; verify the object is now visible.
        if !self.store.contains(oid) {
            return Err(FetcherError::NotPresentAfterFetch(oid));
        }
        Ok(())
    }
}

impl Fetcher for GixFetcher {
    fn fetch_object(&self, oid: ObjectId) -> Result<(), FetcherError> {
        // Fast path: already present, no network needed.
        if self.store.contains(oid) {
            return Ok(());
        }
        // Slow path: coalesce concurrent fetches for the same OID.
        self.coalescer
            .do_or_join(oid, || self.raw_fetch(oid))
            .map_err(|s| {
                // The coalescer collapses the typed error to a String for
                // sharing across threads. Re-classify by content; if we
                // can't, surface as Backend.
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

/// Construction errors for [`GixFetcher`].
#[derive(Debug, thiserror::Error)]
pub enum GixFetcherError {
    /// gix could not open the git directory.
    #[error("gix open failed: {0}")]
    Open(String),
}
