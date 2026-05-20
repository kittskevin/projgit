//! Server implementation: listens on a unix socket, accepts one
//! client at a time per thread, and dispatches incoming
//! [`crate::protocol::Request`]s to the appropriate handler.
//!
//! Stage 2a scaffold: only `Ping`, `Status`, and `Shutdown` are
//! implemented end-to-end. `Mount` / `Umount` return
//! `Response::Err { code: "not_implemented", ... }` and are wired up
//! in Stage 2b.
//!
//! Lifecycle:
//!
//! - `run(config)` opens the listener, installs a Ctrl-C / SIGTERM
//!   handler, and accepts connections in a loop. Each accepted stream
//!   is handed to a fresh thread that runs `handle_connection`.
//! - On signal or `Request::Shutdown`, the accept loop breaks. The
//!   listener drops, then `DaemonState` drops, which drops every
//!   registered `BackgroundSession` and unmounts all served projections
//!   via fuser's Drop impl.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use crate::protocol::{
    codes, read_message, write_message, FrameError, MountInfo, Request, Response, StatusReport,
};
use anyhow::{Context, Result};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Configuration for [`run`].
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path the daemon should `bind(2)` for its control socket. The
    /// caller is responsible for removing any stale file at this path
    /// before calling [`run`] (the daemon does it as a convenience but
    /// won't fight a peer for ownership).
    pub socket_path: PathBuf,
    /// File mode applied to the unix socket via `fchmod`. Defaults to
    /// `0o600` (owner-only) — V1's authentication story (see
    /// `docs/implementation/projgitd-plan.md` §2.2). Multi-user hosts
    /// will want this widened only deliberately.
    pub socket_mode: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            socket_mode: 0o600,
        }
    }
}

/// Choose a default socket path. Prefers `$XDG_RUNTIME_DIR/projgitd.sock`,
/// falls back to `/tmp/projgitd-<uid>.sock`.
fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt).join("projgitd.sock");
        return p;
    }
    let uid = nix::unistd::geteuid().as_raw();
    PathBuf::from(format!("/tmp/projgitd-{uid}.sock"))
}

/// Shared daemon state. Stage 2a holds only the mount registry stub
/// (always empty) and the start time. Stage 2b extends it with the
/// real `ObjectStore`/`Fetcher`/sessions wiring.
struct DaemonState {
    started_at: Instant,
    /// Path the listener is bound to. Used by `request_shutdown` to
    /// self-connect and wake the accept loop — `accept(2)` blocks
    /// otherwise, so setting the atomic alone wouldn\'t reach the
    /// loop until the next genuine client connects.
    socket_path: PathBuf,
    /// Signalled when a `Shutdown` request arrives. The accept loop
    /// checks this on every iteration after the wake-up connection
    /// returns.
    shutdown_requested: AtomicBool,
}

impl DaemonState {
    fn new(socket_path: PathBuf) -> Self {
        Self {
            started_at: Instant::now(),
            socket_path,
            shutdown_requested: AtomicBool::new(false),
        }
    }

    fn status(&self) -> StatusReport {
        StatusReport {
            uptime_secs: self.started_at.elapsed().as_secs(),
            // Stage 2b fills these in for real.
            source: None,
            mounts: Vec::<MountInfo>::new(),
            cache: None,
        }
    }

    fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        // Self-connect to wake the listener\'s accept() call.
        // Errors are intentionally ignored: if the listener has
        // already dropped (e.g. we\'re mid-shutdown anyway), there\'s
        // nothing useful to do.
        let _ = UnixStream::connect(&self.socket_path);
    }

    fn should_shut_down(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }
}

/// Run the daemon. Blocks until shutdown.
///
/// Returns `Ok(())` on graceful shutdown (via `Request::Shutdown` over
/// the socket), `Err(_)` if the listener can\'t be bound. Signal
/// handling is **not** installed here — it\'s a process-wide resource
/// (`ctrlc` can only be set once per process) and belongs in the
/// binary\'s `main()`. Tests that drive `run` directly trigger shutdown
/// by sending [`Request::Shutdown`] over the socket; the daemon binary
/// (`src/main.rs`) wires SIGINT/SIGTERM to the same effect by
/// self-connecting and sending `Shutdown`.
pub fn run(config: DaemonConfig) -> Result<()> {
    // Best-effort cleanup of a stale socket file. If the file exists
    // and a daemon is actively listening on it, this still won't
    // surface a clear conflict — bind() below will fail with EADDRINUSE.
    let _ = std::fs::remove_file(&config.socket_path);

    let listener = UnixListener::bind(&config.socket_path).with_context(|| {
        format!(
            "binding unix socket at {}",
            config.socket_path.display()
        )
    })?;
    nix::sys::stat::fchmodat(
        None,
        &config.socket_path,
        nix::sys::stat::Mode::from_bits_truncate(config.socket_mode as nix::libc::mode_t),
        nix::sys::stat::FchmodatFlags::FollowSymlink,
    )
    .with_context(|| format!("fchmod {:o} on socket", config.socket_mode))?;

    eprintln!(
        "projgitd: listening on {} (socket mode {:o})",
        config.socket_path.display(),
        config.socket_mode
    );

    let state = Arc::new(DaemonState::new(config.socket_path.clone()));

    // Accept loop. Non-blocking would be cleaner but a self-connect
    // wakeup on shutdown is good enough for V1 and matches what most
    // small unix-socket daemons do.
    for stream in listener.incoming() {
        if state.should_shut_down() {
            break;
        }
        match stream {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, state) {
                        eprintln!("projgitd: connection error: {e:#}");
                    }
                });
            }
            Err(e) => {
                eprintln!("projgitd: accept error: {e}");
                // Don't kill the daemon on a transient accept error.
                continue;
            }
        }
    }

    eprintln!("projgitd: shutting down");
    // Drop the listener first so no new connections succeed.
    drop(listener);
    // Remove the socket file so a restart doesn't see a stale path.
    let _ = std::fs::remove_file(&config.socket_path);
    // State drops here. Stage 2b's mount sessions will unmount on this.
    drop(state);
    eprintln!("projgitd: shutdown complete");
    Ok(())
}

/// Per-connection handler. V1 protocol is one request → one response →
/// close, so we read once, dispatch, write once, and let the stream
/// drop.
fn handle_connection(mut stream: UnixStream, state: Arc<DaemonState>) -> Result<()> {
    let request: Request = match read_message(&mut stream) {
        Ok(req) => req,
        Err(FrameError::UnexpectedEof) => {
            // Peer closed without sending — could be the shutdown
            // self-connect wake-up. Treat as a no-op.
            return Ok(());
        }
        Err(e) => {
            let _ = write_message(
                &mut stream,
                &Response::Err {
                    code: codes::PROTOCOL_ERROR.into(),
                    message: format!("{e}"),
                },
            );
            return Ok(());
        }
    };

    let response = dispatch(&state, request);

    if let Err(e) = write_message(&mut stream, &response) {
        // We've already computed the response; if the wire write
        // fails (peer hung up, etc.), there's nothing useful to send.
        eprintln!("projgitd: response write failed: {e}");
    }
    Ok(())
}

/// Pure dispatch logic — factored out of `handle_connection` so it's
/// easy to unit-test without involving real sockets.
fn dispatch(state: &Arc<DaemonState>, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Status => Response::Status(state.status()),
        Request::Shutdown => {
            state.request_shutdown();
            Response::Ok
        }
        Request::Mount { .. } | Request::Umount { .. } => Response::Err {
            code: "not_implemented".into(),
            message: "Mount / Umount land in Stage 2b — see \
                      docs/implementation/projgitd-plan.md §2"
                .into(),
        },
    }
}

// -----------------------------------------------------------------------------
// Tests — unit tests on `dispatch` only. The end-to-end socket roundtrip
// lives in `tests/server_smoke.rs` because it needs a real listener.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_dispatches_to_pong() {
        let state = Arc::new(DaemonState::new(PathBuf::from("/tmp/_test_unused.sock")));
        match dispatch(&state, Request::Ping) {
            Response::Pong => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn status_reports_uptime_and_empty_state() {
        let state = Arc::new(DaemonState::new(PathBuf::from("/tmp/_test_unused.sock")));
        match dispatch(&state, Request::Status) {
            Response::Status(r) => {
                assert!(r.mounts.is_empty());
                assert!(r.source.is_none());
                assert!(r.cache.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn shutdown_sets_flag_and_returns_ok() {
        let state = Arc::new(DaemonState::new(PathBuf::from("/tmp/_test_unused.sock")));
        assert!(!state.should_shut_down());
        match dispatch(&state, Request::Shutdown) {
            Response::Ok => {}
            other => panic!("got {other:?}"),
        }
        assert!(state.should_shut_down());
    }

    #[test]
    fn mount_returns_not_implemented_stub() {
        let state = Arc::new(DaemonState::new(PathBuf::from("/tmp/_test_unused.sock")));
        let req = Request::Mount {
            source: "x".into(),
            ref_name: "main".into(),
            mountpoint: PathBuf::from("/tmp/x"),
            no_dotgit: false,
            allow_other: false,
        };
        match dispatch(&state, req) {
            Response::Err { code, .. } => assert_eq!(code, "not_implemented"),
            other => panic!("got {other:?}"),
        }
    }
}
