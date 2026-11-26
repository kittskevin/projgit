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

#![cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
#![forbid(unsafe_code)]

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Attribution notice required by the WinFsp FLOSS Exception in user-facing
/// interfaces and documentation.
const WINFSP_FLOSS_HELP: &str = "On Windows, filesystem mounting uses WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos (https://github.com/winfsp/winfsp) under the WinFsp FLOSS Exception.";

#[derive(Debug, Parser)]
#[command(
    name = "projgit",
    version = projgit_core::VERSION,
    about = "Lazy-fetch git projections as read-only filesystem mounts.",
    long_about = None,
    after_help = WINFSP_FLOSS_HELP,
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
    /// Serve a projection over a Linux `vhost-user-fs` Unix domain socket
    /// for `qemu` / `cloud-hypervisor` (`virtio-fs` + `DAX`).
    #[cfg(target_os = "linux")]
    Virtiofs(VirtiofsArgs),
}

#[derive(Debug, clap::Args)]
#[command(after_help = WINFSP_FLOSS_HELP)]
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

    /// Synthesize an isolated `.git/` directory without
    /// `objects/info/alternates`.
    ///
    /// Instead of writing a host filesystem path into
    /// `.git/objects/info/alternates`, projects `<git-dir>/objects/pack/*`
    /// and `<git-dir>/objects/info/{commit-graph,packs,commit-graphs/*}`
    /// directly under `.git/objects/` as read-only host-backed files.
    /// Designed for microVM / `virtio-fs` mounts where host paths do
    /// not exist inside the guest and must not leak across the VM
    /// boundary.
    #[arg(long, conflicts_with = "no_dotgit")]
    isolated_dotgit: bool,

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

    /// Stack a writable Linux `overlayfs` scratch layer on top of the
    /// read-only projection (`upperdir=<DIR>/upper,workdir=<DIR>/work,metacopy=on`).
    ///
    /// When set, `projgit` mounts the read-only FUSE projection at
    /// `<DIR>/lower` and stacks kernel `overlayfs` onto `mountpoint`.
    /// Metadata mutations (`chmod`, `rename`) use `metacopy=on` and
    /// data mutations copy-up into `<DIR>/upper` without touching the
    /// shared CAS.
    #[arg(long, value_name = "DIR")]
    writable_scratch: Option<PathBuf>,

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

    /// Apply to every projection: synthesize an isolated `.git/`
    /// directory (no `objects/info/alternates`; packfiles and
    /// commit-graph metadata projected directly under `.git/objects/`).
    /// See `mount --isolated-dotgit`.
    #[arg(long, conflicts_with = "no_dotgit")]
    isolated_dotgit: bool,

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
        /// Synthesise an isolated `.git/` overlay without `objects/info/alternates`.
        #[arg(long, conflicts_with = "no_dotgit")]
        isolated_dotgit: bool,
        /// Set `allow_other` on the FUSE mount.
        #[arg(long)]
        allow_other: bool,
        /// Stack a writable Linux `overlayfs` scratch layer (`<DIR>/{lower,upper,work}`).
        #[arg(long, value_name = "DIR")]
        writable_scratch: Option<PathBuf>,
    },
    /// Ask the daemon to unmount a previously-mounted projection.
    Umount {
        /// Mountpoint passed to the prior `mount` request.
        #[arg(long, value_name = "PATH")]
        mountpoint: PathBuf,
    },
}

#[cfg(target_os = "linux")]
#[derive(Debug, clap::Args)]
struct VirtiofsArgs {
    /// Source repository: a URL (https://, git://, ssh://, git@...) or
    /// a path to a local checkout / bare repository.
    source: String,

    /// Path to the `vhost-user-fs` Unix domain socket to bind.
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,

    /// `virtio-fs` filesystem tag advertised in `virtio_fs_config`
    /// (`mount -t virtiofs <TAG> <MOUNTPOINT>` inside the guest).
    #[arg(long, default_value = projgit_virtiofs::vhost_user::DEFAULT_TAG)]
    tag: String,

    /// Project the tip of this ref (short or full name; `HEAD` works).
    /// Mutually exclusive with `--commit`. Defaults to `HEAD`.
    #[arg(long, conflicts_with = "commit")]
    r#ref: Option<String>,

    /// Project this specific commit (full hex OID).
    #[arg(long)]
    commit: Option<String>,

    /// Project only this `/`-separated subdirectory of the projection.
    #[arg(long)]
    subtree: Option<String>,

    /// Where to keep clones of remote repositories. Only used when
    /// `source` is a URL.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Pass `--depth=N` to `git clone` when partial-cloning a URL source.
    #[arg(long, value_name = "N")]
    depth: Option<u32>,

    /// Remote name to fetch from (default `origin`).
    #[arg(long, default_value = "origin")]
    remote: String,

    /// Skip on-demand fetching.
    #[arg(long)]
    offline: bool,

    /// On shutdown, print cache and prefetch statistics.
    #[arg(long)]
    stats: bool,

    /// Don't synthesize a `.git/` directory at the projection root.
    #[arg(long)]
    no_dotgit: bool,

    /// Synthesize an isolated `.git/` directory without `objects/info/alternates`
    /// (enabled by default for `projgit virtiofs` unless `--no-dotgit` is set).
    #[arg(long, conflicts_with = "no_dotgit")]
    isolated_dotgit: bool,

    /// Fetch backend for URL-backed mounts.
    #[arg(long, value_enum, default_value = "git")]
    fetcher: FetcherChoice,

    /// Base GVFS protocol URL, with or without a trailing `/gvfs`.
    /// Required when `--fetcher gvfs` is selected.
    #[cfg(feature = "gvfs-fetcher")]
    #[arg(long, value_name = "URL")]
    gvfs_url: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match cli.command {
        Command::Mount(args) => cmd_mount(args),
        Command::MountMulti(args) => cmd_mount_multi(args),
        Command::Attach(args) => cmd_attach(args),
        #[cfg(target_os = "linux")]
        Command::Virtiofs(args) => cmd_virtiofs(args),
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
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
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
    // killing the mount.
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
    let projection = build_projection(&args, &store)?;

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
    let overlay = build_root_overlay(&args, &projection, &store, &git_dir)?;
    let mut cfg = MountConfig::default();
    if args.allow_other {
        cfg.acl = projgit_fuse::SessionACL::All;
    }
    cfg.writable_scratch = args.writable_scratch.clone();
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
        run_mount(provider, &mp, &cfg, print_stats)
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
                run_mount(provider, &mp, &cfg, print_stats)
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
                run_mount(provider, &mp, &cfg, print_stats)
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
    let projection = build_projection(&args, &store)?;

    // 4. DaemonFetcher coordinates cold-path hydration with the
    //    daemon. Warm paths read straight from the shared CAS and
    //    never touch the socket.
    let fetcher = DaemonFetcher::new(socket.clone());
    let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), fetcher));

    // Eager-tree warm in the background (sidecar path) so the first
    // os.walk is network-free; tree-fetches route through the daemon
    // via FetchMany. Mirrors the daemon's handle_mount warm.
    spawn_tree_warm(&hydrating, &projection);

    let overlay = build_root_overlay(&args, &projection, &store, &git_dir)?;
    let mut cfg = MountConfig::default();
    if args.allow_other {
        cfg.acl = projgit_fuse::SessionACL::All;
    }
    cfg.writable_scratch = args.writable_scratch.clone();
    let mp = args.mountpoint.clone();
    let print_stats = args.stats;

    let provider = Arc::new(
        ProjectionFsProvider::new(projection, hydrating, overlay, /* projection_id */ 1)
            .context("building ProjectionFsProvider")?,
    );
    run_mount(provider, &mp, &cfg, print_stats)
}

#[cfg(target_os = "windows")]
fn cmd_mount(args: MountArgs) -> Result<()> {
    #[cfg(feature = "gvfs-fetcher")]
    use projgit_core::GvfsFetcher;
    use projgit_core::{
        clone::{git_dir_for, partial_clone, CloneOptions},
        GitCliFetcher, HydratingObjectStore, NoopFetcher, ObjectStore, ProjectionFsProvider,
    };
    use projgit_winfsp::MountConfig;
    use std::sync::Arc;

    if args.daemon_socket.is_some() {
        bail!("--daemon-socket is only supported on Linux and macOS (`projgitd` Unix socket)");
    }
    if args.writable_scratch.is_some() {
        bail!("--writable-scratch is only supported on Linux (`overlayfs`)");
    }
    if args.mountpoint.exists() && !args.mountpoint.is_dir() {
        bail!(
            "mountpoint {} exists and is not a directory",
            args.mountpoint.display()
        );
    }

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

    let store = Arc::new(
        ObjectStore::open(&git_dir)
            .with_context(|| format!("opening object store at {}", git_dir.display()))?,
    );
    let projection = build_projection(&args, &store)?;
    let _remote_hint = &args.remote;
    let overlay = build_root_overlay(&args, &projection, &store, &git_dir)?;
    let cfg = MountConfig::default();
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
        run_mount_windows(provider, &mp, &cfg, print_stats)
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
                run_mount_windows(provider, &mp, &cfg, print_stats)
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
                run_mount_windows(provider, &mp, &cfg, print_stats)
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn run_mount_windows<F>(
    provider: std::sync::Arc<projgit_core::ProjectionFsProvider<F>>,
    mountpoint: &std::path::Path,
    config: &projgit_winfsp::MountConfig,
    print_stats: bool,
) -> Result<()>
where
    F: projgit_core::Fetcher + 'static,
{
    eprintln!(
        "projgit: mounting at {} via WinFsp (Ctrl-C to unmount)",
        mountpoint.display()
    );
    let session = projgit_winfsp::mount_background(provider.clone(), mountpoint, config)
        .with_context(|| format!("mounting at {} via WinFsp", mountpoint.display()))?;

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("installing Ctrl-C handler")?;
    rx.recv().ok();

    let winfsp_stats = session.read_stats();
    eprintln!("projgit: unmounting…");
    session.unmount();

    if print_stats {
        let store = provider.store().store();
        let t = store.tree_cache_stats();
        let h = store.header_cache_stats();
        let dh = store.disk_header_cache_stats();
        let b = store.blob_cache_stats();
        let d = store.disk_blob_cache_stats();
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
            "projgit: disk headers  hits={} misses={} inserts={} entries={} write_errors={}",
            dh.hits, dh.misses, dh.inserts, dh.entries, dh.write_errors,
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
            "projgit: disk blobs    hits={} misses={} inserts={} write_errors={}",
            d.hits, d.misses, d.inserts, d.write_errors,
        );
        eprintln!(
            "projgit: winfsp reads  host_file_reads={} provider_reads={}",
            winfsp_stats.host_file_reads, winfsp_stats.provider_reads,
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

#[cfg(target_os = "windows")]
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
    let mut seen = std::collections::HashSet::new();
    for spec in &args.mounts {
        if spec.mountpoint.exists() && !spec.mountpoint.is_dir() {
            bail!(
                "mountpoint {} exists and is not a directory",
                spec.mountpoint.display()
            );
        }
        if !seen.insert(spec.mountpoint.clone()) {
            bail!(
                "mountpoint {} listed more than once",
                spec.mountpoint.display()
            );
        }
    }

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

    let store = Arc::new(
        ObjectStore::open(&git_dir)
            .with_context(|| format!("opening object store at {}", git_dir.display()))?,
    );

    let _remote_hint = &args.remote;
    if args.offline || !source_is_url {
        if args.fetcher != FetcherChoice::Git {
            bail!("--fetcher gvfs requires a URL source and cannot be combined with --offline");
        }
        let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), NoopFetcher));
        run_mount_multi_windows(store, hydrating, &args, &git_dir)
    } else {
        match args.fetcher {
            FetcherChoice::Git => {
                let fetcher = GitCliFetcher::open(store.clone())
                    .context("opening GitCliFetcher (needs `git` on PATH)")?;
                let hydrating = Arc::new(HydratingObjectStore::new(store.clone(), fetcher));
                run_mount_multi_windows(store, hydrating, &args, &git_dir)
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
                run_mount_multi_windows(store, hydrating, &args, &git_dir)
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn run_mount_multi_windows<F>(
    store: std::sync::Arc<projgit_core::ObjectStore>,
    hydrating: std::sync::Arc<projgit_core::HydratingObjectStore<F>>,
    args: &MountMultiArgs,
    git_dir: &Path,
) -> Result<()>
where
    F: projgit_core::Fetcher + 'static,
{
    use projgit_core::{Projection, ProjectionFsProvider};
    use projgit_winfsp::{MountConfig, WinFspMountSession};
    use std::sync::Arc;

    let cfg = MountConfig::default();
    let mut sessions: Vec<WinFspMountSession<ProjectionFsProvider<F>>> =
        Vec::with_capacity(args.mounts.len());
    let mut providers: Vec<Arc<ProjectionFsProvider<F>>> = Vec::with_capacity(args.mounts.len());

    for (idx, spec) in args.mounts.iter().enumerate() {
        let projection_id = (idx + 1) as u64;
        let projection = Projection::Ref(spec.ref_name.clone());
        let overlay = build_overlay_no_subtree(
            args.no_dotgit,
            args.isolated_dotgit,
            &projection,
            &store,
            git_dir,
        )
        .with_context(|| format!("building overlay for {}", spec.ref_name))?;
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
            "projgit: mounting {} at {} via WinFsp (projection_id={})",
            spec.ref_name,
            spec.mountpoint.display(),
            projection_id
        );
        let session = projgit_winfsp::mount_background(provider.clone(), &spec.mountpoint, &cfg)
            .with_context(|| format!("mounting at {} via WinFsp", spec.mountpoint.display()))?;
        sessions.push(session);
        providers.push(provider);
    }

    eprintln!(
        "projgit: {} projection(s) mounted via WinFsp; Ctrl-C to unmount all",
        sessions.len()
    );

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("installing Ctrl-C handler")?;
    rx.recv().ok();

    eprintln!("projgit: unmounting {} session(s)…", sessions.len());
    drop(sessions);

    if args.stats {
        let s = store.as_ref();
        let t = s.tree_cache_stats();
        let h = s.header_cache_stats();
        let dh = s.disk_header_cache_stats();
        let b = s.blob_cache_stats();
        let d = s.disk_blob_cache_stats();
        eprintln!(
            "projgit: tree cache    hits={} misses={} inserts={} evictions={} len={}/{}",
            t.hits, t.misses, t.inserts, t.evictions, t.len, t.capacity,
        );
        eprintln!(
            "projgit: header cache  hits={} misses={} inserts={} evictions={} len={}/{}",
            h.hits, h.misses, h.inserts, h.evictions, h.len, h.capacity,
        );
        eprintln!(
            "projgit: disk headers  hits={} misses={} inserts={} entries={} write_errors={}",
            dh.hits, dh.misses, dh.inserts, dh.entries, dh.write_errors,
        );
        eprintln!(
            "projgit: blob cache    hits={} misses={} inserts={} evictions={} skipped_too_large={} bytes={}/{}",
            b.hits, b.misses, b.inserts, b.evictions, b.skipped_too_large,
            b.bytes_used, b.capacity_bytes,
        );
        eprintln!(
            "projgit: disk blobs    hits={} misses={} inserts={} write_errors={}",
            d.hits, d.misses, d.inserts, d.write_errors,
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

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn cmd_mount(_args: MountArgs) -> Result<()> {
    bail!("`projgit mount` is only supported on Linux, macOS, and Windows.");
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn cmd_mount_multi(_args: MountMultiArgs) -> Result<()> {
    bail!("`projgit mount-multi` is only supported on Linux, macOS, and Windows.");
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
        let canon = std::fs::canonicalize(&spec.mountpoint)
            .with_context(|| format!("canonicalizing mountpoint {}", spec.mountpoint.display()))?;
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
        let overlay = build_overlay_no_subtree(
            args.no_dotgit,
            args.isolated_dotgit,
            &projection,
            &store,
            git_dir,
        )
        .with_context(|| format!("building overlay for {}", spec.ref_name))?;
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
        let dh = s.disk_header_cache_stats();
        let b = s.blob_cache_stats();
        let d = s.disk_blob_cache_stats();
        eprintln!(
            "projgit: tree cache    hits={} misses={} inserts={} evictions={} len={}/{}",
            t.hits, t.misses, t.inserts, t.evictions, t.len, t.capacity,
        );
        eprintln!(
            "projgit: header cache  hits={} misses={} inserts={} evictions={} len={}/{}",
            h.hits, h.misses, h.inserts, h.evictions, h.len, h.capacity,
        );
        eprintln!(
            "projgit: disk headers  hits={} misses={} inserts={} entries={} write_errors={}",
            dh.hits, dh.misses, dh.inserts, dh.entries, dh.write_errors,
        );
        eprintln!(
            "projgit: blob cache    hits={} misses={} inserts={} evictions={} skipped_too_large={} bytes={}/{}",
            b.hits, b.misses, b.inserts, b.evictions, b.skipped_too_large,
            b.bytes_used, b.capacity_bytes,
        );
        eprintln!(
            "projgit: disk blobs    hits={} misses={} inserts={} write_errors={}",
            d.hits, d.misses, d.inserts, d.write_errors,
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
/// by default (or isolated A1+ when `isolated_dotgit` is set), plus A2
/// ref visibility when the projection is a branch; empty overlay if
/// `no_dotgit` or `Projection::Subtree` (the latter unreachable today
/// since `mount-multi` only accepts refs, but the guard stays for the
/// future).
fn build_overlay_no_subtree(
    no_dotgit: bool,
    isolated_dotgit: bool,
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
    let mut overlay = if isolated_dotgit {
        dotgit::a1_plus_isolated_overlay(store, commit_oid)
            .context("building isolated A1+ overlay")?
    } else {
        let objects_dir = git_dir.join("objects");
        let objects_dir = std::fs::canonicalize(&objects_dir)
            .with_context(|| format!("canonicalizing {}", objects_dir.display()))?;
        dotgit::a1_plus_overlay(store, commit_oid, &objects_dir).context("building A1+ overlay")?
    };
    apply_a2_if_branch(&mut overlay, projection, store, commit_oid);
    Ok(overlay)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_mount<F>(
    provider: std::sync::Arc<projgit_core::ProjectionFsProvider<F>>,
    mountpoint: &std::path::Path,
    config: &projgit_fuse::MountConfig,
    print_stats: bool,
) -> Result<()>
where
    F: projgit_core::Fetcher + 'static,
{
    eprintln!(
        "projgit: mounting at {} (Ctrl-C to unmount)",
        mountpoint.display()
    );
    let session = projgit_fuse::mount_managed_background(provider.clone(), mountpoint, config)
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;

    // Park the main thread on a Ctrl-C / SIGTERM. Dropping `session`
    // synchronously unmounts any stacked overlayfs and the underlying FUSE session.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("installing Ctrl-C handler")?;
    rx.recv().ok();

    eprintln!("projgit: unmounting…");
    drop(session);

    if print_stats {
        let store = provider.store().store();
        let t = store.tree_cache_stats();
        let h = store.header_cache_stats();
        let dh = store.disk_header_cache_stats();
        let b = store.blob_cache_stats();
        let d = store.disk_blob_cache_stats();
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
            "projgit: disk headers  hits={} misses={} inserts={} entries={} write_errors={}",
            dh.hits, dh.misses, dh.inserts, dh.entries, dh.write_errors,
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
            "projgit: disk blobs    hits={} misses={} inserts={} write_errors={}",
            d.hits, d.misses, d.inserts, d.write_errors,
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

// ---------------------------------------------------------------------------
// `virtiofs` (Linux `vhost-user-fs` + `DAX` backend)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn cmd_virtiofs(args: VirtiofsArgs) -> Result<()> {
    #[cfg(feature = "gvfs-fetcher")]
    use projgit_core::GvfsFetcher;
    use projgit_core::{
        clone::{git_dir_for, partial_clone, CloneOptions},
        GitCliFetcher, HydratingObjectStore, NoopFetcher, ObjectStore, ProjectionFsProvider,
    };
    use std::sync::Arc;

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

    let store = Arc::new(
        ObjectStore::open(&git_dir)
            .with_context(|| format!("opening object store at {}", git_dir.display()))?,
    );

    let projection = build_projection_from_parts(
        args.r#ref.as_deref(),
        args.commit.as_deref(),
        args.subtree.as_deref(),
        &store,
    )?;

    // `projgit virtiofs` always uses the isolated `.git/` overlay (unless
    // `--no-dotgit` is set) because a guest microVM cannot resolve host
    // paths in `.git/objects/info/alternates`.
    let _explicit_isolated = args.isolated_dotgit;
    let _remote_hint = &args.remote;
    let overlay = build_overlay_no_subtree(args.no_dotgit, true, &projection, &store, &git_dir)?;

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
        run_virtiofs(provider, &args.socket, &args.tag, args.stats)
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
                run_virtiofs(provider, &args.socket, &args.tag, args.stats)
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
                run_virtiofs(provider, &args.socket, &args.tag, args.stats)
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_virtiofs<F>(
    provider: std::sync::Arc<projgit_core::ProjectionFsProvider<F>>,
    socket: &Path,
    tag: &str,
    print_stats: bool,
) -> Result<()>
where
    F: projgit_core::Fetcher + 'static,
{
    use projgit_virtiofs::dax::DEFAULT_DAX_WINDOW_SIZE;
    use projgit_virtiofs::vhost_user::{
        serve_vhost_user_socket, VhostUserFsBackend, DEFAULT_MAX_QUEUE_SIZE,
    };
    use std::sync::{Arc, RwLock};

    eprintln!(
        "projgit: serving vhost-user-fs at {} (tag={:?}, Ctrl-C or VMM disconnect to stop)",
        socket.display(),
        tag
    );

    let backend = Arc::new(RwLock::new(
        VhostUserFsBackend::with_config(
            provider.clone(),
            tag,
            DEFAULT_DAX_WINDOW_SIZE,
            DEFAULT_MAX_QUEUE_SIZE,
        )
        .context("initializing VhostUserFsBackend")?,
    ));

    serve_vhost_user_socket(socket, backend)
        .with_context(|| format!("serving vhost-user-fs socket {}", socket.display()))?;

    if print_stats {
        let store = provider.store().store();
        let t = store.tree_cache_stats();
        let h = store.header_cache_stats();
        let dh = store.disk_header_cache_stats();
        let b = store.blob_cache_stats();
        let d = store.disk_blob_cache_stats();
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
            "projgit: disk headers  hits={} misses={} inserts={} entries={} write_errors={}",
            dh.hits, dh.misses, dh.inserts, dh.entries, dh.write_errors,
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
            "projgit: disk blobs    hits={} misses={} inserts={} write_errors={}",
            d.hits, d.misses, d.inserts, d.write_errors,
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

    eprintln!("projgit: vhost-user-fs stopped.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_projection(
    args: &MountArgs,
    store: &projgit_core::ObjectStore,
) -> Result<projgit_core::Projection> {
    build_projection_from_parts(
        args.r#ref.as_deref(),
        args.commit.as_deref(),
        args.subtree.as_deref(),
        store,
    )
}

fn build_projection_from_parts(
    ref_name: Option<&str>,
    commit_hex: Option<&str>,
    subtree: Option<&str>,
    store: &projgit_core::ObjectStore,
) -> Result<projgit_core::Projection> {
    use projgit_core::Projection;

    // Pick the base projection from the mutually-exclusive --ref / --commit.
    let base = match (ref_name, commit_hex) {
        (Some(_), Some(_)) => unreachable!("clap rejects this combination"),
        (None, Some(hex)) => {
            let oid = gix::ObjectId::from_hex(hex.as_bytes())
                .map_err(|e| anyhow!("invalid commit OID `{hex}`: {e}"))?;
            Projection::Commit(oid)
        }
        (Some(name), None) => Projection::Ref(name.to_owned()),
        (None, None) => Projection::Ref("HEAD".to_owned()),
    };

    // Fold in --subtree if present.
    let Some(path) = subtree else {
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
    build_overlay_no_subtree(
        args.no_dotgit,
        args.isolated_dotgit,
        projection,
        store,
        git_dir,
    )
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
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .ok_or_else(|| anyhow!("HOME/USERPROFILE not set; pass --cache-dir explicitly"))?;
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

    #[test]
    fn cli_isolated_dotgit_parses_and_conflicts_with_no_dotgit() {
        let cli = Cli::try_parse_from(["projgit", "mount", "/src", "/mnt", "--isolated-dotgit"])
            .expect("mount --isolated-dotgit should parse");
        match cli.command {
            Command::Mount(args) => {
                assert!(args.isolated_dotgit);
                assert!(!args.no_dotgit);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Conflict on `mount`
        assert!(Cli::try_parse_from([
            "projgit",
            "mount",
            "/src",
            "/mnt",
            "--isolated-dotgit",
            "--no-dotgit",
        ])
        .is_err());

        // `mount-multi --isolated-dotgit`
        let cli = Cli::try_parse_from([
            "projgit",
            "mount-multi",
            "/src",
            "--mount",
            "main=/mnt",
            "--isolated-dotgit",
        ])
        .expect("mount-multi --isolated-dotgit should parse");
        match cli.command {
            Command::MountMulti(args) => {
                assert!(args.isolated_dotgit);
                assert!(!args.no_dotgit);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Conflict on `mount-multi`
        assert!(Cli::try_parse_from([
            "projgit",
            "mount-multi",
            "/src",
            "--mount",
            "main=/mnt",
            "--isolated-dotgit",
            "--no-dotgit",
        ])
        .is_err());

        // `attach mount --isolated-dotgit`
        let cli = Cli::try_parse_from([
            "projgit",
            "attach",
            "mount",
            "/src",
            "--ref",
            "main",
            "--mountpoint",
            "/mnt",
            "--isolated-dotgit",
        ])
        .expect("attach mount --isolated-dotgit should parse");
        match cli.command {
            Command::Attach(AttachArgs {
                op:
                    AttachOp::Mount {
                        isolated_dotgit,
                        no_dotgit,
                        ..
                    },
                ..
            }) => {
                assert!(isolated_dotgit);
                assert!(!no_dotgit);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Conflict on `attach mount`
        assert!(Cli::try_parse_from([
            "projgit",
            "attach",
            "mount",
            "/src",
            "--ref",
            "main",
            "--mountpoint",
            "/mnt",
            "--isolated-dotgit",
            "--no-dotgit",
        ])
        .is_err());

        // `mount --writable-scratch`
        let cli = Cli::try_parse_from([
            "projgit",
            "mount",
            "/src",
            "/mnt",
            "--writable-scratch",
            "/tmp/scratch",
        ])
        .expect("mount --writable-scratch should parse");
        match cli.command {
            Command::Mount(args) => {
                assert_eq!(args.writable_scratch, Some(PathBuf::from("/tmp/scratch")));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // `attach mount --writable-scratch`
        let cli = Cli::try_parse_from([
            "projgit",
            "attach",
            "mount",
            "/src",
            "--ref",
            "main",
            "--mountpoint",
            "/mnt",
            "--writable-scratch",
            "/tmp/scratch-daemon",
        ])
        .expect("attach mount --writable-scratch should parse");
        match cli.command {
            Command::Attach(AttachArgs {
                op: AttachOp::Mount {
                    writable_scratch, ..
                },
                ..
            }) => {
                assert_eq!(writable_scratch, Some(PathBuf::from("/tmp/scratch-daemon")));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        #[cfg(target_os = "linux")]
        {
            let cli = Cli::try_parse_from([
                "projgit",
                "virtiofs",
                "/src",
                "--socket",
                "/tmp/projgit-vhost.sock",
                "--tag",
                "myrepo",
                "--ref",
                "main",
                "--isolated-dotgit",
                "--offline",
            ])
            .expect("virtiofs subcommand should parse");
            match cli.command {
                Command::Virtiofs(args) => {
                    assert_eq!(args.source, "/src");
                    assert_eq!(args.socket, PathBuf::from("/tmp/projgit-vhost.sock"));
                    assert_eq!(args.tag, "myrepo");
                    assert_eq!(args.r#ref.as_deref(), Some("main"));
                    assert!(args.isolated_dotgit);
                    assert!(args.offline);
                }
                other => panic!("unexpected command: {other:?}"),
            }

            assert!(Cli::try_parse_from([
                "projgit",
                "virtiofs",
                "/src",
                "--socket",
                "/tmp/projgit-vhost.sock",
                "--isolated-dotgit",
                "--no-dotgit",
            ])
            .is_err());
        }
    }

    #[test]
    fn build_overlay_isolated_dotgit_omits_alternates_and_applies_a2() {
        use projgit_core::{overlay::SyntheticEntry, ObjectStore, Projection};
        use std::{process::Command, sync::Arc};

        let base = std::env::temp_dir().join(format!(
            "projgit-cli-isolated-overlay-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let run_git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&base)
                .env("GIT_AUTHOR_NAME", "projgit-test")
                .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
                .env("GIT_COMMITTER_NAME", "projgit-test")
                .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        run_git(&["init", "-q", "-b", "main"]);
        std::fs::write(base.join("hello.txt"), b"isolated\n").unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "init"]);
        run_git(&["repack", "-a", "-d", "-q"]);

        let git_dir = base.join(".git");
        let store = Arc::new(ObjectStore::open(&git_dir).unwrap());
        let projection = Projection::Ref("main".into());

        let overlay = build_overlay_no_subtree(false, true, &projection, &store, &git_dir).unwrap();
        let dotgit = match overlay.get(b".git").expect(".git entry") {
            SyntheticEntry::Directory { children } => children,
            other => panic!("expected .git dir, got {other:?}"),
        };

        // A2 ref visibility: HEAD is symbolic `ref: refs/heads/main\n`.
        match dotgit.get(b"HEAD".as_slice()).expect("HEAD") {
            SyntheticEntry::File { content, .. } => {
                assert_eq!(content.as_slice(), b"ref: refs/heads/main\n");
            }
            other => panic!("expected HEAD file, got {other:?}"),
        }

        // .git/objects/info/alternates is omitted; .git/objects/pack/* has HostFile entries.
        let objects = match dotgit.get(b"objects".as_slice()).expect("objects") {
            SyntheticEntry::Directory { children } => children,
            other => panic!("expected objects dir, got {other:?}"),
        };
        let info = match objects.get(b"info".as_slice()).expect("info") {
            SyntheticEntry::Directory { children } => children,
            other => panic!("expected info dir, got {other:?}"),
        };
        assert!(
            !info.contains_key(b"alternates".as_slice()),
            "isolated_dotgit must omit .git/objects/info/alternates"
        );
        let pack = match objects.get(b"pack".as_slice()).expect("pack") {
            SyntheticEntry::Directory { children } => children,
            other => panic!("expected pack dir, got {other:?}"),
        };
        assert!(
            pack.values()
                .any(|entry| matches!(entry, SyntheticEntry::HostFile { .. })),
            "expected at least one HostFile pack entry under .git/objects/pack"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cli_help_includes_winfsp_floss_exception_notice() {
        use clap::CommandFactory;

        let mut root_cmd = Cli::command();
        let root_help = root_cmd.render_long_help().to_string();
        assert!(
            root_help
                .contains("WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos"),
            "root --help must include WinFsp copyright notice, got:\n{root_help}"
        );
        assert!(
            root_help.contains("https://github.com/winfsp/winfsp"),
            "root --help must include WinFsp URL, got:\n{root_help}"
        );

        let mount_cmd = root_cmd
            .find_subcommand_mut("mount")
            .expect("mount subcommand");
        let mount_help = mount_cmd.render_long_help().to_string();
        assert!(
            mount_help
                .contains("WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos"),
            "mount --help must include WinFsp copyright notice, got:\n{mount_help}"
        );
        assert!(
            mount_help.contains("https://github.com/winfsp/winfsp"),
            "mount --help must include WinFsp URL, got:\n{mount_help}"
        );
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
            isolated_dotgit,
            allow_other,
            writable_scratch,
        } => Request::Mount {
            source,
            ref_name,
            mountpoint,
            no_dotgit,
            isolated_dotgit,
            allow_other,
            writable_scratch,
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
                println!("tree cache  hits={} misses={}", c.tree_hits, c.tree_misses);
                println!(
                    "header cache hits={} misses={}",
                    c.header_hits, c.header_misses
                );
                println!("blob cache   hits={} misses={}", c.blob_hits, c.blob_misses);
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
