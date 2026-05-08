//! `projgit` command-line entry point.
//!
//! Phase 4 ships exactly one subcommand — `mount` — wiring the pieces
//! Phase 1–3 produced into the first end-to-end demo:
//!
//! ```text
//! projgit mount https://github.com/foo/bar /mnt/bar          # clone + mount HEAD
//! projgit mount /path/to/repo /mnt/bar --commit <oid>        # local repo, specific commit
//! projgit mount https://… /mnt/bar --ref refs/tags/v1 \
//!                                  --subtree src
//! ```
//!
//! See `docs/initial-plan.md` Phase 4 for the broader command surface
//! (`init`, `clone`, `umount`, `ls`, `fetch`) that lands in subsequent
//! phases.

#![forbid(unsafe_code)]

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    /// Currently requires `--commit`; ref+subtree resolution is a
    /// Phase 5 enhancement.
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
    use projgit_core::{
        clone::{git_dir_for, partial_clone, CloneOptions},
        GitCliFetcher, HydratingObjectStore, NoopFetcher, ObjectStore, ProjectionFsProvider,
        RootOverlay,
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

    // 3. Build the projection.
    let projection = build_projection(&args)?;

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
    let overlay = RootOverlay::new();
    let cfg = MountConfig::default();
    let mp = args.mountpoint.clone();

    if args.offline || !source_is_url {
        let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
        let provider = Arc::new(
            ProjectionFsProvider::new(projection, hydrating, overlay, /* projection_id */ 1)
                .context("building ProjectionFsProvider")?,
        );
        run_mount(provider, &mp, &cfg)
    } else {
        let fetcher =
            GitCliFetcher::open(store.clone()).context("opening GitCliFetcher (needs `git` on PATH)")?;
        let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
        let provider = Arc::new(
            ProjectionFsProvider::new(projection, hydrating, overlay, /* projection_id */ 1)
                .context("building ProjectionFsProvider")?,
        );
        run_mount(provider, &mp, &cfg)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmd_mount(_args: MountArgs) -> Result<()> {
    bail!(
        "`projgit mount` is not yet supported on this platform. \
         Linux/macOS use FUSE (projgit-fuse); Windows support lands in \
         Phase 3d (projgit-winfsp)."
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_mount<F>(
    provider: std::sync::Arc<projgit_core::ProjectionFsProvider<F>>,
    mountpoint: &std::path::Path,
    config: &projgit_fuse::MountConfig,
) -> Result<()>
where
    F: projgit_core::Fetcher + 'static,
{
    eprintln!(
        "projgit: mounting at {} (Ctrl-C to unmount)",
        mountpoint.display()
    );
    let session = projgit_fuse::mount_background(provider, mountpoint, config)
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
    eprintln!("projgit: unmounted.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_projection(args: &MountArgs) -> Result<projgit_core::Projection> {
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
    if let Some(path) = &args.subtree {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(base);
        }
        let commit = match base {
            Projection::Commit(oid) => oid,
            Projection::Ref(_) => {
                // The `Subtree` variant requires a commit OID. We
                // don't have an `ObjectStore` here yet to peel the
                // ref, and threading that through this helper would
                // pull mount-side dependencies into projection
                // construction. Defer the convenience to Phase 5.
                bail!(
                    "`--subtree` currently requires `--commit`. \
                     Resolving a ref + subtree pair is a Phase 5 \
                     enhancement; pass --commit <oid> for now."
                );
            }
            Projection::Subtree { .. } => unreachable!("base is Ref or Commit"),
        };
        Ok(Projection::Subtree {
            commit,
            path: trimmed.to_owned(),
        })
    } else {
        Ok(base)
    }
}

fn looks_like_url(s: &str) -> bool {
    s.contains("://") || s.starts_with("git@")
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
fn cache_subdir_for_url(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    let short = format!("{:016x}", h.finish());

    let basename = url
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
}
