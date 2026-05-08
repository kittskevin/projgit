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
use std::path::Path;
use std::sync::Arc;

/// A [`Fetcher`] backed by gitoxide's blocking transport.
///
/// Owns its own `gix::ThreadSafeRepository` handle so the underlying
/// state is shareable across threads. The [`Coalescer`] inside ensures
/// concurrent calls for the same OID issue exactly one network fetch.
pub struct GixFetcher {
    repo: gix::ThreadSafeRepository,
    remote_name: String,
    coalescer: Coalescer<ObjectId, ()>,
    store: Arc<ObjectStore>,
}

impl GixFetcher {
    /// Open a Fetcher against the same git directory the
    /// [`ObjectStore`] uses, configured to fetch from `remote_name`
    /// (typically `"origin"`).
    pub fn open(
        store: Arc<ObjectStore>,
        remote_name: impl Into<String>,
    ) -> Result<Self, GixFetcherError> {
        let git_dir = store.git_dir().to_path_buf();
        Self::open_at(git_dir, store, remote_name)
    }

    /// Variant for tests / advanced callers that want to point the
    /// fetcher at a different git directory than the store reads from.
    /// Most callers should use [`Self::open`].
    pub fn open_at(
        git_dir: impl AsRef<Path>,
        store: Arc<ObjectStore>,
        remote_name: impl Into<String>,
    ) -> Result<Self, GixFetcherError> {
        let repo = gix::open(git_dir.as_ref())
            .map_err(|e| GixFetcherError::Open(e.to_string()))?
            .into_sync();
        Ok(Self {
            repo,
            remote_name: remote_name.into(),
            coalescer: Coalescer::new(),
            store,
        })
    }

    /// Issue a single fetch via the gix Remote API. Internal; bypasses
    /// the coalescer so the coalescer can call us once.
    fn raw_fetch(&self, oid: ObjectId) -> Result<(), FetcherError> {
        use gix::remote::Direction;

        // Per-thread Repository handle (cheap, doesn't carry shared
        // mutable state). This is the gix idiom from §5.3.
        let repo = self.repo.to_thread_local();

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
