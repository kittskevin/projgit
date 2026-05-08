//! `NoopFetcher`: a Fetcher that never hydrates anything.
//!
//! Used by tests, by callers that want to assert "this projection
//! must be fully local," and as the default when no remote is
//! configured.

use super::{Fetcher, FetcherError};
use gix::ObjectId;

/// A [`Fetcher`] that always returns [`FetcherError::NotHydratable`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopFetcher;

impl NoopFetcher {
    /// Construct a NoopFetcher.
    pub fn new() -> Self {
        Self
    }
}

impl Fetcher for NoopFetcher {
    fn fetch_object(&self, oid: ObjectId) -> Result<(), FetcherError> {
        Err(FetcherError::NotHydratable(oid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_returns_not_hydratable() {
        let f = NoopFetcher::new();
        let oid = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let err = f.fetch_object(oid).unwrap_err();
        assert!(matches!(err, FetcherError::NotHydratable(o) if o == oid));
    }
}
