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

    #[test]
    fn noop_fetch_objects_errors_per_oid_in_order() {
        use crate::HeaderProbe;
        let f = NoopFetcher::new();
        let a = gix::ObjectId::null(gix::hash::Kind::Sha1);
        let b = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let probes = f.fetch_objects(&[a, b]);
        assert_eq!(probes.len(), 2);
        assert!(matches!(&probes[0], HeaderProbe::Error(o, _) if *o == a));
        assert!(matches!(&probes[1], HeaderProbe::Error(o, _) if *o == b));
    }
}
