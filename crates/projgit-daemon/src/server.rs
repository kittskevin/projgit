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
    codes, read_message, write_message, CacheStats, FrameError, HeaderProbeWire, MountInfo,
    Request, Response, StatusReport,
};
use anyhow::{anyhow, Context, Result};
use gix::ObjectId;
use projgit_core::{
    clone::{git_dir_for, partial_clone, CloneOptions},
    GitCliFetcher, HeaderProbe, HydratingObjectStore, NoopFetcher, ObjectKind, ObjectStore,
    Projection, ProjectionFsProvider, RootOverlay,
};
use projgit_fuse::{mount_background, BackgroundSession, MountConfig, SessionACL};
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Where to keep partial clones of URL sources. `None` means
    /// "the user defaults" (XDG_CACHE_HOME etc.). Used only the first
    /// time a URL source is mounted.
    pub cache_dir: Option<PathBuf>,
    /// If `Some(n)`, pass `--depth=n` to `git clone` when partial-
    /// cloning a URL source. `Some(1)` is the load-bearing case:
    /// shallow partial clone for big-history repos where the
    /// metadata payload of a full partial clone is itself multi-GB.
    ///
    /// Tradeoff: shallow clones serve `cat` / `ls` of the current
    /// snapshot fine, but `git log` / `git blame` /
    /// `git diff <older>` inside a projection built on top won't
    /// work — there's no history to walk. Right choice for
    /// Harbor-style eval/build agents; wrong for history-walking
    /// workloads. Defaults to `None` (full history).
    pub cache_depth: Option<u32>,
    /// If `true`, emit one structured trace line on stderr per
    /// RPC, capturing per-RPC wall time and the in-flight RPC
    /// count at request-receive time. Used to diagnose
    /// data-plane bottlenecks under load (the rust-lang/rust
    /// scale failure: see
    /// [`docs/implementation/data-plane-investigation-plan.md`]).
    ///
    /// Format (greppable, one line per RPC):
    /// `trace: rpc=<name> served_us=<n> inflight_at_recv=<n>
    /// [oid=<short>] [code=<err>]`
    ///
    /// Off by default — instrumentation is in the hot path so
    /// even a cheap "if trace { ... }" check is worth gating.
    pub trace: bool,
    /// Number of `git cat-file --batch-check` children the
    /// daemon's [`GitCliFetcher`] keeps in its `BatchChildPool`.
    /// `1` matches the pre-pool behaviour (single shared child,
    /// head-of-line blocked under sidecar fan-out). Defaults to
    /// [`GitCliFetcher::default_pool_size`]
    /// (`min(available_parallelism, 8)`).
    ///
    /// Used by [`attach_source`] when constructing the
    /// `GitCliFetcher` for a URL source. Per-source: the value at
    /// first Attach binds for the daemon's lifetime; changing
    /// `--pool-size` requires a daemon restart.
    pub pool_size: usize,
    /// If `Some(path)`, write the daemon's PID to `path` once the
    /// control socket is bound (so the file's presence also serves
    /// as a readiness marker) and remove it on graceful shutdown.
    /// A `SIGKILL` leaves it stale (the usual PID-file caveat).
    /// `None` (default) writes no PID file; socket-bind already
    /// guards against a second instance, so this is opt-in for
    /// supervisors that want a PID handle.
    pub pid_file: Option<PathBuf>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            socket_mode: 0o600,
            cache_dir: None,
            cache_depth: None,
            trace: false,
            pool_size: GitCliFetcher::default_pool_size(),
            pid_file: None,
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
    socket_path: PathBuf,
    cache_dir: Option<PathBuf>,
    /// `Some(n)` applies `--depth=n` to URL partial clones.
    /// Propagated from [`DaemonConfig::cache_depth`].
    cache_depth: Option<u32>,
    /// Per-RPC trace emission flag. Propagated from
    /// [`DaemonConfig::trace`]. Sampled in `handle_connection`.
    trace: bool,
    /// `BatchChildPool` size for [`GitCliFetcher`]. Propagated
    /// from [`DaemonConfig::pool_size`]; used by [`attach_source`]
    /// when constructing the per-URL-source fetcher.
    pool_size: usize,
    /// In-flight RPC count. Incremented at request receive,
    /// decremented after response. Used by the trace path to
    /// surface pile-ups under load (`inflight_at_recv=N` in the
    /// trace line).
    inflight: AtomicUsize,
    shutdown_requested: AtomicBool,
    next_projection_id: AtomicU64,
    active: Mutex<Option<ActiveRepo>>,
}

/// State once the daemon has been bound to a source by its first
/// `Mount` request.
struct ActiveRepo {
    source: String,
    git_dir: PathBuf,
    store: Arc<ObjectStore>,
    backend: ActiveBackend,
    mounts: HashMap<PathBuf, MountEntry>,
}

/// Variant per concrete `Fetcher` type. `ProjectionFsProvider<F>` is
/// generic over `F`, so we dispatch on the variant when building a
/// provider instead of trying to type-erase Fetcher itself.
///
/// `Clone` is cheap: both variants wrap an `Arc`, so cloning bumps a
/// refcount. Used by [`handle_fetch`] and [`handle_prefetch_headers`]
/// to release `state.active` before calling the (slow) backend, so
/// data-plane RPCs across N sidecars run concurrently instead of
/// serialising through one mutex. (Pre-fix, cattrace measurement on
/// 2026-06-04 caught the state.active mutex completely masking the
/// cat-file pool: K children couldn't help when only one RPC could
/// be inside `repo.backend.*` at a time.)
#[derive(Clone)]
enum ActiveBackend {
    Noop(Arc<HydratingObjectStore<NoopFetcher>>),
    GitCli(Arc<HydratingObjectStore<GitCliFetcher>>),
}

impl ActiveBackend {
    /// Make `oid` resident on disk through the underlying
    /// `HydratingObjectStore`. Uses `header()` (not `read_blob`)
    /// because the request may name an object of any kind — trees,
    /// commits, and tags are all things the sidecar will need to
    /// hydrate, and `read_blob` rejects non-blobs with
    /// `UnexpectedKind`. `header()` is also cheap: warm hits skip
    /// the fetcher entirely; cold hits go through the fetcher's
    /// internal coalescer, which is the single-flight that closes
    /// audit A3 (N sidecars asking for the same OID concurrently
    /// see one upstream fetch).
    ///
    /// We discard the header; the daemon doesn't ship metadata
    /// over the wire on this op (the data plane is the shared
    /// on-disk CAS — see `docs/design/projgitd.md` §4.2). The fact
    /// that `header()` succeeded is the signal the sidecar needs.
    fn fetch_one(&self, oid: ObjectId) -> Result<(), String> {
        match self {
            ActiveBackend::Noop(h) => h.header(oid).map(|_| ()).map_err(|e| e.to_string()),
            ActiveBackend::GitCli(h) => h.header(oid).map(|_| ()).map_err(|e| e.to_string()),
        }
    }

    fn prefetch_headers(&self, oids: &[ObjectId]) -> Vec<HeaderProbe> {
        match self {
            ActiveBackend::Noop(h) => h.prefetch_headers(oids),
            ActiveBackend::GitCli(h) => h.prefetch_headers(oids),
        }
    }
}

struct MountEntry {
    ref_name: String,
    projection_id: u64,
    #[allow(dead_code)] // held for its Drop side effect: unmounts via fuser.
    /// Dropping this unmounts via fuser's Drop impl.
    session: BackgroundSession,
}

impl DaemonState {
    /// Backward-compatible constructor for tests that don't care
    /// about `cache_depth`. Delegates to [`Self::with_depth`].
    #[allow(dead_code)]
    fn new(socket_path: PathBuf, cache_dir: Option<PathBuf>) -> Self {
        Self::with_depth(socket_path, cache_dir, None)
    }

    fn with_depth(
        socket_path: PathBuf,
        cache_dir: Option<PathBuf>,
        cache_depth: Option<u32>,
    ) -> Self {
        // Test-only path; pin pool_size to 1 so behaviour matches
        // the pre-pool single-child code path tests historically
        // exercised. Production constructs DaemonState via
        // [`Self::with_full_config`] from `run()`, which takes
        // `pool_size` from `DaemonConfig`.
        Self::with_full_config(socket_path, cache_dir, cache_depth, false, 1)
    }

    fn with_full_config(
        socket_path: PathBuf,
        cache_dir: Option<PathBuf>,
        cache_depth: Option<u32>,
        trace: bool,
        pool_size: usize,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            socket_path,
            cache_dir,
            cache_depth,
            trace,
            pool_size,
            inflight: AtomicUsize::new(0),
            shutdown_requested: AtomicBool::new(false),
            next_projection_id: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    fn status(&self) -> StatusReport {
        let active = self.active.lock().unwrap();
        let (source, mounts, cache) = match &*active {
            None => (None, Vec::new(), None),
            Some(repo) => {
                let mut mounts: Vec<MountInfo> = repo
                    .mounts
                    .iter()
                    .map(|(mp, m)| MountInfo {
                        ref_name: m.ref_name.clone(),
                        mountpoint: mp.clone(),
                        projection_id: m.projection_id,
                    })
                    .collect();
                mounts.sort_by_key(|m| m.projection_id);
                let t = repo.store.tree_cache_stats();
                let h = repo.store.header_cache_stats();
                let b = repo.store.blob_cache_stats();
                let cache = CacheStats {
                    tree_hits: t.hits,
                    tree_misses: t.misses,
                    header_hits: h.hits,
                    header_misses: h.misses,
                    blob_hits: b.hits,
                    blob_misses: b.misses,
                };
                (Some(repo.source.clone()), mounts, Some(cache))
            }
        };
        StatusReport {
            uptime_secs: self.started_at.elapsed().as_secs(),
            source,
            mounts,
            cache,
        }
    }

    fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path);
    }

    fn should_shut_down(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }

    fn next_projection_id(&self) -> u64 {
        self.next_projection_id.fetch_add(1, Ordering::SeqCst)
    }
}

/// Run the daemon. Blocks until shutdown.
///
/// Returns `Ok(())` on graceful shutdown (via `Request::Shutdown` over
/// the socket), `Err(_)` if the listener can't be bound. Signal
/// handling is **not** installed here — it's a process-wide resource
/// (`ctrlc` can only be set once per process) and belongs in the
/// binary's `main()`. Tests that drive `run` directly trigger shutdown
/// by sending [`Request::Shutdown`] over the socket; the daemon binary
/// (`src/main.rs`) wires SIGINT/SIGTERM to the same effect by
/// self-connecting and sending `Shutdown`.
pub fn run(config: DaemonConfig) -> Result<()> {
    // Best-effort cleanup of a stale socket file. If the file exists
    // and a daemon is actively listening on it, this still won't
    // surface a clear conflict — bind() below will fail with EADDRINUSE.
    let _ = std::fs::remove_file(&config.socket_path);

    let listener = UnixListener::bind(&config.socket_path)
        .with_context(|| format!("binding unix socket at {}", config.socket_path.display()))?;
    nix::sys::stat::fchmodat(
        None,
        &config.socket_path,
        nix::sys::stat::Mode::from_bits_truncate(config.socket_mode as nix::libc::mode_t),
        nix::sys::stat::FchmodatFlags::FollowSymlink,
    )
    .with_context(|| format!("fchmod {:o} on socket", config.socket_mode))?;

    tracing::info!(
        "listening on {} (socket mode {:o})",
        config.socket_path.display(),
        config.socket_mode
    );

    // Write the PID file once bound + listening, so its presence is
    // also a readiness marker. A write failure is a startup config
    // error: surface it loudly and don't leave a half-initialised
    // daemon (socket bound, no pid file) behind.
    if let Some(pid_path) = &config.pid_file {
        if let Err(e) = std::fs::write(pid_path, format!("{}\n", std::process::id())) {
            let _ = std::fs::remove_file(&config.socket_path);
            return Err(e)
                .with_context(|| format!("writing pid file {}", pid_path.display()));
        }
        tracing::info!("wrote pid file {}", pid_path.display());
    }

    let state = Arc::new(DaemonState::with_full_config(
        config.socket_path.clone(),
        config.cache_dir.clone(),
        config.cache_depth,
        config.trace,
        config.pool_size,
    ));

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
                        tracing::warn!("connection error: {e:#}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("accept error: {e}");
                // Don't kill the daemon on a transient accept error.
                continue;
            }
        }
    }

    tracing::info!("shutting down");
    // Drop the listener first so no new connections succeed.
    drop(listener);
    // Remove the socket file so a restart doesn't see a stale path.
    let _ = std::fs::remove_file(&config.socket_path);
    // Remove the PID file (if any) so its presence stays a truthful
    // liveness marker. A SIGKILL skips this and leaves it stale.
    if let Some(pid_path) = &config.pid_file {
        let _ = std::fs::remove_file(pid_path);
    }
    // State drops here. Stage 2b's mount sessions will unmount on this.
    drop(state);
    tracing::info!("shutdown complete");
    Ok(())
}

/// Per-connection handler. V1 protocol is one request → one response →
/// close, so we read once, dispatch, write once, and let the stream
/// drop. When `state.trace` is set, emits one trace line per RPC
/// on stderr capturing wall time and in-flight count at receive.
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

    // Trace bookkeeping: increment in-flight before dispatch,
    // decrement after response. Sample in-flight at receive so the
    // emitted line records "how loaded was the daemon when this
    // request arrived" (the pile-up signal).
    let trace = state.trace;
    let inflight_at_recv = if trace {
        state.inflight.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        0
    };
    let rpc_label = request_label(&request);
    let rpc_extra = request_extra(&request);
    let start = if trace { Some(Instant::now()) } else { None };

    let response = dispatch(&state, request);

    if let (true, Some(t0)) = (trace, start) {
        let served_us = t0.elapsed().as_micros();
        let code = response_error_code(&response);
        state.inflight.fetch_sub(1, Ordering::Relaxed);
        let code_part = code.map(|c| format!(" code={c}")).unwrap_or_default();
        let extra_part = if rpc_extra.is_empty() {
            String::new()
        } else {
            format!(" {rpc_extra}")
        };
        eprintln!(
            "trace: rpc={rpc_label} served_us={served_us} inflight_at_recv={inflight_at_recv}{extra_part}{code_part}",
        );
    }

    if let Err(e) = write_message(&mut stream, &response) {
        // We've already computed the response; if the wire write
        // fails (peer hung up, etc.), there's nothing useful to send.
        tracing::warn!("response write failed: {e}");
    }
    Ok(())
}

/// Short label for a request variant. Used as the `rpc=` value
/// in trace output.
fn request_label(req: &Request) -> &'static str {
    match req {
        Request::Ping => "Ping",
        Request::Status => "Status",
        Request::Shutdown => "Shutdown",
        Request::Mount { .. } => "Mount",
        Request::Umount { .. } => "Umount",
        Request::Attach { .. } => "Attach",
        Request::Fetch { .. } => "Fetch",
        Request::PrefetchHeaders { .. } => "PrefetchHeaders",
    }
}

/// Per-RPC extra trace fields (OID for Fetch, batch size for
/// PrefetchHeaders, etc.). Empty string when nothing to add.
fn request_extra(req: &Request) -> String {
    match req {
        Request::Fetch { oid } => {
            let short = oid.get(..8).unwrap_or(oid);
            format!("oid={short}")
        }
        Request::PrefetchHeaders { oids } => format!("n_oids={}", oids.len()),
        Request::Mount { mountpoint, .. } => format!("mp={}", mountpoint.display()),
        _ => String::new(),
    }
}

/// Surface the error code for trace output when the dispatch
/// produced [`Response::Err`]. Returns `None` for successful
/// responses.
fn response_error_code(r: &Response) -> Option<&str> {
    match r {
        Response::Err { code, .. } => Some(code.as_str()),
        _ => None,
    }
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
        Request::Mount {
            source,
            ref_name,
            mountpoint,
            no_dotgit,
            allow_other,
        } => handle_mount(state, source, ref_name, mountpoint, no_dotgit, allow_other),
        Request::Umount { mountpoint } => handle_umount(state, mountpoint),
        Request::Attach { source } => handle_attach(state, source),
        Request::Fetch { oid } => handle_fetch(state, oid),
        Request::PrefetchHeaders { oids } => handle_prefetch_headers(state, oids),
    }
}

/// Mount handler. Acquires the state mutex once for the whole
/// operation; not ideal for concurrency but trivially correct, and
/// mount/umount are not hot paths.
fn handle_mount(
    state: &Arc<DaemonState>,
    source: String,
    ref_name: String,
    mountpoint: PathBuf,
    no_dotgit: bool,
    allow_other: bool,
) -> Response {
    let mountpoint = match std::fs::canonicalize(&mountpoint) {
        Ok(p) => p,
        Err(e) => {
            return err(
                codes::MOUNT_FAILED,
                format!("canonicalize mountpoint {}: {e}", mountpoint.display()),
            )
        }
    };

    let mut active = match state.active.lock() {
        Ok(g) => g,
        Err(_) => return err(codes::INTERNAL, "daemon state mutex poisoned (prior panic)"),
    };

    // First mount: attach the daemon to this source.
    if active.is_none() {
        match attach_source(
            &source,
            state.cache_dir.as_deref(),
            state.cache_depth,
            state.pool_size,
        ) {
            Ok(repo) => *active = Some(repo),
            Err(e) => return err(codes::SOURCE_OPEN_FAILED, format!("{e:#}")),
        }
    }
    let repo = active.as_mut().expect("just attached");
    if repo.source != source {
        return err(
            codes::SOURCE_MISMATCH,
            format!(
                "daemon is bound to source `{}`; this Mount asked for `{source}`.                  V1 is one source per daemon.",
                repo.source
            ),
        );
    }
    if repo.mounts.contains_key(&mountpoint) {
        return err(
            codes::MOUNTPOINT_BUSY,
            format!("{} already mounted by this daemon", mountpoint.display()),
        );
    }

    let projection = Projection::Ref(ref_name.clone());
    let overlay = match build_overlay(no_dotgit, &projection, &repo.store, &repo.git_dir) {
        Ok(o) => o,
        Err(e) => return err(codes::REF_RESOLVE_FAILED, format!("{e:#}")),
    };

    let projection_id = state.next_projection_id();
    let mut cfg = MountConfig::default();
    if allow_other {
        cfg.acl = SessionACL::All;
    }

    let session_result = match &repo.backend {
        ActiveBackend::Noop(h) => {
            ProjectionFsProvider::new(projection, h.clone(), overlay, projection_id)
                .map_err(anyhow::Error::from)
                .and_then(|p| {
                    mount_background(Arc::new(p), &mountpoint, &cfg).map_err(anyhow::Error::from)
                })
        }
        ActiveBackend::GitCli(h) => {
            ProjectionFsProvider::new(projection, h.clone(), overlay, projection_id)
                .map_err(anyhow::Error::from)
                .and_then(|p| {
                    mount_background(Arc::new(p), &mountpoint, &cfg).map_err(anyhow::Error::from)
                })
        }
    };

    match session_result {
        Ok(session) => {
            repo.mounts.insert(
                mountpoint,
                MountEntry {
                    ref_name,
                    projection_id,
                    session,
                },
            );
            Response::Ok
        }
        Err(e) => err(codes::MOUNT_FAILED, format!("{e:#}")),
    }
}

fn handle_umount(state: &Arc<DaemonState>, mountpoint: PathBuf) -> Response {
    let mountpoint = match std::fs::canonicalize(&mountpoint) {
        Ok(p) => p,
        Err(e) => {
            return err(
                codes::NO_SUCH_MOUNT,
                format!("canonicalize mountpoint {}: {e}", mountpoint.display()),
            )
        }
    };
    let mut active = match state.active.lock() {
        Ok(g) => g,
        Err(_) => return err(codes::INTERNAL, "daemon state mutex poisoned"),
    };
    let Some(repo) = active.as_mut() else {
        return err(codes::NO_SUCH_MOUNT, "daemon has no active source yet");
    };
    match repo.mounts.remove(&mountpoint) {
        Some(_entry) => Response::Ok,
        None => err(
            codes::NO_SUCH_MOUNT,
            format!("no mount registered at {}", mountpoint.display()),
        ),
    }
}

// -----------------------------------------------------------------------------
// Stage 3 — sidecar-mode handlers (Attach / Fetch / PrefetchHeaders)
// -----------------------------------------------------------------------------

/// Bind the daemon to `source` (clone if needed) and return the
/// on-disk git-dir path. Idempotent: a second `Attach` to the same
/// source returns the existing git_dir; a second `Attach` to a
/// different source returns `source_mismatch`. Mirrors the
/// first-`Mount`-wins behaviour for V1's one-source-per-daemon rule.
fn handle_attach(state: &Arc<DaemonState>, source: String) -> Response {
    let mut active = match state.active.lock() {
        Ok(g) => g,
        Err(_) => return err(codes::INTERNAL, "daemon state mutex poisoned"),
    };
    if active.is_none() {
        match attach_source(
            &source,
            state.cache_dir.as_deref(),
            state.cache_depth,
            state.pool_size,
        ) {
            Ok(repo) => *active = Some(repo),
            Err(e) => return err(codes::SOURCE_OPEN_FAILED, format!("{e:#}")),
        }
    }
    let repo = active.as_ref().expect("just attached");
    if repo.source != source {
        return err(
            codes::SOURCE_MISMATCH,
            format!(
                "daemon is bound to source `{}`; this Attach asked for `{source}`. \
                 V1 is one source per daemon.",
                repo.source
            ),
        );
    }
    Response::Attached {
        git_dir: repo.git_dir.clone(),
    }
}

/// Make `oid` resident on disk. Returns once the daemon's
/// `HydratingObjectStore` reports success (the coalescer deduplicates
/// concurrent requests for the same OID — closes audit A3). The
/// sidecar's `ObjectStore::contains(oid)` succeeds immediately after.
fn handle_fetch(state: &Arc<DaemonState>, oid_hex: String) -> Response {
    let oid = match parse_oid(&oid_hex) {
        Ok(o) => o,
        Err(e) => return err(codes::BAD_OID, e),
    };
    // Clone the Arc-wrapped backend out of the critical section so
    // the (slow) `fetch_one` call runs without holding state.active.
    // Concurrent Fetch / PrefetchHeaders RPCs across N sidecars
    // can then proceed in parallel and actually exercise the
    // cat-file pool's K slots. See ActiveBackend's Clone derive.
    let backend = {
        let active = match state.active.lock() {
            Ok(g) => g,
            Err(_) => return err(codes::INTERNAL, "daemon state mutex poisoned"),
        };
        let Some(repo) = active.as_ref() else {
            return err(
                codes::NOT_ATTACHED,
                "daemon has no active source yet; send Attach first",
            );
        };
        repo.backend.clone()
    };
    match backend.fetch_one(oid) {
        Ok(()) => Response::Ok,
        Err(e) => err(codes::FETCH_FAILED, e),
    }
}

/// Batch variant: resolve headers for many OIDs in one round trip.
fn handle_prefetch_headers(state: &Arc<DaemonState>, oid_hexes: Vec<String>) -> Response {
    // Parse all OIDs up front; any malformed one rejects the whole
    // batch with `bad_oid` so the client can fix its caller.
    let mut oids: Vec<ObjectId> = Vec::with_capacity(oid_hexes.len());
    for hex in &oid_hexes {
        match parse_oid(hex) {
            Ok(o) => oids.push(o),
            Err(e) => return err(codes::BAD_OID, e),
        }
    }
    // Same pattern as handle_fetch: clone backend out, drop the
    // lock, then call. Without this, two sidecars firing concurrent
    // PrefetchHeaders RPCs would serialise at state.active and the
    // cat-file pool's K slots would sit idle.
    let backend = {
        let active = match state.active.lock() {
            Ok(g) => g,
            Err(_) => return err(codes::INTERNAL, "daemon state mutex poisoned"),
        };
        let Some(repo) = active.as_ref() else {
            return err(
                codes::NOT_ATTACHED,
                "daemon has no active source yet; send Attach first",
            );
        };
        repo.backend.clone()
    };
    let probes = backend.prefetch_headers(&oids);
    Response::HeaderProbes {
        probes: probes.into_iter().map(probe_to_wire).collect(),
    }
}

fn parse_oid(hex: &str) -> Result<ObjectId, String> {
    ObjectId::from_hex(hex.as_bytes()).map_err(|e| format!("invalid OID `{hex}`: {e}"))
}

fn object_kind_str(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

fn probe_to_wire(probe: HeaderProbe) -> HeaderProbeWire {
    match probe {
        HeaderProbe::Present(oid) => HeaderProbeWire::Present {
            oid: oid.to_string(),
        },
        HeaderProbe::PresentWithHeader(oid, kind, size) => HeaderProbeWire::PresentWithHeader {
            oid: oid.to_string(),
            object_kind: object_kind_str(kind).to_owned(),
            size,
        },
        HeaderProbe::HeaderOnly(oid, kind, size) => HeaderProbeWire::HeaderOnly {
            oid: oid.to_string(),
            object_kind: object_kind_str(kind).to_owned(),
            size,
        },
        HeaderProbe::Error(oid, e) => HeaderProbeWire::Error {
            oid: oid.to_string(),
            code: codes::FETCH_FAILED.into(),
            message: format!("{e}"),
        },
    }
}

fn err(code: &str, message: impl Into<String>) -> Response {
    Response::Err {
        code: code.into(),
        message: message.into(),
    }
}

// -----------------------------------------------------------------------------
// Source attachment + overlay helpers
// -----------------------------------------------------------------------------

fn looks_like_url(s: &str) -> bool {
    s.contains("://") || s.starts_with("git@")
}

/// Open the source repo (URL -> partial clone into cache; local path
/// -> open in place), choose the matching backend, and return a
/// freshly-constructed `ActiveRepo`.
fn attach_source(
    source: &str,
    cache_dir_override: Option<&Path>,
    cache_depth: Option<u32>,
    pool_size: usize,
) -> Result<ActiveRepo> {
    if looks_like_url(source) {
        let cache_dir = resolve_cache_dir(cache_dir_override)?;
        let dest = cache_dir.join(cache_subdir_for_url(source));
        if !dest.exists() {
            let depth_note = match cache_depth {
                Some(n) => format!(" (--depth={n})"),
                None => String::new(),
            };
            tracing::info!(
                "partial-cloning {} into {}{}",
                source,
                dest.display(),
                depth_note,
            );
            std::fs::create_dir_all(&cache_dir)
                .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
            let mut opts = CloneOptions::new(source.to_owned(), dest.clone());
            if let Some(n) = cache_depth {
                opts = opts.with_depth(n);
            }
            partial_clone(&opts).with_context(|| format!("cloning {source}"))?;
        } else {
            tracing::info!("reusing cached clone at {}", dest.display());
        }
        let git_dir = git_dir_for(&dest);
        let store = Arc::new(
            ObjectStore::open(&git_dir)
                .with_context(|| format!("opening object store at {}", git_dir.display()))?,
        );
        let fetcher = GitCliFetcher::open_with_pool_size(store.clone(), pool_size)
            .context("opening GitCliFetcher (needs `git` on PATH)")?;
        let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), fetcher));
        Ok(ActiveRepo {
            source: source.to_owned(),
            git_dir,
            store,
            backend: ActiveBackend::GitCli(hydrating),
            mounts: HashMap::new(),
        })
    } else {
        let path = PathBuf::from(source);
        if !path.exists() {
            return Err(anyhow!("source path {} does not exist", path.display()));
        }
        let git_dir = git_dir_for(&path);
        let store = Arc::new(
            ObjectStore::open(&git_dir)
                .with_context(|| format!("opening object store at {}", git_dir.display()))?,
        );
        let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), NoopFetcher));
        Ok(ActiveRepo {
            source: source.to_owned(),
            git_dir,
            store,
            backend: ActiveBackend::Noop(hydrating),
            mounts: HashMap::new(),
        })
    }
}

fn resolve_cache_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(xdg).join("projgit"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME not set; pass --cache-dir explicitly"))?;
    Ok(PathBuf::from(home).join(".cache").join("projgit"))
}

/// Stable filesystem-safe subdir name for a URL. Mirrors the helper
/// in `projgit-cli/src/main.rs`; duplicated because lifting the
/// CLI helper would mean a new shared crate or making the CLI a
/// library too.
fn cache_subdir_for_url(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let canonical = canonicalize_url_for_hash(url);
    let mut h = DefaultHasher::new();
    canonical.hash(&mut h);
    let hash = h.finish();
    let basename = url
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("repo")
        .trim_end_matches(".git");
    format!("{basename}-{hash:016x}")
}

fn canonicalize_url_for_hash(url: &str) -> String {
    let mut s = url.trim().to_ascii_lowercase();
    while s.ends_with('/') {
        s.pop();
    }
    if s.ends_with(".git") {
        s.truncate(s.len() - 4);
    }
    s
}

fn build_overlay(
    no_dotgit: bool,
    projection: &Projection,
    store: &Arc<ObjectStore>,
    git_dir: &Path,
) -> Result<RootOverlay> {
    use projgit_core::dotgit;

    if no_dotgit {
        return Ok(RootOverlay::new());
    }
    if matches!(projection, Projection::Subtree { .. }) {
        return Ok(RootOverlay::new());
    }
    let commit_oid = projection
        .resolve_commit(store)
        .with_context(|| "resolving projection commit for .git/ synthesis")?;
    let objects_dir = git_dir.join("objects");
    let objects_dir = std::fs::canonicalize(&objects_dir)
        .with_context(|| format!("canonicalizing {}", objects_dir.display()))?;
    let mut overlay = dotgit::a1_plus_overlay(store, commit_oid, &objects_dir)
        .context("building A1+ overlay")?;
    // Apply A2 ref visibility when the projection is a local branch.
    // No-op for Commit projections and for refs that resolve to tags
    // (those correctly stay on A1's detached HEAD; see
    // docs/design/dotgit-synthesis.md §4.1 row A2).
    if let Projection::Ref(name) = projection {
        if let Some(full) = store.try_resolve_branch_full_name(name) {
            dotgit::apply_a2_ref_visibility(&mut overlay, &full, commit_oid);
        }
    }
    Ok(overlay)
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
        let state = Arc::new(DaemonState::new(
            PathBuf::from("/tmp/_test_unused.sock"),
            None,
        ));
        match dispatch(&state, Request::Ping) {
            Response::Pong => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn status_reports_uptime_and_empty_state() {
        let state = Arc::new(DaemonState::new(
            PathBuf::from("/tmp/_test_unused.sock"),
            None,
        ));
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
        let state = Arc::new(DaemonState::new(
            PathBuf::from("/tmp/_test_unused.sock"),
            None,
        ));
        assert!(!state.should_shut_down());
        match dispatch(&state, Request::Shutdown) {
            Response::Ok => {}
            other => panic!("got {other:?}"),
        }
        assert!(state.should_shut_down());
    }
}
