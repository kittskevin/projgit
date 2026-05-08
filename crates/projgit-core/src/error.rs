//! Typed errors for the core crate.
//!
//! Two principles:
//! 1. `MissingObject(oid)` is its own variant on [`ObjectStoreError`] so
//!    the Fetcher's hot path can pattern-match it cheaply without
//!    inspecting `gix` internals.
//! 2. Higher layers wrap `ObjectStoreError` rather than collapsing it,
//!    so the missing-object signal survives all the way to the FS
//!    callback that triggered the read.

use gix::ObjectId;
use std::path::PathBuf;

/// Errors from [`crate::object_store::ObjectStore`].
#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreError {
    /// The object does not exist in the local store. The Fetcher
    /// intercepts this variant and triggers hydration.
    #[error("object {0} is not present in the store")]
    MissingObject(ObjectId),

    /// Failed to open the underlying git directory.
    #[error("failed to open git directory at {path}: {source}")]
    Open {
        /// The path we tried to open.
        path: PathBuf,
        /// Underlying gix error. Boxed because the inner error is
        /// large and we want `Result<_, ObjectStoreError>` to stay cheap.
        #[source]
        source: Box<gix::open::Error>,
    },

    /// The object exists but its kind disagrees with what the caller
    /// asked for (e.g. asked for a tree, found a blob).
    #[error("object {oid} has kind {actual:?}, expected {expected:?}")]
    UnexpectedKind {
        /// The OID in question.
        oid: ObjectId,
        /// What the caller wanted.
        expected: crate::object_store::ObjectKind,
        /// What was actually stored.
        actual: crate::object_store::ObjectKind,
    },

    /// Anything else from the underlying gix layer.
    #[error("gix backend error: {0}")]
    Backend(String),
}

/// Errors from [`crate::projection::Projection`] resolution.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    /// Path component does not exist at this point in the tree.
    #[error("path component {component:?} not found under {parent:?}")]
    NotFound {
        /// The missing component.
        component: String,
        /// The parent path that was being walked.
        parent: String,
    },

    /// Tried to descend into something that isn't a tree.
    #[error("path component {component:?} under {parent:?} is not a directory")]
    NotADirectory {
        /// The non-directory component name.
        component: String,
        /// Parent path.
        parent: String,
    },

    /// The virtual path is malformed (e.g. contains `..` or null bytes).
    #[error("invalid virtual path {path:?}: {reason}")]
    InvalidPath {
        /// The offending path.
        path: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// An error from the object store (typically [`ObjectStoreError::MissingObject`]).
    #[error(transparent)]
    Store(#[from] ObjectStoreError),
}
