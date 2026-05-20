//! Control-plane protocol between `projgitd` and its clients.
//!
//! Wire format: a single message is a 4-byte big-endian length prefix
//! followed by a JSON body. Request/response is strictly one-shot per
//! connection in V1 — the client opens a connection, sends one request,
//! reads one response, closes. Multiplexing / pipelining is a Stage 2+
//! follow-up if profiling ever shows it matters.
//!
//! Always-compile (no platform-specific types here) so future client
//! crates on Windows can build against this module without pulling in
//! the FUSE-bound server.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::PathBuf;

/// Maximum accepted message size (length prefix). 1 MiB is far more
/// than any request / response we expect — guards against an
/// adversarial peer trying to make us allocate gigabytes.
pub const MAX_MSG_LEN: usize = 1024 * 1024;

/// Control-plane request from a client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check. Daemon responds with [`Response::Pong`].
    Ping,
    /// Snapshot of the daemon's mount registry and cache counters.
    Status,
    /// Mount a projection of `source` at `mountpoint` for ref `ref_name`.
    ///
    /// The daemon canonicalises `source` and reuses the existing
    /// `ObjectStore` / `Fetcher` for that source if any prior mount
    /// referenced it (the §1.6-in-memory amortisation property).
    Mount {
        /// URL or local path of the upstream repo.
        source: String,
        /// Ref name to project (short or full). `HEAD` works.
        #[serde(rename = "ref")]
        ref_name: String,
        /// Where to mount on the daemon's host. Must already exist as
        /// an empty directory.
        mountpoint: PathBuf,
        /// Skip the synthesised `.git/` overlay. See
        /// `projgit mount --no-dotgit` for the semantics.
        #[serde(default)]
        no_dotgit: bool,
        /// Set `allow_other` so non-mounter UIDs (e.g. containers)
        /// can read the mount. See `projgit mount --allow-other`.
        #[serde(default)]
        allow_other: bool,
    },
    /// Unmount a previously-mounted projection. `mountpoint` must
    /// match what was passed to a prior [`Request::Mount`].
    Umount { mountpoint: PathBuf },
    /// Graceful shutdown: daemon unmounts everything, closes the
    /// listener, and exits with status 0.
    Shutdown,
}

/// Control-plane response from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// Reply to [`Request::Ping`].
    Pong,
    /// Generic success for operations that don't carry a payload
    /// (`Mount`, `Umount`, `Shutdown`).
    Ok,
    /// Reply to [`Request::Status`].
    Status(StatusReport),
    /// Operation failed. `code` is a short stable identifier the
    /// client can match on; `message` is human-readable detail.
    Err { code: String, message: String },
}

/// Snapshot of daemon state returned by [`Request::Status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Source repository the daemon is currently bound to, or
    /// `None` if no `Mount` request has arrived yet. V1 supports
    /// one source per daemon (see `docs/implementation/projgitd-plan.md`
    /// §2 — multi-source per daemon is a Stage 5+ extension).
    pub source: Option<String>,
    /// Active mounts.
    pub mounts: Vec<MountInfo>,
    /// Shared `ObjectStore` cache counters. `None` if no source
    /// is attached yet.
    pub cache: Option<CacheStats>,
}

/// One row of [`StatusReport::mounts`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub mountpoint: PathBuf,
    pub projection_id: u64,
}

/// Subset of cache counters surfaced over the wire. Mirrors the
/// per-cache `Stats` types in `projgit-core` but kept narrow so the
/// protocol doesn't drag in those structs as wire-types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub tree_hits: u64,
    pub tree_misses: u64,
    pub header_hits: u64,
    pub header_misses: u64,
    pub blob_hits: u64,
    pub blob_misses: u64,
}

/// Stable error codes for [`Response::Err`]. Strings (not an enum)
/// because clients deserialise messages they may not understand;
/// unknown codes are forward-compatible.
pub mod codes {
    /// Mount syscall / FUSE setup failure.
    pub const MOUNT_FAILED: &str = "mount_failed";
    /// Mountpoint already in use by a prior `Mount` request.
    pub const MOUNTPOINT_BUSY: &str = "mountpoint_busy";
    /// Mountpoint specified for `Umount` was never registered.
    pub const NO_SUCH_MOUNT: &str = "no_such_mount";
    /// A `Mount` request specified a source different from the one
    /// already attached. V1 is one-source-per-daemon.
    pub const SOURCE_MISMATCH: &str = "source_mismatch";
    /// Failed to open / clone the source repository.
    pub const SOURCE_OPEN_FAILED: &str = "source_open_failed";
    /// Failed to resolve the requested ref.
    pub const REF_RESOLVE_FAILED: &str = "ref_resolve_failed";
    /// Wire-protocol violation (oversized message, malformed JSON).
    pub const PROTOCOL_ERROR: &str = "protocol_error";
    /// Internal daemon error (panic, lock poison, etc.).
    pub const INTERNAL: &str = "internal";
}

// -----------------------------------------------------------------------------
// Framing — 4-byte big-endian length prefix + JSON body.
// -----------------------------------------------------------------------------

/// Errors from `read_message` / `write_message`.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("message too large: {0} bytes exceeds MAX_MSG_LEN={MAX_MSG_LEN}")]
    TooLarge(usize),
    #[error("unexpected EOF reading message frame")]
    UnexpectedEof,
}

/// Write a length-prefixed JSON message.
///
/// On success, the entire frame (4 bytes header + body) has been
/// flushed to `writer`.
pub fn write_message<W, T>(writer: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: Write,
    T: Serialize,
{
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_MSG_LEN {
        return Err(FrameError::TooLarge(body.len()));
    }
    let len = body.len() as u32;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Read a length-prefixed JSON message.
///
/// Returns [`FrameError::UnexpectedEof`] if the peer closed before
/// completing a frame; [`FrameError::TooLarge`] if the announced size
/// exceeds [`MAX_MSG_LEN`].
pub fn read_message<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: Read,
    T: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    read_exact_or_eof(reader, &mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    read_exact_or_eof(reader, &mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(FrameError::UnexpectedEof),
        Err(e) => Err(FrameError::Io(e)),
    }
}

// -----------------------------------------------------------------------------
// Tests — protocol roundtrip, framing, error cases.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip<T>(msg: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let mut buf = Vec::new();
        write_message(&mut buf, msg).expect("write");
        let mut cursor = Cursor::new(buf);
        read_message(&mut cursor).expect("read")
    }

    #[test]
    fn ping_roundtrip() {
        match roundtrip::<Request>(&Request::Ping) {
            Request::Ping => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn mount_request_roundtrip() {
        let original = Request::Mount {
            source: "https://example/repo".into(),
            ref_name: "main".into(),
            mountpoint: PathBuf::from("/tmp/mp"),
            no_dotgit: true,
            allow_other: false,
        };
        match roundtrip::<Request>(&original) {
            Request::Mount {
                source,
                ref_name,
                mountpoint,
                no_dotgit,
                allow_other,
            } => {
                assert_eq!(source, "https://example/repo");
                assert_eq!(ref_name, "main");
                assert_eq!(mountpoint, PathBuf::from("/tmp/mp"));
                assert!(no_dotgit);
                assert!(!allow_other);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn response_err_roundtrip() {
        let original = Response::Err {
            code: codes::MOUNTPOINT_BUSY.into(),
            message: "already in use".into(),
        };
        match roundtrip::<Response>(&original) {
            Response::Err { code, message } => {
                assert_eq!(code, codes::MOUNTPOINT_BUSY);
                assert_eq!(message, "already in use");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn status_report_roundtrip() {
        let original = Response::Status(StatusReport {
            uptime_secs: 42,
            source: Some("/path/to/repo".into()),
            mounts: vec![MountInfo {
                ref_name: "main".into(),
                mountpoint: PathBuf::from("/tmp/a"),
                projection_id: 1,
            }],
            cache: Some(CacheStats {
                tree_hits: 10,
                tree_misses: 1,
                header_hits: 0,
                header_misses: 0,
                blob_hits: 5,
                blob_misses: 3,
            }),
        });
        match roundtrip::<Response>(&original) {
            Response::Status(r) => {
                assert_eq!(r.uptime_secs, 42);
                assert_eq!(r.source.as_deref(), Some("/path/to/repo"));
                assert_eq!(r.mounts.len(), 1);
                assert_eq!(r.mounts[0].ref_name, "main");
                let c = r.cache.as_ref().expect("cache");
                assert_eq!(c.tree_hits, 10);
                assert_eq!(c.blob_misses, 3);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unexpected_eof_surfaces() {
        let mut empty = Cursor::new(Vec::new());
        match read_message::<_, Request>(&mut empty) {
            Err(FrameError::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn too_large_rejected_on_read() {
        // 4-byte big-endian header announcing 10 MiB; we never even
        // try to read the body.
        let header = ((MAX_MSG_LEN + 1) as u32).to_be_bytes();
        let mut buf = Cursor::new(header.to_vec());
        match read_message::<_, Request>(&mut buf) {
            Err(FrameError::TooLarge(_)) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_surfaces() {
        // header announcing 5 bytes, body is "hello" (not valid JSON).
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_be_bytes());
        buf.extend_from_slice(b"hello");
        let mut cursor = Cursor::new(buf);
        match read_message::<_, Request>(&mut cursor) {
            Err(FrameError::Json(_)) => {}
            other => panic!("expected Json error, got {other:?}"),
        }
    }
}
