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

    /// Fetch backend for URL-backed mounts.
    #[arg(long, value_enum, default_value = "git")]
    fetcher: FetcherChoice,

    /// Base GVFS protocol URL, with or without a trailing `/gvfs`.
    /// Required when `--fetcher gvfs` is selected.
    #[cfg(feature = "gvfs-fetcher")]
    #[arg(long, value_name = "URL")]
    gvfs_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum FetcherChoice {
    /// Use system Git's partial-clone promisor path.
    Git,
    /// Use the optional GVFS protocol backend.
    #[cfg(feature = "gvfs-fetcher")]
    Gvfs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match cli.command {
        Command::Mount(args) => cmd_mount(args),
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

    // 1. Resolve `source` to a local git directory.
    let (git_dir, source_is_url) = if looks_like_url(&args.source) {
        let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
        let dest = cache_dir.join(cache_subdir_for_url(&args.source));
        if !dest.exists() {
            eprintln!(
                "projgit: partial-cloning {} into {}",
                args.source,
                dest.display()
            );
            std::fs::create_dir_all(&cache_dir)
                .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
            let opts = CloneOptions::new(args.source.clone(), dest.clone());
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
    let cfg = MountConfig::default();
    let mp = args.mountpoint.clone();
    let print_stats = args.stats;

    if args.offline || !source_is_url {
        if args.fetcher != FetcherChoice::Git {
            bail!("--fetcher gvfs requires a URL source and cannot be combined with --offline");
        }
        let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmd_mount(_args: MountArgs) -> Result<()> {
    bail!(
        "`projgit mount` is not yet supported on this platform. \
         Linux/macOS use FUSE (projgit-fuse); Windows support is \
         deferred to the planned projgit-winfsp backend."
    );
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
    let session = projgit_fuse::mount_background(provider.clone(), mountpoint, config)
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;

    // Park the main thread on a Ctrl-C / SIGTERM. Dropping `session`
    // synchronously unmounts via fuser's drop impl.
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
            "projgit: prefetch (T1) posted={} dropped={} batches={} resolved={} headers={} failed={}",
            p.posted,
            p.dropped,
            p.batches_sent,
            p.oids_resolved,
            p.headers_published,
            p.oids_failed,
        );
    }

    eprintln!("projgit: unmounted.");
    Ok(())
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
/// and the projection kind. Default behavior synthesizes an A1 `.git/`
/// pointing at the commit and the shared object store; `--no-dotgit` or
/// a `Subtree` projection yields an empty overlay.
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
    Ok(dotgit::a1_overlay(commit_oid, &objects_dir))
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

#[cfg(test)]
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
