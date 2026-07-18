//! `projgit` command-line entry point.
//!
//! Ships one subcommand today, `mount`, which wires the projection
//! engine (`projgit-core`) and the FUSE adapter (`projgit-fuse`) into a
//! single end-to-end demo: clone the upstream into a shared on-disk cache,
//! open it as an `ObjectStore`, build a `Projection` (ref / commit /
//! subtree), wrap it in a `ProjectionFsProvider` backed by the chosen
//! `Fetcher`, then mount it through FUSE until the user sends Ctrl-C.
//!
//! ```text
//! projgit mount https://github.com/foo/bar /mnt/bar          # clone + mount HEAD
//! projgit mount /path/to/repo /mnt/bar --commit <oid>        # local repo, specific commit
//! projgit mount https://… /mnt/bar --ref refs/tags/v1 \
//!                                  --subtree src --stats
//! ```
//!
//! Notable flags: `--offline` (no fetcher; any miss is an I/O error),
//! `--fetcher` (pick a fetcher backend; `git` is the default and only
//! always-on choice), `--cache-dir` (where URL sources are partial-cloned),
//! `--stats` (print cache and prefetch counters on unmount).
//!
//! The CLI surface is deliberately minimal: anything beyond `mount` (an
//! `umount` companion with a PID-file flow, daemonized background mounts,
//! `tracing-subscriber` wiring for the existing `-v` flag) is tracked in
//! `docs/implementation/handoff.md` and deferred.

#![forbid(unsafe_code)]

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "projgit",
    version = projgit_core::VERSION,
    about = "Lazy-fetch git projections as read-only filesystem mounts.",
    long_about = None,
)]
struct Cli {
    /// Increase log verbosity. Repeat for more (`-v` info, `-vv` debug).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Mount a projection of a repository at a local path.
    Mount(MountArgs),
    /// Mount many projections of one repository in a single process,
    /// sharing the `ObjectStore`, `Fetcher`, and in-memory caches.
    /// Stage 1 of the `projgitd` plan
    /// (see `docs/implementation/projgitd-plan.md`).
    MountMulti(MountMultiArgs),
    /// Talk to a running `projgitd` (Stage 2c). Subcommands send
    /// individual control-plane RPCs over the daemon's unix socket.
    Attach(AttachArgs),
    /// Switch a live **writable** mount to another commit/branch without
    /// rewriting the worktree (checkout-under-live-mount). Updates the
    /// mount's git index + HEAD via plumbing; the running mount observes
    /// the HEAD change and re-projects the worktree (local edits survive).
    Checkout(CheckoutArgs),
    /// (internal) Notify a running writable mount that `HEAD` moved, over
    /// its control socket, so it re-projects. Invoked by the scratch git
    /// dir's `reference-transaction` hook after a stock git operation;
    /// not intended for direct use.
    #[command(name = "__swap-notify", hide = true)]
    SwapNotify,
    /// (internal) Answer git's `core.fsmonitor` query for a writable
    /// mount by streaming the mount's modified-path set from its control
    /// socket. Invoked by the scratch git dir's fsmonitor hook; not
    /// intended for direct use.
    #[command(name = "__fsmonitor", hide = true)]
    Fsmonitor {
        /// `<version> <token>` passed by git (ignored; we return the
        /// current token + cumulative set).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, clap::Args)]
struct CheckoutArgs {
    /// Branch, tag, or commit to switch to. Must be resolvable in the
    /// mount's repository (fetch first if it is a new remote branch).
    refname: String,

    /// Directory inside the writable mount to operate from (like
    /// `git -C`). Defaults to the current directory.
    #[arg(short = 'C', long = "dir")]
    dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct MountArgs {
    /// Source repository: a URL (https://, git://, ssh://, git@...) or
    /// a path to a local checkout / bare repository.
    source: String,

    /// Empty directory to mount on. Must already exist.
    mountpoint: PathBuf,

    /// Mount the tip of this ref (short or full name; `HEAD` works).
    /// Mutually exclusive with `--commit`. Defaults to `HEAD`.
    #[arg(long, conflicts_with = "commit")]
    r#ref: Option<String>,

    /// Mount this specific commit (full hex OID).
    #[arg(long)]
    commit: Option<String>,

    /// Mount only this `/`-separated subdirectory of the projection.
    /// Combines with `--ref` (peeled to a commit at construction)
    /// or `--commit`.
    #[arg(long)]
    subtree: Option<String>,

    /// Where to keep clones of remote repositories. Only used when
    /// `source` is a URL. Defaults to `$XDG_CACHE_HOME/projgit` (or
    /// `~/.cache/projgit`).
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Pass `--depth=N` to `git clone` when partial-cloning a URL
    /// source. `--depth 1` is the shallow case: pull only the
    /// current commit's metadata, no history. Orders of magnitude
    /// smaller on deep-history repos.
    ///
    /// Tradeoff: shallow projections can `cat` / `ls` the current
    /// snapshot fine, but `git log`, `git blame`,
    /// `git diff <older>`, and `git checkout <older>` won't work
    /// inside the projection — there's no history to walk. Right
    /// choice for eval/build agents that only need the current
    /// snapshot; wrong for history-walking workloads.
    ///
    /// Ignored for local-path sources (which don't clone) and for
    /// `--daemon-socket` sidecar mode (the daemon owns clone
    /// policy via its own `--depth` flag).
    #[arg(long, value_name = "N")]
    depth: Option<u32>,

    /// Remote name to fetch from (default `origin`). Ignored in
    /// `--offline` mode and for sources that have no remote.
    #[arg(long, default_value = "origin")]
    remote: String,

    /// Skip on-demand fetching. Any miss against the local store
    /// surfaces as an I/O error instead of triggering network traffic.
    #[arg(long)]
    offline: bool,

    /// On unmount, print a summary of in-process cache and prefetch
    /// counters. Useful when tuning cache sizes or sanity-checking
    /// that warm reads are hitting the cache.
    #[arg(long)]
    stats: bool,

    /// Don't synthesize a `.git/` directory at the mount root.
    ///
    /// By default the mount carries an A1-flavored `.git/` (detached
    /// HEAD pointing at the projection's commit, `config`, empty
    /// `refs/`, and an `objects/info/alternates` line pointing at the
    /// shared on-disk object store). That makes `git rev-parse HEAD`,
    /// `git log`, `git cat-file`, `cargo build`'s VCS detection, and
    /// `ripgrep`'s VCS-aware ignore handling all work as expected.
    /// `--no-dotgit` opts out and exposes only the projected tree.
    ///
    /// `.git/` synthesis is automatically skipped for `--subtree`
    /// projections, since `.git/HEAD` would point at the full
    /// commit's tree rather than the subtree the user is browsing.
    /// See `docs/design/dotgit-synthesis.md` for the design ladder.
    #[arg(long)]
    no_dotgit: bool,

    /// **Writable worktree** (Phase 2). Mount the projection read-WRITE:
    /// unmodified files stay virtual, but you can edit them, create new
    /// files, `git add`, and `git commit` — only touched files are
    /// materialized (EdenFS-style). projgit sets up a real, writable
    /// git dir on disk (sharing the mount's object store) and links the
    /// mount's `.git` to it, so stock git inside the mount works with no
    /// fork. The git dir persists under `<cache>/worktrees/`, so
    /// committed work is restored when you remount the same mountpoint.
    ///
    /// First-cut limitations: the upper is in-memory, so *uncommitted*
    /// edits are lost on unmount (commit or push to keep them);
    /// not combinable with `--subtree`, `--no-dotgit`, or
    /// `--daemon-socket`.
    #[arg(long)]
    writable: bool,

    /// Enable FSMonitor for a `--writable` mount: install a
    /// `core.fsmonitor` hook that streams the mount's modified-path set
    /// over its control socket, so `git status` skips scanning the
    /// virtual worktree. A win on large worktrees; requires `--writable`.
    #[arg(long, requires = "writable")]
    fsmonitor: bool,

    /// Pass the `allow_other` FUSE mount option, so users other than
    /// the one running `projgit mount` can read the mount.
    ///
    /// Required for the "projgit on the host, containers consume the
    /// mount via bind-mount" deployment topology (and the equivalent
    /// sidecar-container shape). Without this flag, even `root` gets
    /// `EACCES` on the mount because FUSE's default security check
    /// only allows the mounting UID.
    ///
    /// Non-root users additionally need `user_allow_other` enabled in
    /// `/etc/fuse.conf` on the host; without it, `fusermount` will
    /// refuse the mount entirely.
    ///
    /// **Security note:** with this on, any local user (or anyone
    /// with a bind-mount into a container) can read the projection.
    /// Right default for single-tenant agent-eval rigs where every
    /// consumer is "yours"; wrong for a multi-tenant host where
    /// projections may contain private code.
    #[arg(long)]
    allow_other: bool,

    /// Fetch backend for URL-backed mounts.
    #[arg(long, value_enum, default_value = "git")]
    fetcher: FetcherChoice,

    /// Base GVFS protocol URL, with or without a trailing `/gvfs`.
    /// Required when `--fetcher gvfs` is selected.
    #[cfg(feature = "gvfs-fetcher")]
    #[arg(long, value_name = "URL")]
    gvfs_url: Option<String>,

    /// **Sidecar mode** (Stage 3 of the projgitd plan, see
    /// `docs/design/projgitd.md` §8).
    ///
    /// Connect to a running `projgitd` over the unix socket at
    /// `<PATH>` and hydrate cold-path objects through it instead of
    /// spawning a local fetcher. The daemon owns the upstream
    /// connection, the partial-clone cache, and the in-flight
    /// fetch coalescer that dedupes concurrent reads of the same
    /// OID across N sidecars. This process still holds its own
    /// `/dev/fuse` fd and runs the FUSE protocol loop locally, so
    /// a daemon crash degrades to brief cold-fetch unavailability
    /// instead of killing the mount (warm reads continue to work
    /// because the sidecar reads pack bytes directly from the
    /// shared on-disk CAS).
    ///
    /// With this flag, `--cache-dir`, `--remote`, and `--fetcher`
    /// are ignored: the daemon already owns the cache and the
    /// fetcher choice. `--offline` is rejected.
    #[arg(long, value_name = "PATH")]
    daemon_socket: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum FetcherChoice {
    /// Use system Git's partial-clone promisor path.
    Git,
    /// Use the optional GVFS protocol backend.
    #[cfg(feature = "gvfs-fetcher")]
    Gvfs,
}

#[derive(Debug, clap::Args)]
struct MountMultiArgs {
    /// Source repository: a URL or a path to a local checkout /
    /// bare repository. The repo is opened **once**; every
    /// `--mount` projection of it shares the same `ObjectStore`,
    /// `Fetcher`, and in-memory caches.
    source: String,

    /// One projection to mount, formatted as `REF=PATH`. The ref is
    /// resolved against the source repo; the path must already
    /// exist as a directory. Repeat to host multiple projections
    /// in this process. `--commit` and `--subtree` are not yet
    /// supported in `mount-multi`; use single-mount for those.
    #[arg(long = "mount", value_name = "REF=PATH",
          value_parser = parse_mount_spec, required = true,
          num_args = 1.., action = clap::ArgAction::Append)]
    mounts: Vec<MountSpec>,

    /// Where to keep clones of remote repositories. Only used when
    /// `source` is a URL.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Pass `--depth=N` to `git clone` when partial-cloning a URL
    /// source. See `projgit mount --help` for the full tradeoff;
    /// applies the same way here. Ignored for local sources.
    #[arg(long, value_name = "N")]
    depth: Option<u32>,

    /// Remote name to fetch from (default `origin`). Ignored in
    /// `--offline` mode and for local sources.
    #[arg(long, default_value = "origin")]
    remote: String,

    /// Skip on-demand fetching. Any miss against the local store
    /// surfaces as an I/O error.
    #[arg(long)]
    offline: bool,

    /// On unmount, print shared `ObjectStore` cache stats and
    /// per-projection prefetch counters.
    #[arg(long)]
    stats: bool,

    /// Apply to every projection: don't synthesize a `.git/` directory
    /// at the projection root. See `mount --no-dotgit` for the
    /// semantics.
    #[arg(long)]
    no_dotgit: bool,

    /// Apply to every projection: pass the `allow_other` FUSE mount
    /// option. See `mount --allow-other` for the security note.
    #[arg(long)]
    allow_other: bool,

    /// Fetch backend for URL-backed mounts.
    #[arg(long, value_enum, default_value = "git")]
    fetcher: FetcherChoice,

    /// Base GVFS protocol URL, with or without a trailing `/gvfs`.
    /// Required when `--fetcher gvfs` is selected.
    #[cfg(feature = "gvfs-fetcher")]
    #[arg(long, value_name = "URL")]
    gvfs_url: Option<String>,
}

#[derive(Debug, Clone)]
struct MountSpec {
    ref_name: String,
    mountpoint: PathBuf,
}

fn parse_mount_spec(s: &str) -> Result<MountSpec, String> {
    let (r, p) = s
        .split_once('=')
        .ok_or_else(|| format!("expected REF=PATH, got `{s}`"))?;
    if r.is_empty() {
        return Err(format!("ref name is empty in `{s}`"));
    }
    if p.is_empty() {
        return Err(format!("mountpoint is empty in `{s}`"));
    }
    Ok(MountSpec {
        ref_name: r.to_owned(),
        mountpoint: PathBuf::from(p),
    })
}

#[derive(Debug, clap::Args)]
struct AttachArgs {
    /// Unix socket the daemon is listening on. Defaults to
    /// `$XDG_RUNTIME_DIR/projgitd.sock` (fall back to
    /// `/tmp/projgitd-<uid>.sock`), matching `projgitd --socket`.
    #[arg(long, value_name = "PATH", global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    op: AttachOp,
}

#[derive(Debug, clap::Subcommand)]
enum AttachOp {
    /// Liveness check.
    Ping,
    /// Snapshot of the daemon's mount registry + cache counters.
    Status,
    /// Graceful daemon shutdown.
    Shutdown,
    /// Ask the daemon to mount a projection.
    Mount {
        /// Source repo (URL or local path).
        source: String,
        /// Ref name to project.
        #[arg(long = "ref", value_name = "REF")]
        ref_name: String,
        /// Existing empty directory the daemon should mount on.
        #[arg(long, value_name = "PATH")]
        mountpoint: PathBuf,
        /// Skip the synthesised `.git/` overlay.
        #[arg(long)]
        no_dotgit: bool,
        /// Set `allow_other` on the FUSE mount.
        #[arg(long)]
        allow_other: bool,
    },
    /// Ask the daemon to unmount a previously-mounted projection.
    Umount {
        /// Mountpoint passed to the prior `mount` request.
        #[arg(long, value_name = "PATH")]
        mountpoint: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match cli.command {
        Command::Mount(args) => cmd_mount(args),
        Command::MountMulti(args) => cmd_mount_multi(args),
        Command::Attach(args) => cmd_attach(args),
        Command::Checkout(args) => cmd_checkout(args),
        Command::SwapNotify => cmd_swap_notify(),
        Command::Fsmonitor { .. } => cmd_fsmonitor(),
    }
}

fn init_logging(verbose: u8) {
    // Deliberately not pulling in `tracing-subscriber` for MVP — the
    // CLI's diagnostics today are short eprintln! lines. Stash the
    // verbosity in an env var so future tracing init can read it
    // without changing the arg surface.
    let level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    std::env::set_var("PROJGIT_LOG", level);
}

// ---------------------------------------------------------------------------
// `mount`
// ---------------------------------------------------------------------------

/// Spawn a background eager-tree warm of `projection`'s tree closure
/// into the shared store, so the first `os.walk` over the mount is
/// network-free without blocking the mount. Best-effort: anything not
/// warmed self-heals on the on-demand path. Mirrors the daemon's
/// `handle_mount` warm for the standalone and sidecar mount paths.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_tree_warm<F>(
    hydrating: &std::sync::Arc<projgit_core::HydratingObjectStore<F>>,
    projection: &projgit_core::Projection,
) where
    F: projgit_core::Fetcher + 'static,
{
    if let Ok(root_tree) = projection.root_tree(hydrating.store()) {
        let h = std::sync::Arc::clone(hydrating);
        std::thread::spawn(move || {
            let _ = h.warm_tree_closure(root_tree);
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmd_mount(args: MountArgs) -> Result<()> {
    #[cfg(feature = "gvfs-fetcher")]
    use projgit_core::GvfsFetcher;
    use projgit_core::{
        clone::{git_dir_for, partial_clone, CloneOptions},
        GitCliFetcher, HydratingObjectStore, NoopFetcher, ObjectStore, ProjectionFsProvider,
    };
    use projgit_fuse::MountConfig;
    use std::sync::Arc;

    if !args.mountpoint.is_dir() {
        bail!(
            "mountpoint {} does not exist or is not a directory",
            args.mountpoint.display()
        );
    }

    // Stage 3 sidecar mode: the daemon owns the cache + the fetcher.
    // We just discover its git_dir, open our own (shared) ObjectStore
    // at that path, and use a DaemonFetcher to coordinate cold-path
    // hydration. The FUSE protocol loop runs locally so a daemon
    // crash degrades to brief cold-fetch unavailability rather than
    // killing the mount. `--writable` composes with the sidecar: the
    // scratch worktree shares the daemon's CAS.
    if args.writable {
        if args.no_dotgit {
            bail!("--writable cannot be combined with --no-dotgit (it needs a synthesized .git link)");
        }
        if args.subtree.is_some() {
            bail!("--writable cannot be combined with --subtree");
        }
    }
    if let Some(sock) = args.daemon_socket.clone() {
        return cmd_mount_via_daemon(args, sock);
    }

    // 1. Resolve `source` to a local git directory.
    let (git_dir, source_is_url) = if looks_like_url(&args.source) {
        let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
        let dest = cache_dir.join(cache_subdir_for_url(&args.source));
        if !dest.exists() {
            let depth_note = match args.depth {
                Some(n) => format!(" (--depth={n})"),
                None => String::new(),
            };
            eprintln!(
                "projgit: partial-cloning {} into {}{}",
                args.source,
                dest.display(),
                depth_note,
            );
            std::fs::create_dir_all(&cache_dir)
                .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
            let mut opts = CloneOptions::new(args.source.clone(), dest.clone());
            if let Some(n) = args.depth {
                if n == 0 {
                    bail!("--depth 0 is not supported; git rejects --depth=0");
                }
                opts = opts.with_depth(n);
            }
            partial_clone(&opts).with_context(|| format!("cloning {}", args.source))?;
        } else {
            eprintln!("projgit: reusing cached clone at {}", dest.display());
        }
        (git_dir_for(&dest), true)
    } else {
        let p = PathBuf::from(&args.source);
        if !p.exists() {
            bail!("source path {} does not exist", p.display());
        }
        (git_dir_for(&p), false)
    };

    // 2. Open the store.
    let store = Arc::new(
        ObjectStore::open(&git_dir)
            .with_context(|| format!("opening object store at {}", git_dir.display()))?,
    );

    // 3. Build the projection. Done after opening the store so we
    //    can peel `--ref` to a commit OID when combined with `--subtree`.
    //    Mutable because a writable mount may re-pin it to a persisted
    //    HEAD when reusing an existing worktree (see below).
    let mut projection = build_projection(&args, &store)?;

    // 4. Pick a fetcher.
    //
    //    - Local sources & `--offline` get `NoopFetcher`: no network
    //      attempts, misses surface as I/O errors.
    //    - URL sources get `GitCliFetcher` (the production default).
    //      We use the system `git` rather than `GixFetcher` because
    //      modern GitHub rejects the bare-OID `allow-tip-sha1-in-want`
    //      requests `GixFetcher` issues; `git`'s native promisor-fetch
    //      protocol works against the same servers. See the
    //      `GitCliFetcher` module docs for the full rationale.
    //
    //    The two arms diverge at the type level
    //    (`ProjectionFsProvider<F>` is generic over `Fetcher`), so we
    //    instantiate the concrete provider in each arm and call the
    //    same generic `run_mount`. `--remote` is currently advisory:
    //    `git`'s promisor mechanism uses whatever remote the partial
    //    clone configured (typically `origin`).
    let _remote_hint = &args.remote;
    let mut upper_dir: Option<PathBuf> = None;
    let mut writable_scratch: Option<PathBuf> = None;
    let (overlay, writable) = if args.writable {
        let w = prepare_writable(&args, &store, &git_dir, &projection, source_is_url)?;
        // Pin the LOWER projection to the worktree's effective commit:
        // the requested tip on a fresh worktree, or the persisted HEAD
        // when reusing one whose branch advanced via committed work.
        projection = projgit_core::Projection::Commit(w.commit);
        upper_dir = Some(w.upper_dir);
        writable_scratch = Some(w.scratch);
        (w.overlay, true)
    } else {
        (build_root_overlay(&args, &projection, &store, &git_dir)?, false)
    };
    let mut cfg = MountConfig::default();
    if args.allow_other {
        cfg.acl = projgit_fuse::SessionACL::All;
    }
    cfg.upper_dir = upper_dir;
    let mp = args.mountpoint.clone();
    let print_stats = args.stats;

    if args.offline || !source_is_url {
        if args.fetcher != FetcherChoice::Git {
            bail!("--fetcher gvfs requires a URL source and cannot be combined with --offline");
        }
        let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
        spawn_tree_warm(&hydrating, &projection);
        let provider = Arc::new(
            ProjectionFsProvider::new(projection, hydrating, overlay, /* projection_id */ 1)
                .context("building ProjectionFsProvider")?,
        );
        run_mount(provider, &mp, &cfg, print_stats, writable, writable_scratch)
    } else {
        match args.fetcher {
            FetcherChoice::Git => {
                let fetcher = GitCliFetcher::open(store.clone())
                    .context("opening GitCliFetcher (needs `git` on PATH)")?;
                let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
                spawn_tree_warm(&hydrating, &projection);
                let provider = Arc::new(
                    ProjectionFsProvider::new(
                        projection, hydrating, overlay, /* projection_id */ 1,
                    )
                    .context("building ProjectionFsProvider")?,
                );
                run_mount(provider, &mp, &cfg, print_stats, writable, writable_scratch)
            }
            #[cfg(feature = "gvfs-fetcher")]
            FetcherChoice::Gvfs => {
                let gvfs_url = args
                    .gvfs_url
                    .as_deref()
                    .ok_or_else(|| anyhow!("--fetcher gvfs requires --gvfs-url <URL>"))?;
                let fetcher = match std::env::var("PROJGIT_GVFS_TOKEN") {
                    Ok(token) if !token.is_empty() => {
                        GvfsFetcher::with_bearer_token(store.clone(), gvfs_url, token)
                    }
                    _ => GvfsFetcher::open(store.clone(), gvfs_url),
                }
                .context("opening GvfsFetcher")?;
                let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
                spawn_tree_warm(&hydrating, &projection);
                let provider = Arc::new(
                    ProjectionFsProvider::new(
                        projection, hydrating, overlay, /* projection_id */ 1,
                    )
                    .context("building ProjectionFsProvider")?,
                );
                run_mount(provider, &mp, &cfg, print_stats, writable, writable_scratch)
            }
        }
    }
}

/// Sidecar-mode `mount`: talk to a running `projgitd`, discover its
/// on-disk git_dir, open our own `ObjectStore` against the shared
/// CAS, and serve FUSE locally with a `DaemonFetcher`. Stage 3 of
/// the projgitd plan; see `docs/design/projgitd.md` §3 / §5 for the
/// failure-mode reasoning.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmd_mount_via_daemon(args: MountArgs, socket: PathBuf) -> Result<()> {
    use projgit_core::{HydratingObjectStore, ObjectStore, ProjectionFsProvider};
    use projgit_daemon::protocol::{read_message, write_message, Request, Response};
    use projgit_daemon::DaemonFetcher;
    use projgit_fuse::MountConfig;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    if args.offline {
        bail!("--offline is incompatible with --daemon-socket; the daemon owns the fetcher");
    }
    if !socket.exists() {
        bail!(
            "daemon socket {} does not exist; is `projgitd` running?",
            socket.display()
        );
    }

    // 1. Attach: ask the daemon to bind to `source` (clones if it
    //    hasn't already) and tell us where the on-disk CAS lives.
    let git_dir = {
        let mut s = UnixStream::connect(&socket)
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?;
        write_message(
            &mut s,
            &Request::Attach {
                source: args.source.clone(),
            },
        )
        .context("writing Attach request")?;
        match read_message::<_, Response>(&mut s).context("reading Attach response")? {
            Response::Attached { git_dir } => git_dir,
            Response::Err { code, message } => {
                bail!("daemon refused Attach (code `{code}`): {message}");
            }
            other => bail!("unexpected response to Attach: {other:?}"),
        }
    };
    eprintln!(
        "projgit: attached to daemon at {} (shared CAS: {})",
        socket.display(),
        git_dir.display()
    );

    // 2. Open the shared on-disk ObjectStore. Two processes (this
    //    sidecar + the daemon) read the same gix repo concurrently;
    //    git's on-disk format supports this natively (mmap'd packs
    //    + lock files on writes, which the sidecar never does).
    let store = Arc::new(
        ObjectStore::open(&git_dir)
            .with_context(|| format!("opening object store at {}", git_dir.display()))?,
    );

    // 3. Resolve --ref / --commit / --subtree against the local
    //    store. The daemon has already populated refs as part of
    //    Attach (partial-clone wrote `.git/refs/`); the sidecar
    //    sees them through gix's normal ref-resolution.
    //    Mutable because a writable mount re-pins it to the scratch
    //    worktree's effective commit.
    let mut projection = build_projection(&args, &store)?;

    // Writable vs read-only overlay. A `--writable` sidecar shares the
    // daemon's CAS (the scratch git dir's objects link into `git_dir`,
    // so committed + hydrated objects both land in the shared store).
    let (overlay, writable, upper_dir, writable_scratch) = if args.writable {
        let w = prepare_writable(&args, &store, &git_dir, &projection, looks_like_url(&args.source))?;
        projection = projgit_core::Projection::Commit(w.commit);
        (w.overlay, true, Some(w.upper_dir), Some(w.scratch))
    } else {
        (
            build_root_overlay(&args, &projection, &store, &git_dir)?,
            false,
            None,
            None,
        )
    };

    // 4. DaemonFetcher coordinates cold-path hydration with the
    //    daemon. Warm paths read straight from the shared CAS and
    //    never touch the socket.
    let fetcher = DaemonFetcher::new(socket.clone());
    let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), fetcher));

    // Eager-tree warm in the background (sidecar path) so the first
    // os.walk is network-free; tree-fetches route through the daemon
    // via FetchMany. Mirrors the daemon's handle_mount warm.
    spawn_tree_warm(&hydrating, &projection);

    let mut cfg = MountConfig::default();
    if args.allow_other {
        cfg.acl = projgit_fuse::SessionACL::All;
    }
    cfg.upper_dir = upper_dir;
    let mp = args.mountpoint.clone();
    let print_stats = args.stats;

    let provider = Arc::new(
        ProjectionFsProvider::new(projection, hydrating, overlay, /* projection_id */ 1)
            .context("building ProjectionFsProvider")?,
    );
    run_mount(provider, &mp, &cfg, print_stats, writable, writable_scratch)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmd_mount(_args: MountArgs) -> Result<()> {
    bail!(
        "`projgit mount` is not yet supported on this platform. \
         Linux/macOS use FUSE (projgit-fuse); Windows support is \
         deferred to the planned projgit-winfsp backend."
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmd_mount_multi(_args: MountMultiArgs) -> Result<()> {
    bail!(
        "`projgit mount-multi` is not yet supported on this platform. \
         Linux/macOS use FUSE (projgit-fuse); Windows support is \
         deferred to the planned projgit-winfsp backend."
    );
}

// ---------------------------------------------------------------------------
// `mount-multi` (Stage 1 of the projgitd plan)
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmd_mount_multi(args: MountMultiArgs) -> Result<()> {
    #[cfg(feature = "gvfs-fetcher")]
    use projgit_core::GvfsFetcher;
    use projgit_core::{
        clone::{git_dir_for, partial_clone, CloneOptions},
        GitCliFetcher, HydratingObjectStore, NoopFetcher, ObjectStore,
    };
    use std::sync::Arc;

    if args.mounts.is_empty() {
        bail!("at least one --mount REF=PATH is required");
    }
    // Reject duplicate mountpoints up front; the underlying mount(2)
    // would just blow up later and the error wouldn\'t name what
    // collided.
    let mut seen = std::collections::HashSet::new();
    for spec in &args.mounts {
        if !spec.mountpoint.is_dir() {
            bail!(
                "mountpoint {} does not exist or is not a directory",
                spec.mountpoint.display()
            );
        }
        let canon = std::fs::canonicalize(&spec.mountpoint).with_context(|| {
            format!("canonicalizing mountpoint {}", spec.mountpoint.display())
        })?;
        if !seen.insert(canon.clone()) {
            bail!(
                "mountpoint {} listed more than once",
                spec.mountpoint.display()
            );
        }
    }

    // 1. Resolve `source` to a local git directory (same as cmd_mount).
    let (git_dir, source_is_url) = if looks_like_url(&args.source) {
        let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
        let dest = cache_dir.join(cache_subdir_for_url(&args.source));
        if !dest.exists() {
            let depth_note = match args.depth {
                Some(n) => format!(" (--depth={n})"),
                None => String::new(),
            };
            eprintln!(
                "projgit: partial-cloning {} into {}{}",
                args.source,
                dest.display(),
                depth_note,
            );
            std::fs::create_dir_all(&cache_dir)
                .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
            let mut opts = CloneOptions::new(args.source.clone(), dest.clone());
            if let Some(n) = args.depth {
                if n == 0 {
                    bail!("--depth 0 is not supported; git rejects --depth=0");
                }
                opts = opts.with_depth(n);
            }
            partial_clone(&opts).with_context(|| format!("cloning {}", args.source))?;
        } else {
            eprintln!("projgit: reusing cached clone at {}", dest.display());
        }
        (git_dir_for(&dest), true)
    } else {
        let p = PathBuf::from(&args.source);
        if !p.exists() {
            bail!("source path {} does not exist", p.display());
        }
        (git_dir_for(&p), false)
    };

    // 2. Open the shared ObjectStore (one per process, regardless of N).
    let store = Arc::new(
        ObjectStore::open(&git_dir)
            .with_context(|| format!("opening object store at {}", git_dir.display()))?,
    );

    // 3. Pick a fetcher; dispatch into the generic helper. The arms
    //    differ only at the fetcher\'s concrete type.
    let _remote_hint = &args.remote;
    if args.offline || !source_is_url {
        if args.fetcher != FetcherChoice::Git {
            bail!("--fetcher gvfs requires a URL source and cannot be combined with --offline");
        }
        let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), NoopFetcher));
        run_mount_multi(store, hydrating, &args, &git_dir)
    } else {
        match args.fetcher {
            FetcherChoice::Git => {
                let fetcher = GitCliFetcher::open(store.clone())
                    .context("opening GitCliFetcher (needs `git` on PATH)")?;
                let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), fetcher));
                run_mount_multi(store, hydrating, &args, &git_dir)
            }
            #[cfg(feature = "gvfs-fetcher")]
            FetcherChoice::Gvfs => {
                let gvfs_url = args
                    .gvfs_url
                    .as_deref()
                    .ok_or_else(|| anyhow!("--fetcher gvfs requires --gvfs-url <URL>"))?;
                let fetcher = match std::env::var("PROJGIT_GVFS_TOKEN") {
                    Ok(token) if !token.is_empty() => {
                        GvfsFetcher::with_bearer_token(store.clone(), gvfs_url, token)
                    }
                    _ => GvfsFetcher::open(store.clone(), gvfs_url),
                }
                .context("opening GvfsFetcher")?;
                let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), fetcher));
                run_mount_multi(store, hydrating, &args, &git_dir)
            }
        }
    }
}

/// Shared per-fetcher body. Loops over the user\'s `--mount` specs,
/// building one `ProjectionFsProvider` per spec (all sharing the same
/// `Arc<HydratingObjectStore<F>>`) and spawning one
/// `mount_background` per spec. Holds every `BackgroundSession` until
/// Ctrl-C; drop-order unmounts all cleanly.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_mount_multi<F>(
    store: std::sync::Arc<projgit_core::ObjectStore>,
    hydrating: std::sync::Arc<projgit_core::HydratingObjectStore<F>>,
    args: &MountMultiArgs,
    git_dir: &Path,
) -> Result<()>
where
    F: projgit_core::Fetcher + 'static,
{
    use projgit_core::{Projection, ProjectionFsProvider};
    use projgit_fuse::MountConfig;
    use std::sync::Arc;

    let mut cfg = MountConfig::default();
    if args.allow_other {
        cfg.acl = projgit_fuse::SessionACL::All;
    }

    let mut sessions: Vec<projgit_fuse::BackgroundSession> = Vec::with_capacity(args.mounts.len());
    let mut providers: Vec<Arc<ProjectionFsProvider<F>>> = Vec::with_capacity(args.mounts.len());

    for (idx, spec) in args.mounts.iter().enumerate() {
        // projection_id is 1-based so it stays distinct from the
        // single-mount path (which uses 1) when this code shares a
        // future daemon address space.
        let projection_id = (idx + 1) as u64;
        let projection = Projection::Ref(spec.ref_name.clone());
        let overlay =
            build_overlay_no_subtree(args.no_dotgit, &projection, &store, git_dir).with_context(
                || format!("building overlay for {}", spec.ref_name),
            )?;
        let provider = Arc::new(
            ProjectionFsProvider::new(projection, hydrating.clone(), overlay, projection_id)
                .with_context(|| {
                    format!(
                        "building ProjectionFsProvider for {} -> {}",
                        spec.ref_name,
                        spec.mountpoint.display()
                    )
                })?,
        );
        eprintln!(
            "projgit: mounting {} at {} (projection_id={})",
            spec.ref_name,
            spec.mountpoint.display(),
            projection_id
        );
        let session = projgit_fuse::mount_background(provider.clone(), &spec.mountpoint, &cfg)
            .with_context(|| format!("mounting at {}", spec.mountpoint.display()))?;
        sessions.push(session);
        providers.push(provider);
    }

    eprintln!(
        "projgit: {} projection(s) mounted; Ctrl-C to unmount all",
        sessions.len()
    );

    // Park on Ctrl-C / SIGTERM. Dropping every session unmounts via
    // fuser\'s Drop impl.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("installing Ctrl-C handler")?;
    rx.recv().ok();

    eprintln!("projgit: unmounting {} session(s)…", sessions.len());
    drop(sessions);

    if args.stats {
        // Tree / header / blob caches are shared across all
        // providers (they live on the inner ObjectStore), so print
        // them once. Prefetch counters are per-provider; print one
        // line per mount.
        let s = store.as_ref();
        let t = s.tree_cache_stats();
        let h = s.header_cache_stats();
        let b = s.blob_cache_stats();
        eprintln!(
            "projgit: tree cache    hits={} misses={} inserts={} evictions={} len={}/{}",
            t.hits, t.misses, t.inserts, t.evictions, t.len, t.capacity,
        );
        eprintln!(
            "projgit: header cache  hits={} misses={} inserts={} evictions={} len={}/{}",
            h.hits, h.misses, h.inserts, h.evictions, h.len, h.capacity,
        );
        eprintln!(
            "projgit: blob cache    hits={} misses={} inserts={} evictions={} skipped_too_large={} bytes={}/{}",
            b.hits, b.misses, b.inserts, b.evictions, b.skipped_too_large,
            b.bytes_used, b.capacity_bytes,
        );
        for (provider, spec) in providers.iter().zip(args.mounts.iter()) {
            let p = provider.prefetch_stats();
            eprintln!(
                "projgit: prefetch ({:<20}) posted={} dropped={} batches={} resolved={} headers={} failed={} blobs_warmed={} blobs_skipped={}",
                spec.ref_name,
                p.posted, p.dropped, p.batches_sent, p.oids_resolved, p.headers_published, p.oids_failed,
                p.blobs_warmed, p.blobs_skipped,
            );
        }
    }

    eprintln!("projgit: unmounted all.");
    Ok(())
}

/// `build_root_overlay` sibling that takes individual flags rather
/// than the single-mount `MountArgs`. Behaviour identical: A1+ overlay
/// by default, plus A2 ref visibility when the projection is a
/// branch; empty overlay if `no_dotgit` or `Projection::Subtree`
/// (the latter unreachable today since `mount-multi` only accepts
/// refs, but the guard stays for the future).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_overlay_no_subtree(
    no_dotgit: bool,
    projection: &projgit_core::Projection,
    store: &std::sync::Arc<projgit_core::ObjectStore>,
    git_dir: &Path,
) -> Result<projgit_core::RootOverlay> {
    use projgit_core::{dotgit, Projection, RootOverlay};

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
    apply_a2_if_branch(&mut overlay, projection, store, commit_oid);
    Ok(overlay)
}


#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_mount<F>(
    provider: std::sync::Arc<projgit_core::ProjectionFsProvider<F>>,
    mountpoint: &std::path::Path,
    config: &projgit_fuse::MountConfig,
    print_stats: bool,
    writable: bool,
    writable_scratch: Option<PathBuf>,
) -> Result<()>
where
    F: projgit_core::Fetcher + 'static,
{
    eprintln!(
        "projgit: mounting at {} ({}, Ctrl-C to unmount)",
        mountpoint.display(),
        if writable { "writable" } else { "read-only" },
    );
    // A writable mount keeps its `WritableHandle` so the HEAD watcher and
    // the control socket can swap the LOWER baseline on checkout. Read-only
    // mounts need neither.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut watcher: Option<std::thread::JoinHandle<()>> = None;
    let mut control: Option<std::thread::JoinHandle<()>> = None;
    let session = if writable {
        let (session, handle) =
            projgit_fuse::mount_writable_background_with_handle(provider.clone(), mountpoint, config)
                .with_context(|| format!("mounting (writable) at {}", mountpoint.display()))?;
        match writable_scratch {
            Some(scratch) => {
                let swapper =
                    std::sync::Arc::new(Swapper::new(provider.clone(), handle, scratch.clone()));
                let socket = scratch.join(CONTROL_SOCKET_NAME);
                control = Some(spawn_control_listener(swapper.clone(), socket, stop.clone()));
                // Prefer the `reference-transaction` hook to observe stock
                // git ops (push, synchronous). Fall back to the HEAD poll
                // watcher only if the hook can't be installed.
                match install_reference_hook(&scratch) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!(
                            "projgit: reference-transaction hook install failed ({e:#}); using HEAD poll fallback"
                        );
                        watcher = Some(spawn_head_watcher(swapper, scratch, stop.clone()));
                    }
                }
            }
            None => drop(handle),
        }
        session
    } else {
        projgit_fuse::mount_background(provider.clone(), mountpoint, config)
            .with_context(|| format!("mounting at {}", mountpoint.display()))?
    };

    // Park the main thread on a Ctrl-C / SIGTERM. Dropping `session`
    // synchronously unmounts via fuser's drop impl.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("installing Ctrl-C handler")?;
    rx.recv().ok();

    eprintln!("projgit: unmounting…");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(session);
    if let Some(w) = watcher {
        let _ = w.join();
    }
    if let Some(c) = control {
        let _ = c.join();
    }

    if print_stats {
        let store = provider.store().store();
        let t = store.tree_cache_stats();
        let h = store.header_cache_stats();
        let b = store.blob_cache_stats();
        let p = provider.prefetch_stats();
        eprintln!(
            "projgit: tree cache    hits={} misses={} inserts={} evictions={} len={}/{}",
            t.hits, t.misses, t.inserts, t.evictions, t.len, t.capacity,
        );
        eprintln!(
            "projgit: header cache  hits={} misses={} inserts={} evictions={} len={}/{}",
            h.hits, h.misses, h.inserts, h.evictions, h.len, h.capacity,
        );
        eprintln!(
            "projgit: blob cache    hits={} misses={} inserts={} evictions={} skipped_too_large={} bytes={}/{}",
            b.hits,
            b.misses,
            b.inserts,
            b.evictions,
            b.skipped_too_large,
            b.bytes_used,
            b.capacity_bytes,
        );
        eprintln!(
            "projgit: prefetch (T1) posted={} dropped={} batches={} resolved={} headers={} failed={} blobs_warmed={} blobs_skipped={}",
            p.posted,
            p.dropped,
            p.batches_sent,
            p.oids_resolved,
            p.headers_published,
            p.oids_failed,
            p.blobs_warmed,
            p.blobs_skipped,
        );
    }

    eprintln!("projgit: unmounted.");
    Ok(())
}

/// Filename of a writable mount's control socket, inside the scratch git
/// dir. `projgit checkout` connects here to drive a synchronous baseline
/// swap (so a checkout is consistent by the time the command returns).
const CONTROL_SOCKET_NAME: &str = "projgit-control.sock";

/// Shared baseline swapper for a writable mount: builds a new LOWER
/// projection for a target commit (reusing the same hydrating store +
/// fetcher via [`ProjectionFsProvider::store_arc`]) and swaps it under
/// the live mount, deduplicating against the currently-projected commit.
/// Shared (behind `Arc`) by the HEAD watcher (stock git ops) and the
/// control-socket listener (`projgit checkout`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Swapper<F: projgit_core::Fetcher + 'static> {
    provider: std::sync::Arc<projgit_core::ProjectionFsProvider<F>>,
    handle: projgit_fuse::WritableHandle<projgit_core::ProjectionFsProvider<F>>,
    scratch: PathBuf,
    next_id: std::sync::atomic::AtomicU64,
    current: std::sync::Mutex<Option<gix::ObjectId>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<F: projgit_core::Fetcher + 'static> Swapper<F> {
    fn new(
        provider: std::sync::Arc<projgit_core::ProjectionFsProvider<F>>,
        handle: projgit_fuse::WritableHandle<projgit_core::ProjectionFsProvider<F>>,
        scratch: PathBuf,
    ) -> Self {
        // Seed `current` with the mounted commit so the watcher's first
        // poll doesn't trigger a spurious swap.
        let current = resolve_head_commit_fast(&scratch);
        Self {
            provider,
            handle,
            scratch,
            next_id: std::sync::atomic::AtomicU64::new(2),
            current: std::sync::Mutex::new(current),
        }
    }

    /// Swap the LOWER baseline to `commit` unless it is already current.
    /// Returns `Ok(true)` if a swap happened. Serialized (and dedup'd)
    /// against concurrent callers — the watcher and the control socket.
    fn swap_to(&self, commit: gix::ObjectId) -> Result<bool> {
        use std::sync::atomic::Ordering;
        let mut cur = self.current.lock().unwrap();
        if *cur == Some(commit) {
            return Ok(false);
        }
        let overlay = build_writable_overlay(&self.scratch)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let new_lower = projgit_core::ProjectionFsProvider::new(
            projgit_core::Projection::Commit(commit),
            self.provider.store_arc(),
            overlay,
            id,
        )
        .with_context(|| format!("re-projecting worktree to {commit}"))?;
        self.handle.swap_baseline(std::sync::Arc::new(new_lower));
        *cur = Some(commit);
        Ok(true)
    }

    /// Current FSMonitor (query protocol v2) response bytes for the mount.
    fn fsmonitor_response(&self) -> Vec<u8> {
        self.handle.fsmonitor_response()
    }
}

/// Watch a writable mount's scratch git dir `HEAD` and swap the LOWER
/// baseline whenever it moves — the safety net for STOCK `git checkout` /
/// `switch` / `reset` / `pull` / `commit` (which we don't control).
/// `projgit checkout` drives the swap synchronously over the control
/// socket instead. Polls (reads the ref files directly, no subprocess);
/// exits when `stop` is set on unmount.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_head_watcher<F>(
    swapper: std::sync::Arc<Swapper<F>>,
    scratch: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()>
where
    F: projgit_core::Fetcher + 'static,
{
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    std::thread::Builder::new()
        .name("projgit-head-watcher".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some(head) = resolve_head_commit_fast(&scratch) else {
                    continue;
                };
                match swapper.swap_to(head) {
                    Ok(true) => {
                        eprintln!("projgit: checkout observed — re-projected worktree to {head}")
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("projgit: re-projection to {head} failed: {e:#}"),
                }
            }
        })
        .expect("spawning HEAD watcher thread")
}

/// Serve the writable mount's control socket: `projgit checkout` sends
/// `SWAP <oid>\n` and the mount performs the baseline swap synchronously,
/// replying `OK\n` (or `ERR <msg>\n`) once applied — so a checkout is
/// consistent by the time the command returns (no poll lag). Falls back
/// to a no-op keepalive thread if the socket can't be bound. Exits when
/// `stop` is set on unmount.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_control_listener<F>(
    swapper: std::sync::Arc<Swapper<F>>,
    socket_path: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()>
where
    F: projgit_core::Fetcher + 'static,
{
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    // Replace any stale socket from a previous (crashed) mount.
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "projgit: control socket unavailable ({e}); `projgit checkout` will fall back to the poll watcher"
            );
            return std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
        }
    };
    let _ = listener.set_nonblocking(true);

    std::thread::Builder::new()
        .name("projgit-control".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut reader = match stream.try_clone() {
                            Ok(s) => BufReader::new(s),
                            Err(_) => continue,
                        };
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() {
                            continue;
                        }
                        let mut stream = stream;
                        let line = line.trim();
                        if line.starts_with("FSMONITOR") {
                            // Binary v2 response; closing the stream is the
                            // client's EOF.
                            let _ = stream.write_all(&swapper.fsmonitor_response());
                        } else {
                            let reply = match line.strip_prefix("SWAP ") {
                                Some(hex) => match gix::ObjectId::from_hex(hex.as_bytes()) {
                                    Ok(oid) => match swapper.swap_to(oid) {
                                        Ok(_) => "OK\n".to_string(),
                                        Err(e) => format!("ERR {e:#}\n"),
                                    },
                                    Err(_) => "ERR bad oid\n".to_string(),
                                },
                                None => "ERR bad request\n".to_string(),
                            };
                            let _ = stream.write_all(reply.as_bytes());
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
            let _ = std::fs::remove_file(&socket_path);
        })
        .expect("spawning control-socket thread")
}

/// `projgit checkout <ref>` — switch a live **writable** mount to another
/// commit/branch **without rewriting the worktree**. Updates the mount's
/// scratch git index + HEAD via plumbing (`read-tree` then
/// `symbolic-ref`/`update-ref`), so the O(N-file) worktree write stock
/// `git checkout` performs is avoided; the mount's HEAD watcher then
/// swaps the LOWER baseline and the worktree re-virtualizes to the new
/// commit. Local edits survive (the upper is path-keyed).
fn cmd_checkout(args: CheckoutArgs) -> Result<()> {
    let dir = match args.dir {
        Some(d) => d,
        None => std::env::current_dir().context("getting current directory")?,
    };
    // Resolve the mount's git dir (follows the `.git` gitdir-link file to
    // the writable scratch git dir).
    let git_dir = git_capture(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--absolute-git-dir"]),
    )
    .context("resolving git dir — run `projgit checkout` inside a writable mount")?;
    let git_dir = PathBuf::from(git_dir.trim());

    // Resolve the target commit.
    let commit = git_capture(
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["rev-parse", "--verify", &format!("{}^{{commit}}", args.refname)]),
    )
    .with_context(|| format!("resolving ref '{}' in the mount repository", args.refname))?;
    let commit = commit.trim().to_string();

    // Is the ref a local branch (switch) or something else (detached)?
    let is_branch = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&git_dir)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", args.refname),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // 1) Update the index to the target tree WITHOUT touching the
    //    worktree (no `-u`). git takes `index.lock`, so this is safe
    //    against a concurrent `git status` in the mount.
    git_run(
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .env("PROJGIT_SUPPRESS_HOOK", "1")
            .args(["read-tree", &commit]),
    )
    .context("updating the index to the target tree")?;

    // 2) Move HEAD — this is what the mount's watcher observes.
    if is_branch {
        git_run(
            std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .env("PROJGIT_SUPPRESS_HOOK", "1")
                .args(["symbolic-ref", "HEAD", &format!("refs/heads/{}", args.refname)]),
        )
        .context("switching HEAD to the branch")?;
    } else {
        git_run(
            std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .env("PROJGIT_SUPPRESS_HOOK", "1")
                .args(["update-ref", "--no-deref", "HEAD", &commit]),
        )
        .context("moving detached HEAD")?;
    }

    let short = &commit[..commit.len().min(12)];
    // Drive the swap synchronously over the mount's control socket so the
    // worktree reflects the new commit by the time we return. Falls back
    // to the mount's poll watcher if no socket is listening.
    let synced = notify_mount_swap(&git_dir, &commit);
    let tail = if synced {
        "mount re-projected"
    } else {
        "mount will re-project"
    };
    if is_branch {
        eprintln!(
            "projgit: checked out branch '{}' ({short}) — {tail}",
            args.refname
        );
    } else {
        eprintln!("projgit: checked out {short} (detached HEAD) — {tail}");
    }
    Ok(())
}

/// Ask a running writable mount (via its control socket in `git_dir`) to
/// swap the LOWER baseline to `commit` synchronously, returning `true`
/// once the mount acknowledges. Returns `false` if no mount is listening
/// (the caller then relies on the mount's poll watcher).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn notify_mount_swap(git_dir: &Path, commit: &str) -> bool {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let socket = git_dir.join(CONTROL_SOCKET_NAME);
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        return false;
    };
    // A cold re-projection may hydrate objects, so allow generous time.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    if stream
        .write_all(format!("SWAP {commit}\n").as_bytes())
        .is_err()
    {
        return false;
    }
    let mut reply = String::new();
    let Ok(reader) = stream.try_clone() else {
        return false;
    };
    if BufReader::new(reader).read_line(&mut reply).is_err() {
        return false;
    }
    reply.trim() == "OK"
}

/// Non-unix stub: no control socket, always falls back to the poll watcher.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn notify_mount_swap(_git_dir: &Path, _commit: &str) -> bool {
    false
}

/// `projgit __swap-notify` — invoked by the scratch git dir's
/// `reference-transaction` hook after a stock git op moves `HEAD`. Reads
/// the git dir (from `GIT_DIR`, which git sets for hooks, else the cwd),
/// resolves `HEAD`, and drives the mount's baseline swap over the control
/// socket. Silent + best-effort: a missing mount / socket is a no-op.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmd_swap_notify() -> Result<()> {
    let git_dir = match std::env::var_os("GIT_DIR") {
        Some(g) => {
            let p = PathBuf::from(g);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir()?.join(p)
            }
        }
        None => {
            let cwd = std::env::current_dir()?;
            let out = git_capture(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&cwd)
                    .args(["rev-parse", "--absolute-git-dir"]),
            )?;
            PathBuf::from(out.trim())
        }
    };
    if let Some(oid) = resolve_head_commit_fast(&git_dir) {
        let _ = notify_mount_swap(&git_dir, &oid.to_string());
    }
    Ok(())
}

/// Non-unix stub.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmd_swap_notify() -> Result<()> {
    Ok(())
}

/// `projgit __fsmonitor <version> <token>` — git's `core.fsmonitor`
/// query program for a writable mount. git runs it with cwd = the
/// worktree and no `GIT_DIR`, so we resolve the git dir from the cwd and
/// stream the mount's v2 response (`<token>\0<path>\0…`) from the
/// control socket to stdout. Any failure exits non-zero so git falls
/// back to a full worktree scan (safe).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmd_fsmonitor() -> Result<()> {
    if fsmonitor_query().is_none() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fsmonitor_query() -> Option<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let cwd = std::env::current_dir().ok()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let git_dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let socket = git_dir.join(CONTROL_SOCKET_NAME);
    let mut stream = UnixStream::connect(&socket).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    stream.write_all(b"FSMONITOR\n").ok()?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).ok()?;
    std::io::stdout().write_all(&resp).ok()?;
    Some(())
}

/// Non-unix stub: no control socket, so git full-scans.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmd_fsmonitor() -> Result<()> {
    std::process::exit(1);
}

/// Install a `core.fsmonitor` hook into the writable scratch git dir
/// (query protocol v2) that streams the mount's modified-path set from
/// the control socket, and point git's config at it. Lets `git status`
/// skip scanning the (virtual) worktree — the big-worktree win.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install_fsmonitor_hook(scratch: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().context("resolving projgit binary path")?;
    let hooks = scratch.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let hook = hooks.join("projgit-fsmonitor");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nexec \"{}\" __fsmonitor \"$@\"\n", exe.display()),
    )?;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    let hook_abs = std::fs::canonicalize(&hook)?;
    git_run(
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(scratch)
            .args(["config", "core.fsmonitorHookVersion", "2"]),
    )?;
    git_run(
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(scratch)
            .arg("config")
            .arg("core.fsmonitor")
            .arg(&hook_abs),
    )?;
    Ok(())
}

/// Install the `reference-transaction` hook into the writable scratch git
/// dir so STOCK git operations (`checkout`/`switch`/`reset`/`commit`/
/// `pull`) that move `HEAD` push the change to the mount over the control
/// socket — replacing the poll watcher. The hook shells the current
/// `projgit` binary; `$PROJGIT_SUPPRESS_HOOK` lets `projgit checkout`
/// suppress it for its own ref updates (it notifies directly).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install_reference_hook(scratch: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().context("resolving projgit binary path")?;
    let hooks = scratch.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let hook = hooks.join("reference-transaction");
    let script = format!(
        "#!/bin/sh\n\
         # projgit writable worktree: notify the mount when HEAD moves so it\n\
         # re-projects. Installed by `projgit mount --writable`.\n\
         cat >/dev/null\n\
         [ \"$1\" = committed ] || exit 0\n\
         [ -n \"$PROJGIT_SUPPRESS_HOOK\" ] && exit 0\n\
         \"{}\" __swap-notify >/dev/null 2>&1 || true\n",
        exe.display()
    );
    std::fs::write(&hook, script)?;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Run a `git` command, returning its stdout on success or an error
/// carrying stderr.
fn git_capture(cmd: &mut std::process::Command) -> Result<String> {
    let out = cmd.output().context("spawning git")?;
    if !out.status.success() {
        bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a `git` command for its side effect, erroring on failure.
fn git_run(cmd: &mut std::process::Command) -> Result<()> {
    git_capture(cmd).map(|_| ())
}


// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_projection(
    args: &MountArgs,
    store: &projgit_core::ObjectStore,
) -> Result<projgit_core::Projection> {
    use projgit_core::Projection;

    // Pick the base projection from the mutually-exclusive --ref / --commit.
    let base = match (&args.r#ref, &args.commit) {
        (Some(_), Some(_)) => unreachable!("clap rejects this combination"),
        (None, Some(hex)) => {
            let oid = gix::ObjectId::from_hex(hex.as_bytes())
                .map_err(|e| anyhow!("invalid commit OID `{hex}`: {e}"))?;
            Projection::Commit(oid)
        }
        (Some(name), None) => Projection::Ref(name.clone()),
        (None, None) => Projection::Ref("HEAD".to_owned()),
    };

    // Fold in --subtree if present.
    let Some(path) = &args.subtree else {
        return Ok(base);
    };
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(base);
    }
    // The `Subtree` variant requires a commit OID. Peel the
    // current base down to one. For `Projection::Ref` this is the
    // ref-tip resolve we already do at provider-construction time;
    // doing it here, once, keeps the resulting `Projection::Subtree`
    // self-contained and matches the rest of the projection layer's
    // "OID-based identity" rule.
    let commit = match base {
        Projection::Commit(oid) => oid,
        Projection::Ref(name) => store
            .resolve_ref(&name)
            .with_context(|| format!("resolving ref `{name}` for --subtree"))?,
        Projection::Subtree { .. } => unreachable!("base is Ref or Commit"),
    };
    Ok(Projection::Subtree {
        commit,
        path: trimmed.to_owned(),
    })
}

fn looks_like_url(s: &str) -> bool {
    s.contains("://") || s.starts_with("git@")
}

/// Construct the `RootOverlay` for the mount based on the user's flags
/// and the projection kind. Default behavior synthesizes an A1+ `.git/`
/// (A1 plus a clean read-only `.git/index` matching HEAD) pointing at
/// the commit and the shared object store, and applies **A2** ref
/// visibility on top when the projection is a branch (symbolic `HEAD`
/// → `refs/heads/<branch>` plus the loose ref file). `--no-dotgit`
/// or a `Subtree` projection yields an empty overlay.
fn build_root_overlay(
    args: &MountArgs,
    projection: &projgit_core::Projection,
    store: &std::sync::Arc<projgit_core::ObjectStore>,
    git_dir: &Path,
) -> Result<projgit_core::RootOverlay> {
    use projgit_core::{dotgit, Projection, RootOverlay};

    if args.no_dotgit {
        return Ok(RootOverlay::new());
    }
    // `.git/HEAD` would point at the full commit's tree, not the
    // subtree the user is browsing — that produces surprising
    // `git log <path>` semantics. Subtree mounts opt out by default;
    // a future variant can revisit this if the use case calls for it.
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
    apply_a2_if_branch(&mut overlay, projection, store, commit_oid);
    Ok(overlay)
}

/// Build the `RootOverlay` for a **writable** mount: a single `.git`
/// FILE that is a `gitdir:` link to the real, writable scratch git dir
/// on disk (see [`setup_writable_gitdir`]). Stock git inside the mount
/// follows the link and uses the on-disk git dir for all metadata
/// (index, objects, refs), while the worktree is the FUSE mount.
fn build_writable_overlay(scratch_gitdir: &Path) -> Result<projgit_core::RootOverlay> {
    use projgit_core::{overlay::SyntheticEntry, RootOverlay};
    let abs = std::fs::canonicalize(scratch_gitdir)
        .with_context(|| format!("canonicalizing {}", scratch_gitdir.display()))?;
    let content = format!("gitdir: {}\n", abs.display()).into_bytes();
    let mut overlay = RootOverlay::new();
    overlay.insert(".git", SyntheticEntry::file(content));
    Ok(overlay)
}

/// Everything a writable mount needs on top of a projection: the `.git`
/// overlay, the upper-journal dir, the scratch git dir (for the control
/// socket / watcher), and the effective projection commit (re-pinned to
/// the persisted `HEAD` when reusing a worktree).
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct WritableSetup {
    overlay: projgit_core::RootOverlay,
    upper_dir: PathBuf,
    scratch: PathBuf,
    commit: gix::ObjectId,
}

/// Prepare the writable scratch git dir + overlay for a `--writable`
/// mount. Shared by the standalone ([`cmd_mount`]) and sidecar
/// ([`cmd_mount_via_daemon`]) paths — the only difference between them is
/// where the object store / fetcher come from; the writable worktree
/// setup (scratch git dir sharing `git_dir`'s CAS, seeded index, imported
/// refs, optional fsmonitor hook) is identical.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn prepare_writable(
    args: &MountArgs,
    store: &projgit_core::ObjectStore,
    git_dir: &Path,
    projection: &projgit_core::Projection,
    source_is_url: bool,
) -> Result<WritableSetup> {
    let requested_oid = projection
        .resolve_commit(store)
        .context("resolving projection commit for --writable")?;
    let branch = writable_branch_name(args, git_dir);
    let remote_url =
        read_clone_remote_url(git_dir).or_else(|| source_is_url.then(|| args.source.clone()));
    let worktrees_root = resolve_cache_dir(args.cache_dir.as_deref())?.join("worktrees");
    let wt = setup_writable_gitdir(
        git_dir,
        store,
        requested_oid,
        &worktrees_root,
        &args.mountpoint,
        branch.as_deref(),
        remote_url.as_deref(),
    )?;
    let where_ = match (&branch, &remote_url) {
        (Some(b), Some(_)) => format!("on branch '{b}' (push → origin)"),
        (Some(b), None) => format!("on branch '{b}' (no remote configured)"),
        (None, _) => "detached HEAD".to_string(),
    };
    let state = if wt.reused {
        "reused — prior work restored (committed + uncommitted edits)"
    } else {
        "fresh — changes persist across unmount"
    };
    eprintln!(
        "projgit: writable mount — {where_} — git dir {} ({state})",
        wt.path.display()
    );
    if args.fsmonitor {
        if let Err(e) = install_fsmonitor_hook(&wt.path) {
            eprintln!("projgit: fsmonitor setup failed ({e:#}); status will do a full scan");
        }
    }
    let overlay = build_writable_overlay(&wt.path)?;
    Ok(WritableSetup {
        overlay,
        upper_dir: wt.path.join("projgit-upper"),
        scratch: wt.path.clone(),
        commit: wt.commit,
    })
}

/// A prepared writable scratch git dir.
struct WritableGitDir {
    /// Path to the scratch git dir (the `.git` link points here).
    path: PathBuf,
    /// The commit the LOWER projection should serve: the requested
    /// projection tip on a fresh worktree, or the persisted `HEAD`
    /// (advanced by committed work) when an existing worktree is reused.
    commit: gix::ObjectId,
    /// Whether an existing worktree with committed work was reused.
    reused: bool,
}

/// Create — or reuse — a real, writable git dir on disk for a writable
/// worktree mount. Its `objects` is a **symlink into the shared clone
/// CAS**, so committed objects are durable and readable by the
/// projection's `ObjectStore` (which opens the clone) rather than
/// stranded in a throwaway odb. The dir carries a **writable** index
/// (real per-entry stat, no `ASSUME_VALID`), `core.worktree` pointing at
/// the mountpoint, and `core.checkStat = minimal`.
///
/// When `branch` is `Some`, `HEAD` is symbolic (`refs/heads/<branch>`)
/// so commits advance a named branch; otherwise `HEAD` is detached at
/// the projection commit. When `remote_url` is `Some`, an `origin`
/// remote is configured (with branch upstream) so `git push` works out
/// of the box.
///
/// Located under `<cache>/worktrees/`, keyed by the mountpoint. If a
/// valid dir already exists there it is **reused** — its refs, index,
/// and objects survive the unmount, so committed work is restored on
/// remount and the projection is re-pinned to its `HEAD` (uncommitted
/// in-memory edits are still lost until the upper is made durable).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn setup_writable_gitdir(
    clone_git_dir: &Path,
    store: &projgit_core::ObjectStore,
    commit_oid: gix::ObjectId,
    worktrees_root: &Path,
    mountpoint: &Path,
    branch: Option<&str>,
    remote_url: Option<&str>,
) -> Result<WritableGitDir> {
    use std::collections::hash_map::DefaultHasher;
    use std::fmt::Write as _;
    use std::hash::{Hash, Hasher};

    let mp_abs = std::fs::canonicalize(mountpoint)
        .with_context(|| format!("canonicalizing mountpoint {}", mountpoint.display()))?;
    let mut hasher = DefaultHasher::new();
    mp_abs.hash(&mut hasher);
    std::fs::create_dir_all(worktrees_root)
        .with_context(|| format!("creating worktrees root {}", worktrees_root.display()))?;
    let gd = worktrees_root.join(format!("projgit-wt-{:016x}.git", hasher.finish()));

    // Reuse an existing, valid worktree so committed work survives an
    // unmount / remount. Only recreate when it is absent or unreadable.
    if gd.join("HEAD").exists() {
        if let Some(head) = read_git_dir_head_commit(&gd) {
            return Ok(WritableGitDir {
                path: gd,
                commit: head,
                reused: true,
            });
        }
        // Present but unreadable — start clean.
        let _ = std::fs::remove_dir_all(&gd);
    }

    std::fs::create_dir_all(gd.join("refs/heads"))?;
    std::fs::create_dir_all(gd.join("refs/tags"))?;

    // Objects: symlink to the shared clone CAS so commits land in the
    // durable, shared store (readable by the projection, pushable).
    let clone_objects = std::fs::canonicalize(clone_git_dir.join("objects"))
        .with_context(|| format!("canonicalizing {}/objects", clone_git_dir.display()))?;
    std::os::unix::fs::symlink(&clone_objects, gd.join("objects")).with_context(|| {
        format!(
            "linking shared object store {} into {}",
            clone_objects.display(),
            gd.display()
        )
    })?;

    // HEAD: symbolic on a branch, or detached at the commit.
    match branch {
        Some(b) => {
            std::fs::write(gd.join("HEAD"), format!("ref: refs/heads/{b}\n"))?;
            let ref_path = gd.join("refs/heads").join(b);
            if let Some(parent) = ref_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(ref_path, format!("{commit_oid}\n"))?;
        }
        None => std::fs::write(gd.join("HEAD"), format!("{commit_oid}\n"))?,
    }
    // Import the source's branches + tags (as a packed-refs snapshot) so
    // `projgit checkout <ref>` can switch among them under the live mount;
    // their objects are already reachable via the object-store symlink.
    std::fs::write(gd.join("packed-refs"), import_source_refs(clone_git_dir))?;

    let mut config = format!(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n\tcheckStat = minimal\n\tworktree = {}\n",
        mp_abs.display()
    );
    if let Some(url) = remote_url {
        let _ = write!(
            config,
            "[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
        );
        if let Some(b) = branch {
            let _ = write!(
                config,
                "[branch \"{b}\"]\n\tremote = origin\n\tmerge = refs/heads/{b}\n"
            );
        }
    }
    std::fs::write(gd.join("config"), config)?;
    let index = projgit_core::dotgit::build_writable_index_bytes(store, commit_oid)
        .context("building writable index")?;
    std::fs::write(gd.join("index"), index)?;
    Ok(WritableGitDir {
        path: gd,
        commit: commit_oid,
        reused: false,
    })
}

/// The commit `HEAD` resolves to in an existing scratch git dir, or
/// `None` if the dir is missing / has no valid `HEAD`. Uses `git
/// rev-parse` so loose, symbolic, and packed refs all resolve.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_git_dir_head_commit(git_dir: &Path) -> Option<gix::ObjectId> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let hex = String::from_utf8_lossy(&out.stdout);
    gix::ObjectId::from_hex(hex.trim().as_bytes()).ok()
}

/// Resolve a git dir's `HEAD` commit by reading files directly (no `git`
/// subprocess) — for the hot HEAD-watcher poll loop. Handles a symbolic
/// `HEAD` (loose ref, then `packed-refs`) and a detached `HEAD`. Returns
/// `None` if it can't parse (the watcher treats that as "no change").
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve_head_commit_fast(git_dir: &Path) -> Option<gix::ObjectId> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let refname = match head.strip_prefix("ref: ") {
        Some(r) => r.trim(),
        // Detached HEAD: the file is the commit oid.
        None => return gix::ObjectId::from_hex(head.as_bytes()).ok(),
    };
    // Loose ref file wins over packed-refs.
    if let Ok(s) = std::fs::read_to_string(git_dir.join(refname)) {
        if let Ok(oid) = gix::ObjectId::from_hex(s.trim().as_bytes()) {
            return Some(oid);
        }
    }
    // Fall back to packed-refs.
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    for line in packed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        if let Some((oid, name)) = line.split_once(' ') {
            if name.trim() == refname {
                return gix::ObjectId::from_hex(oid.trim().as_bytes()).ok();
            }
        }
    }
    None
}

/// Build a `packed-refs` snapshot of the source's branches + tags so a
/// writable scratch git dir can resolve any of them for
/// checkout-under-live-mount. Remote-tracking refs (`refs/remotes/origin/*`,
/// as a partial clone stores non-default branches) are mapped to local
/// `refs/heads/*` when a same-named local branch isn't already present.
/// Best effort: returns an empty file (header only) if `git` fails.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn import_source_refs(clone_git_dir: &Path) -> String {
    use std::collections::HashSet;
    use std::fmt::Write as _;

    let mut packed = String::from("# pack-refs with: peeled fully-peeled \n");
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(clone_git_dir)
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/tags",
            "refs/remotes/origin",
        ])
        .output();
    let Ok(out) = out else { return packed };
    if !out.status.success() {
        return packed;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut heads: HashSet<String> = HashSet::new();
    // Local branches + tags, verbatim.
    for line in text.lines() {
        if let Some((oid, refname)) = line.split_once(' ') {
            if refname.starts_with("refs/heads/") {
                heads.insert(refname.to_string());
                let _ = writeln!(packed, "{oid} {refname}");
            } else if refname.starts_with("refs/tags/") {
                let _ = writeln!(packed, "{oid} {refname}");
            }
        }
    }
    // Remote-tracking branches -> local branches when not already present.
    for line in text.lines() {
        if let Some((oid, refname)) = line.split_once(' ') {
            if let Some(branch) = refname.strip_prefix("refs/remotes/origin/") {
                if branch == "HEAD" {
                    continue;
                }
                let head = format!("refs/heads/{branch}");
                if heads.insert(head.clone()) {
                    let _ = writeln!(packed, "{oid} {head}");
                }
            }
        }
    }
    packed
}

/// The branch a writable mount should commit onto, or `None` for a
/// detached HEAD. `--commit <oid>` is always detached; an explicit
/// `--ref <branch>` uses that branch; otherwise the clone's default
/// branch (its `HEAD` symref) is used.
fn writable_branch_name(args: &MountArgs, git_dir: &Path) -> Option<String> {
    if args.commit.is_some() {
        return None;
    }
    if let Some(r) = &args.r#ref {
        let name = r.strip_prefix("refs/heads/").unwrap_or(r);
        if name != "HEAD" {
            return Some(name.to_string());
        }
    }
    // Default / `HEAD`: read the clone's HEAD symref for its default branch.
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.strip_prefix("ref: refs/heads/")
        .map(|s| s.trim().to_string())
}

/// The clone's configured `remote.origin.url`, if any — so a writable
/// mount can `push` to the same place the source came from.
fn read_clone_remote_url(git_dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Apply A2 ref visibility to `overlay` when `projection` is a
/// `Projection::Ref` that resolves to a local branch. No-op for
/// `Commit`, `Subtree`, tag refs, or refs that don't exist.
///
/// Factored out of [`build_root_overlay`] / [`build_overlay_no_subtree`]
/// so both single-mount and multi-mount call sites apply the same
/// rule with one definition.
fn apply_a2_if_branch(
    overlay: &mut projgit_core::RootOverlay,
    projection: &projgit_core::Projection,
    store: &projgit_core::ObjectStore,
    commit_oid: gix::ObjectId,
) {
    use projgit_core::{dotgit, Projection};
    if let Projection::Ref(name) = projection {
        if let Some(full) = store.try_resolve_branch_full_name(name) {
            dotgit::apply_a2_ref_visibility(overlay, &full, commit_oid);
        }
    }
}

/// Default cache root. Honours `XDG_CACHE_HOME`; falls back to
/// `$HOME/.cache/projgit`.
fn resolve_cache_dir(explicit: Option<&std::path::Path>) -> Result<PathBuf> {
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

/// Stable, filesystem-safe directory name for a clone of `url`.
///
/// Uses `<basename>-<short_hash>`: the basename keeps the path
/// human-readable, the hash prevents collisions between different
/// remotes that share a basename (e.g. forks of `bar`).
///
/// The hash is computed over a **normalized** form of the URL so that
/// trivial variations (`https://host/repo`, `https://host/repo.git`,
/// `https://host/repo/`, case-only differences in scheme or host) all
/// resolve to the same cache directory. This is what the README means
/// by "one on-disk object store is shared across every mount": two
/// `projgit mount` invocations of the same conceptual remote share
/// the partial-clone cache and the projected blobs hydrated into it.
fn cache_subdir_for_url(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let canonical = canonicalize_url_for_hash(url);
    let mut h = DefaultHasher::new();
    canonical.hash(&mut h);
    let short = format!("{:016x}", h.finish());

    let basename = canonical
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or("repo")
        .trim_end_matches(".git");

    let safe: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();

    format!("{safe}-{short}")
}

/// Normalize a remote URL for cache-key hashing.
///
/// Rules (conservative — only collapse forms that demonstrably point at
/// the same upstream):
/// - Trim surrounding whitespace.
/// - Lowercase the scheme + authority (URI spec: case-insensitive).
///   The path is left case-sensitive because git server semantics vary.
/// - Strip a single trailing `/`.
/// - Strip a single trailing `.git`.
///
/// We deliberately do **not** unify SSH and HTTPS forms: they have
/// different auth contexts and a user who picked one over the other
/// likely meant it. We also do not strip default ports or query
/// strings; current sources for projgit don't carry them.
fn canonicalize_url_for_hash(url: &str) -> String {
    let trimmed = url.trim();
    let lowered = if let Some(idx) = trimmed.find("://") {
        // `scheme://authority/path` — lowercase up to and including
        // the authority, leave the path alone.
        let after_scheme = idx + 3;
        let path_start = trimmed[after_scheme..]
            .find('/')
            .map(|p| after_scheme + p)
            .unwrap_or(trimmed.len());
        let mut s = String::with_capacity(trimmed.len());
        s.push_str(&trimmed[..path_start].to_ascii_lowercase());
        s.push_str(&trimmed[path_start..]);
        s
    } else if let Some(colon) = trimmed.find(':') {
        // SSH short form `user@host:path` — lowercase up to and
        // including the colon.
        let mut s = String::with_capacity(trimmed.len());
        s.push_str(&trimmed[..=colon].to_ascii_lowercase());
        s.push_str(&trimmed[colon + 1..]);
        s
    } else {
        trimmed.to_owned()
    };
    let stripped = lowered.trim_end_matches('/');
    let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
    stripped.to_owned()
}

// `tests` for the helpers above. The `attach` subcommand helpers
// further down were added after this module existed; allow the
// resulting clippy lint rather than reorder the file.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn url_detection() {
        assert!(looks_like_url("https://github.com/foo/bar"));
        assert!(looks_like_url("http://example.com/x"));
        assert!(looks_like_url("git://example.com/x"));
        assert!(looks_like_url("ssh://git@example.com/x"));
        assert!(looks_like_url("git@github.com:foo/bar.git"));
        assert!(!looks_like_url("/abs/path"));
        assert!(!looks_like_url("./rel/path"));
        assert!(!looks_like_url("repo"));
    }

    #[test]
    fn cache_subdir_is_stable_and_disambiguates() {
        let a = cache_subdir_for_url("https://github.com/foo/bar.git");
        let b = cache_subdir_for_url("https://github.com/foo/bar.git");
        assert_eq!(a, b, "same input must produce same subdir");

        let c = cache_subdir_for_url("https://gitlab.com/foo/bar.git");
        assert_ne!(a, c, "different URLs with same basename must differ");

        assert!(a.starts_with("bar-"), "should keep the basename: {a}");
    }

    #[test]
    fn cache_subdir_handles_ssh_and_no_dotgit() {
        let s = cache_subdir_for_url("git@github.com:foo/bar");
        assert!(s.starts_with("bar-"), "ssh form: {s}");

        let s = cache_subdir_for_url("https://example.com/some-repo");
        assert!(s.starts_with("some-repo-"), "no .git suffix: {s}");
    }

    #[test]
    fn cache_subdir_sanitizes_unsafe_chars() {
        // Pathological basename should still be filesystem-safe.
        let s = cache_subdir_for_url("https://example.com/weird name.git");
        let prefix = s.split('-').next().unwrap();
        assert!(
            prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "basename portion must be sanitized: {s}",
        );
    }

    #[test]
    fn cache_subdir_dedupes_equivalent_urls() {
        // Trivial URL variations that point at the same upstream must
        // hash to the same cache subdir so the shared on-disk CAS is
        // actually shared. (Audit finding D2.)
        let canon = cache_subdir_for_url("https://github.com/foo/bar");
        for equiv in [
            "https://github.com/foo/bar",
            "https://github.com/foo/bar.git",
            "https://github.com/foo/bar/",
            "https://github.com/foo/bar.git/",
            "  https://github.com/foo/bar  ",
            "HTTPS://GitHub.com/foo/bar",
            "https://GITHUB.COM/foo/bar.git",
        ] {
            assert_eq!(
                cache_subdir_for_url(equiv),
                canon,
                "expected `{equiv}` to share a cache dir with the canonical form"
            );
        }
    }

    #[test]
    fn cache_subdir_keeps_distinct_protocols_separate() {
        // HTTPS and SSH are different access protocols (different auth
        // contexts); a user who picked one likely meant it. Don't
        // collapse them even though they reach the same upstream.
        let https = cache_subdir_for_url("https://github.com/foo/bar");
        let ssh = cache_subdir_for_url("git@github.com:foo/bar");
        let sshlong = cache_subdir_for_url("ssh://git@github.com/foo/bar");
        assert_ne!(https, ssh);
        assert_ne!(https, sshlong);
    }
}

// ---------------------------------------------------------------------------
// `attach` (Stage 2c of the projgitd plan)
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmd_attach(args: AttachArgs) -> Result<()> {
    use projgit_daemon::protocol::{read_message, write_message, Request, Response};
    use std::os::unix::net::UnixStream;

    let socket = args.socket.unwrap_or_else(default_daemon_socket);
    let request = match args.op {
        AttachOp::Ping => Request::Ping,
        AttachOp::Status => Request::Status,
        AttachOp::Shutdown => Request::Shutdown,
        AttachOp::Mount {
            source,
            ref_name,
            mountpoint,
            no_dotgit,
            allow_other,
        } => Request::Mount {
            source,
            ref_name,
            mountpoint,
            no_dotgit,
            allow_other,
        },
        AttachOp::Umount { mountpoint } => Request::Umount { mountpoint },
    };

    let mut stream = UnixStream::connect(&socket).with_context(|| {
        format!(
            "connecting to projgitd socket at {} (is the daemon running?)",
            socket.display()
        )
    })?;
    write_message(&mut stream, &request).context("write request")?;
    let response: Response = read_message(&mut stream).context("read response")?;

    print_response(&response);
    match response {
        Response::Err { code, .. } => bail!("daemon returned error: {code}"),
        _ => Ok(()),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn default_daemon_socket() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("projgitd.sock");
    }
    let uid = nix::unistd::geteuid().as_raw();
    PathBuf::from(format!("/tmp/projgitd-{uid}.sock"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn print_response(r: &projgit_daemon::protocol::Response) {
    use projgit_daemon::protocol::Response;
    match r {
        Response::Pong => println!("pong"),
        Response::Ok => println!("ok"),
        Response::Status(s) => {
            println!("uptime    : {}s", s.uptime_secs);
            match &s.source {
                Some(src) => println!("source    : {src}"),
                None => println!("source    : (no Mount request yet)"),
            }
            println!("mounts    : {}", s.mounts.len());
            for m in &s.mounts {
                println!(
                    "  [{}] {} -> {}",
                    m.projection_id,
                    m.ref_name,
                    m.mountpoint.display()
                );
            }
            if let Some(c) = &s.cache {
                println!(
                    "tree cache  hits={} misses={}",
                    c.tree_hits, c.tree_misses
                );
                println!(
                    "header cache hits={} misses={}",
                    c.header_hits, c.header_misses
                );
                println!(
                    "blob cache   hits={} misses={}",
                    c.blob_hits, c.blob_misses
                );
            }
        }
        Response::Err { code, message } => {
            eprintln!("err: {code}: {message}");
        }
        // Attach / HeaderProbes are emitted by the new Stage 3
        // RPCs (Attach / PrefetchHeaders). The `attach` CLI
        // doesn't expose those today — they're driven by the
        // sidecar's DaemonFetcher — but a future debug subcommand
        // (or just a user poking around with `socat`) might. Print
        // a one-line summary so the response isn't silently
        // swallowed.
        Response::Attached { git_dir } => {
            println!("attached  : {}", git_dir.display());
        }
        Response::HeaderProbes { probes } => {
            println!("probes    : {}", probes.len());
            for p in probes {
                use projgit_daemon::protocol::HeaderProbeWire;
                match p {
                    HeaderProbeWire::Present { oid } => println!("  present {oid}"),
                    HeaderProbeWire::PresentWithHeader {
                        oid,
                        object_kind,
                        size,
                    } => println!("  present {oid} ({object_kind}, {size}B)"),
                    HeaderProbeWire::HeaderOnly {
                        oid,
                        object_kind,
                        size,
                    } => println!("  header-only {oid} ({object_kind}, {size}B)"),
                    HeaderProbeWire::Error { oid, code, message } => {
                        println!("  error   {oid} ({code}): {message}")
                    }
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmd_attach(_args: AttachArgs) -> Result<()> {
    bail!(
        "`projgit attach` requires the daemon, which is Linux/macOS only today. \
         Windows support is deferred to the planned projgit-winfsp backend."
    );
}
