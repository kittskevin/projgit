//! `projgitd` binary — entry point for the daemon. Argument parsing
//! and platform dispatch only; the real work is in
//! [`projgit_daemon::server::run`].

#![forbid(unsafe_code)]

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    use clap::Parser;
    use projgit_daemon::server::{run, DaemonConfig};
    use std::path::PathBuf;

    #[derive(Debug, clap::Parser)]
    #[command(
        name = "projgitd",
        version = projgit_core::VERSION,
        about = "Long-lived projgit daemon serving multi-projection FUSE mounts over a unix socket.",
    )]
    struct Cli {
        /// Unix socket path the daemon listens on. Defaults to
        /// `$XDG_RUNTIME_DIR/projgitd.sock` (fall back to
        /// `/tmp/projgitd-<uid>.sock`).
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,

        /// File mode for the socket (octal). Defaults to `0600`
        /// (owner only). Set to `0660` for a multi-user shared
        /// deployment and rely on `chgrp` of the socket file.
        #[arg(long, value_name = "MODE", default_value = "0600")]
        socket_mode: String,

        /// Pass `--depth=N` to `git clone` when partial-cloning a
        /// URL source. `--depth 1` is the load-bearing case:
        /// shallow partial clone for big-history repos where the
        /// metadata payload of a full partial clone is itself
        /// multi-GB. Daemon-wide setting; applies to every
        /// `Attach` for a URL source. Default: full history (no
        /// `--depth`).
        ///
        /// Tradeoff: shallow projections can serve `cat` / `ls`
        /// of the current snapshot fine, but `git log`,
        /// `git blame`, `git diff <older>`, and
        /// `git checkout <older>` won't work inside the
        /// projection — there's no history to walk. Right
        /// choice for eval/build agents that only need the
        /// current snapshot.
        #[arg(long, value_name = "N")]
        depth: Option<u32>,

        /// Emit one structured trace line per RPC on stderr.
        /// Format: `trace: rpc=<name> served_us=<n>
        /// inflight_at_recv=<n> [oid=<short>] [code=<err>]`.
        /// Used to diagnose data-plane bottlenecks under load
        /// (see docs/implementation/data-plane-investigation-plan.md).
        /// Off by default — instrumentation is in the hot path.
        #[arg(long)]
        trace: bool,

        /// Number of `git cat-file --batch-check` children the
        /// daemon's `GitCliFetcher` keeps in its pool. Round-robin
        /// dispatch removes the head-of-line block diagnosed on
        /// 2026-06-04 where one shared child serialised every
        /// sidecar's prefetch + on-demand fetch through a single
        /// mutex (see docs/bench/baseline.md §Diagnostic).
        ///
        /// Default: `min(available_parallelism, 8)`. Pick K close
        /// to your expected sidecar fan-out for best parallelism
        /// (the cat-file pool plan suggests K >= N_sidecars + 1).
        /// `N=0` is rejected at parse time — a zero-sized pool is
        /// not a useful configuration.
        #[arg(long, value_name = "N")]
        pool_size: Option<usize>,

        /// Increase log verbosity. Repeat for more: default `info`,
        /// `-v` `debug`, `-vv` `trace`. Overridable per-target via
        /// the `PROJGIT_LOG` (preferred) or `RUST_LOG` env var, e.g.
        /// `PROJGIT_LOG=projgit_daemon=debug`.
        #[arg(short, long, action = clap::ArgAction::Count)]
        verbose: u8,

        /// Write the daemon's PID to <PATH> once the control socket
        /// is bound (the file's presence doubles as a readiness
        /// marker), and remove it on graceful shutdown. A SIGKILL
        /// leaves it stale. Optional — socket-bind already guards
        /// against a second instance; use this when a supervisor
        /// wants a PID handle.
        #[arg(long, value_name = "PATH")]
        pid_file: Option<PathBuf>,

        /// Directory for the partial clones of URL sources. Required
        /// for a system service whose user has no `$HOME`; without it
        /// the daemon falls back to `$XDG_CACHE_HOME` then
        /// `$HOME/.cache/projgit`, and errors at first URL Attach if
        /// neither is set.
        #[arg(long, value_name = "DIR")]
        cache_dir: Option<PathBuf>,
    }

    let cli = Cli::parse();

    init_tracing(cli.verbose);

    let socket_mode = u32::from_str_radix(cli.socket_mode.trim_start_matches('0'), 8)
        .map_err(|e| anyhow::anyhow!("--socket-mode must be octal, got `{}`: {e}", cli.socket_mode))?;

    if matches!(cli.depth, Some(0)) {
        anyhow::bail!("--depth 0 is not supported; git rejects --depth=0");
    }

    if matches!(cli.pool_size, Some(0)) {
        anyhow::bail!("--pool-size 0 is not supported; pool must have at least one slot");
    }

    let mut config = DaemonConfig::default();
    if let Some(p) = cli.socket {
        config.socket_path = p;
    }
    config.socket_mode = socket_mode;
    config.cache_depth = cli.depth;
    config.trace = cli.trace;
    if let Some(n) = cli.pool_size {
        config.pool_size = n;
    }
    config.pid_file = cli.pid_file;
    if let Some(dir) = cli.cache_dir {
        config.cache_dir = Some(dir);
    }

    // Signal handling lives in the binary, not the library, because
    // `ctrlc::set_handler` is a process-wide resource (set-once) and
    // would prevent tests from running multiple daemon instances in
    // one process.
    install_signal_handler(config.socket_path.clone())?;

    run(config)
}

/// Initialise the global `tracing` subscriber for the daemon.
///
/// Level precedence: an explicit `PROJGIT_LOG` (or `RUST_LOG`) env
/// directive wins; otherwise the `-v` count picks the default
/// (`info` / `debug` / `trace`). Logs go to stderr so they land in
/// the journal under a systemd unit. Called once from `main`; the
/// library `run()` never installs a subscriber, so in-process tests
/// that drive the daemon stay quiet.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn init_tracing(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let default_level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = std::env::var("PROJGIT_LOG")
        .ok()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .map(EnvFilter::new)
        .unwrap_or_else(|| EnvFilter::new(default_level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install_signal_handler(socket_path: std::path::PathBuf) -> anyhow::Result<()> {
    use projgit_daemon::protocol::{write_message, Request};
    use std::os::unix::net::UnixStream;

    ctrlc::set_handler(move || {
        tracing::info!("shutdown signal received; forwarding via socket");
        // Self-connect and send Shutdown — the running accept loop
        // picks it up like any other client. Keeps the in-library
        // shutdown path unique.
        match UnixStream::connect(&socket_path) {
            Ok(mut s) => {
                let _ = write_message(&mut s, &Request::Shutdown);
            }
            Err(e) => {
                tracing::warn!("signal-handler self-connect failed: {e}");
            }
        }
    })
    .map_err(|e| anyhow::anyhow!("installing signal handler: {e}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!(
        "`projgitd` is only supported on Linux/macOS today. Windows support \
         depends on the deferred projgit-winfsp backend (Phase 3d)."
    );
}
