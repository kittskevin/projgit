//! [`DaemonFetcher`] — a [`projgit_core::Fetcher`] impl that hydrates
//! objects by asking a long-running `projgitd` daemon over the
//! daemon's unix-socket control plane.
//!
//! Stage 3 of the projgitd plan: the sidecar holds the `/dev/fuse`
//! fd and runs the FUSE protocol loop locally; cold-path object
//! hydration coordinates with the daemon via this fetcher. Bytes
//! never cross the unix socket — the daemon writes packs into the
//! shared on-disk CAS, the sidecar's `ObjectStore` reads them via
//! gix's mmap path. See
//! [`docs/design/projgitd.md`](../../../../docs/design/projgitd.md)
//! §4 / §5 for the full data-plane walk-through.
//!
//! V1 (this module) opens a fresh `UnixStream` per call. The
//! existing protocol is one-shot per connection (`write` → `read` →
//! close), so this is the simplest correct implementation and is
//! reconnect-friendly by construction: if the daemon dies and
//! restarts, the next call connects to the new instance with no
//! state to migrate. A connection pool / pipelined connection is a
//! later optimisation if profiling cares.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use crate::protocol::{codes, read_message, write_message, HeaderProbeWire, Request, Response};
use gix::ObjectId;
use projgit_core::{Fetcher, FetcherError, HeaderProbe, ObjectKind};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// A [`Fetcher`] that hydrates objects via a remote `projgitd`.
///
/// Cheap to `Clone` — internally just a [`PathBuf`] socket path.
/// `Send + Sync` because it holds no mutable state; each call opens
/// its own `UnixStream`.
#[derive(Debug, Clone)]
pub struct DaemonFetcher {
    socket_path: PathBuf,
}

impl DaemonFetcher {
    /// Construct a fetcher that talks to the daemon listening at
    /// `socket_path`. The path must exist by the time the first
    /// fetch happens; this constructor doesn't validate (the FS
    /// frontend's cold-path miss is the natural failure point).
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Socket path the fetcher dials. Exposed for diagnostics /
    /// tests.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Open a fresh `UnixStream`, write `req`, read one `Response`.
    /// Errors are flattened into a `String` for `map_to_fetcher_err`
    /// to translate; this keeps the IO/protocol error paths from
    /// leaking into the `Fetcher` trait's narrow error surface.
    fn rpc(&self, req: &Request) -> Result<Response, String> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| format!("connect {}: {e}", self.socket_path.display()))?;
        write_message(&mut stream, req).map_err(|e| format!("write: {e}"))?;
        read_message::<_, Response>(&mut stream).map_err(|e| format!("read: {e}"))
    }
}

impl Fetcher for DaemonFetcher {
    fn fetch_object(&self, oid: ObjectId) -> Result<(), FetcherError> {
        let req = Request::Fetch {
            oid: oid.to_string(),
        };
        match self.rpc(&req) {
            Ok(Response::Ok) => Ok(()),
            Ok(Response::Err { code, message }) => Err(daemon_err_to_fetcher(oid, &code, message)),
            Ok(other) => Err(FetcherError::Backend(
                oid,
                format!("unexpected response to Fetch: {other:?}"),
            )),
            Err(transport) => Err(FetcherError::Transport(oid, transport)),
        }
    }

    fn prefetch_headers(&self, oids: &[ObjectId]) -> Vec<HeaderProbe> {
        if oids.is_empty() {
            return Vec::new();
        }
        let req = Request::PrefetchHeaders {
            oids: oids.iter().map(ObjectId::to_string).collect(),
        };
        match self.rpc(&req) {
            Ok(Response::HeaderProbes { probes }) => merge_probes(oids, probes),
            // The daemon returned an error before producing per-OID
            // probes (e.g. NOT_ATTACHED, BAD_OID, or a transport
            // hiccup). Surface the same error for every OID so the
            // caller can degrade gracefully and the prefetch worker
            // doesn't silently lose half the batch.
            Ok(Response::Err { code, message }) => oids
                .iter()
                .map(|oid| {
                    HeaderProbe::Error(*oid, daemon_err_to_fetcher(*oid, &code, message.clone()))
                })
                .collect(),
            Ok(other) => oids
                .iter()
                .map(|oid| {
                    HeaderProbe::Error(
                        *oid,
                        FetcherError::Backend(
                            *oid,
                            format!("unexpected response to PrefetchHeaders: {other:?}"),
                        ),
                    )
                })
                .collect(),
            Err(transport) => oids
                .iter()
                .map(|oid| {
                    HeaderProbe::Error(*oid, FetcherError::Transport(*oid, transport.clone()))
                })
                .collect(),
        }
    }
}

/// Translate a daemon error response into a [`FetcherError`] for the
/// given OID. Unknown codes fall through to `Backend` so the wire
/// surface is forward-compatible.
fn daemon_err_to_fetcher(oid: ObjectId, code: &str, message: String) -> FetcherError {
    match code {
        codes::BAD_OID => FetcherError::Backend(oid, format!("bad_oid: {message}")),
        codes::NOT_ATTACHED => FetcherError::Backend(oid, format!("not_attached: {message}")),
        codes::FETCH_FAILED => {
            FetcherError::Backend(oid, format!("daemon fetch_failed: {message}"))
        }
        codes::SOURCE_MISMATCH => FetcherError::Backend(oid, format!("source_mismatch: {message}")),
        codes::INTERNAL => FetcherError::Backend(oid, format!("daemon internal: {message}")),
        other => FetcherError::Backend(oid, format!("daemon {other}: {message}")),
    }
}

/// Convert per-OID wire probes back into in-process [`HeaderProbe`]s,
/// reassembled in the original request order. If the daemon omitted
/// or duplicated probes, we synthesise an `Error` for any missing
/// OID and drop duplicates beyond the first.
fn merge_probes(oids: &[ObjectId], wire: Vec<HeaderProbeWire>) -> Vec<HeaderProbe> {
    use std::collections::HashMap;

    let mut by_oid: HashMap<ObjectId, HeaderProbe> = HashMap::with_capacity(wire.len());
    for probe in wire {
        match probe {
            HeaderProbeWire::Present { oid } => {
                if let Ok(parsed) = ObjectId::from_hex(oid.as_bytes()) {
                    by_oid.entry(parsed).or_insert(HeaderProbe::Present(parsed));
                }
            }
            HeaderProbeWire::PresentWithHeader {
                oid,
                object_kind,
                size,
            } => {
                if let (Ok(parsed), Some(kind)) = (
                    ObjectId::from_hex(oid.as_bytes()),
                    parse_object_kind(&object_kind),
                ) {
                    by_oid
                        .entry(parsed)
                        .or_insert(HeaderProbe::PresentWithHeader(parsed, kind, size));
                }
            }
            HeaderProbeWire::HeaderOnly {
                oid,
                object_kind,
                size,
            } => {
                if let (Ok(parsed), Some(kind)) = (
                    ObjectId::from_hex(oid.as_bytes()),
                    parse_object_kind(&object_kind),
                ) {
                    by_oid
                        .entry(parsed)
                        .or_insert(HeaderProbe::HeaderOnly(parsed, kind, size));
                }
            }
            HeaderProbeWire::Error { oid, code, message } => {
                if let Ok(parsed) = ObjectId::from_hex(oid.as_bytes()) {
                    by_oid.entry(parsed).or_insert(HeaderProbe::Error(
                        parsed,
                        daemon_err_to_fetcher(parsed, &code, message),
                    ));
                }
            }
        }
    }

    oids.iter()
        .map(|oid| {
            by_oid.remove(oid).unwrap_or_else(|| {
                HeaderProbe::Error(
                    *oid,
                    FetcherError::Backend(*oid, "daemon omitted probe for this OID".to_owned()),
                )
            })
        })
        .collect()
}

fn parse_object_kind(s: &str) -> Option<ObjectKind> {
    match s {
        "blob" => Some(ObjectKind::Blob),
        "tree" => Some(ObjectKind::Tree),
        "commit" => Some(ObjectKind::Commit),
        "tag" => Some(ObjectKind::Tag),
        _ => None,
    }
}

// -----------------------------------------------------------------------------
// Unit tests — translation helpers only. The end-to-end "DaemonFetcher
// against a real running daemon" test lives in
// `tests/daemon_fetcher_smoke.rs` to keep this module side-effect free.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_oid(byte: u8) -> ObjectId {
        ObjectId::from_hex(&[byte_char(byte); 40]).unwrap()
    }

    fn byte_char(byte: u8) -> u8 {
        // Map 0..16 to '0'..'9','a'..'f'; outside that range, default to '0'.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        HEX[(byte as usize) % HEX.len()]
    }

    #[test]
    fn merge_probes_preserves_order_and_fills_gaps() {
        let oids = vec![fake_oid(1), fake_oid(2), fake_oid(3)];
        // Daemon returned only probes for oids[0] and oids[2].
        let wire = vec![
            HeaderProbeWire::Present {
                oid: oids[0].to_string(),
            },
            HeaderProbeWire::PresentWithHeader {
                oid: oids[2].to_string(),
                object_kind: "blob".into(),
                size: 7,
            },
        ];
        let probes = merge_probes(&oids, wire);
        assert_eq!(probes.len(), 3);
        match &probes[0] {
            HeaderProbe::Present(o) => assert_eq!(*o, oids[0]),
            other => panic!("probe[0] = {other:?}"),
        }
        match &probes[1] {
            HeaderProbe::Error(o, _) => assert_eq!(*o, oids[1]),
            other => panic!("probe[1] = {other:?}"),
        }
        match &probes[2] {
            HeaderProbe::PresentWithHeader(o, k, s) => {
                assert_eq!(*o, oids[2]);
                assert_eq!(*k, ObjectKind::Blob);
                assert_eq!(*s, 7);
            }
            other => panic!("probe[2] = {other:?}"),
        }
    }

    #[test]
    fn merge_probes_handles_error_variant() {
        let oid = fake_oid(4);
        let wire = vec![HeaderProbeWire::Error {
            oid: oid.to_string(),
            code: codes::FETCH_FAILED.into(),
            message: "boom".into(),
        }];
        let probes = merge_probes(&[oid], wire);
        match &probes[0] {
            HeaderProbe::Error(o, FetcherError::Backend(_, msg)) => {
                assert_eq!(*o, oid);
                assert!(msg.contains("fetch_failed"), "msg = {msg:?}");
            }
            other => panic!("probe[0] = {other:?}"),
        }
    }

    #[test]
    fn daemon_err_codes_map_to_backend_variant() {
        let oid = fake_oid(5);
        for code in [
            codes::BAD_OID,
            codes::NOT_ATTACHED,
            codes::FETCH_FAILED,
            codes::SOURCE_MISMATCH,
            codes::INTERNAL,
            "future_unknown_code",
        ] {
            match daemon_err_to_fetcher(oid, code, "x".into()) {
                FetcherError::Backend(o, msg) => {
                    assert_eq!(o, oid);
                    assert!(msg.contains(code) || msg.contains("daemon"));
                }
                other => panic!("code {code}: got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_object_kind_round_trips() {
        assert_eq!(parse_object_kind("blob"), Some(ObjectKind::Blob));
        assert_eq!(parse_object_kind("tree"), Some(ObjectKind::Tree));
        assert_eq!(parse_object_kind("commit"), Some(ObjectKind::Commit));
        assert_eq!(parse_object_kind("tag"), Some(ObjectKind::Tag));
        assert_eq!(parse_object_kind("nope"), None);
    }
}
