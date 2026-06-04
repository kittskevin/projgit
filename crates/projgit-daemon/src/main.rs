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
    }

    let cli = Cli::parse();

    let socket_mode = u32::from_str_radix(cli.socket_mode.trim_start_matches('0'), 8)
        .map_err(|e| anyhow::anyhow!("--socket-mode must be octal, got `{}`: {e}", cli.socket_mode))?;

    if matches!(cli.depth, Some(0)) {
        anyhow::bail!("--depth 0 is not supported; git rejects --depth=0");
    }

    let mut config = DaemonConfig::default();
    if let Some(p) = cli.socket {
        config.socket_path = p;
    }
    config.socket_mode = socket_mode;
    config.cache_depth = cli.depth;

    // Signal handling lives in the binary, not the library, because
    // `ctrlc::set_handler` is a process-wide resource (set-once) and
    // would prevent tests from running multiple daemon instances in
    // one process.
    install_signal_handler(config.socket_path.clone())?;

    run(config)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install_signal_handler(socket_path: std::path::PathBuf) -> anyhow::Result<()> {
    use projgit_daemon::protocol::{write_message, Request};
    use std::os::unix::net::UnixStream;

    ctrlc::set_handler(move || {
        eprintln!("projgitd: shutdown signal received; forwarding via socket");
        // Self-connect and send Shutdown — the running accept loop
        // picks it up like any other client. Keeps the in-library
        // shutdown path unique.
        match UnixStream::connect(&socket_path) {
            Ok(mut s) => {
                let _ = write_message(&mut s, &Request::Shutdown);
            }
            Err(e) => {
                eprintln!("projgitd: signal-handler self-connect failed: {e}");
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
