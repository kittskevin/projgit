//! Network-gated benchmark: time representative access patterns through
//! projgit vs. system git's partial-clone path.
//!
//! Runs against `https://github.com/rust-lang/log` by default. Override
//! with `--url`, `--ref`, `--files`, `--iterations`, `--scenario`.
//! Doubly gated: needs `git` on `PATH` and `PROJGIT_NETWORK_TESTS=1` set.
//!
//! Two scenarios:
//! - `--scenario single` (default): one mount, cold + warm passes. The
//!   shape captured in `docs/bench/baseline.md`.
//! - `--scenario sequential`: same as single, then mounts a *second*
//!   freshly-constructed `ObjectStore` against the same on-disk cache
//!   dir and re-measures cold cat. Falsifies (or confirms) the
//!   workload-doc §1.6 amortisation claim: "the first mount pays the
//!   network cost; every subsequent mount sees a warm hit." In-process
//!   caches are empty for mount 2 by construction (fresh
//!   `ObjectStore`/`Fetcher`/`Provider`); the on-disk CAS is warm from
//!   mount 1's cold reads. *Cross-process concurrent* mounts (the audit's
//!   Phase C) are deliberately NOT exercised here; see
//!   `docs/bench/baseline.md` for why.
//!
//! ```sh
//! PROJGIT_NETWORK_TESTS=1 \
//!   cargo run -p projgit-cli --example bench_mount --release
//! ```
//!
//! Cfg-gated to Linux + macOS because the projgit side mounts via
//! FUSE; on other targets the binary prints a friendly message.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "https://github.com/rust-lang/log";
const DEFAULT_REF: &str = "master";
const DEFAULT_FILES: &[&str] = &["Cargo.toml", "src/lib.rs", "LICENSE-APACHE"];
const DEFAULT_ITERATIONS: usize = 3;
const DEFAULT_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Single,
    Sequential,
    /// Phase C daemon arm: one in-thread `projgitd` + N sidecar
    /// threads holding `DaemonFetcher`, all cold-catting the same
    /// files at the same time. The daemon's in-flight coalescer
    /// (audit A3) is the architectural property under test.
    DaemonConcurrent,
    /// Phase C comparator arm: no daemon. N independent local
    /// mounts, each with its own `GitCliFetcher`, all pointing at
    /// the **same** on-disk cache dir. Models the actual A3
    /// scenario the daemon was built to fix — N consumers racing
    /// `git fetch` children into one `.git/objects/pack/` with no
    /// coordination. Failure mode is data: thread errors are
    /// counted, not panicked.
    NaiveConcurrent,
    /// Sparse-access single-agent: scripted access pattern (one
    /// `ls` plus read of N files) against three configurations:
    /// projgit mount of a partial clone, partial-clone-plus-
    /// `cat-file --batch`, and `clone --depth=1` direct read.
    /// Measures the per-blob path and the upfront-cost gap;
    /// projgit's per-blob path should be structurally equivalent
    /// to the partial-cat configuration (same mechanism, plus
    /// FUSE overhead).
    SparseSingle,
    /// Sparse-access multi-agent: N agents each run the same
    /// scripted access pattern (one `ls` plus read of N files)
    /// against two configurations — N projgit sidecars sharing
    /// one in-thread daemon + one CAS, vs N independent
    /// `--filter=blob:none` clones each with their own
    /// `cat-file --batch` reader. 100% blob overlap (every agent
    /// reads the same files), so the daemon's Coalescer is the
    /// architectural property under measurement and the shared
    /// CAS is the disk-bytes property under measurement.
    SparseShared,
}

impl Scenario {
    fn is_concurrent(self) -> bool {
        matches!(self, Scenario::DaemonConcurrent | Scenario::NaiveConcurrent)
    }

    fn is_sparse(self) -> bool {
        matches!(self, Scenario::SparseSingle | Scenario::SparseShared)
    }

    fn uses_concurrency(self) -> bool {
        matches!(
            self,
            Scenario::DaemonConcurrent | Scenario::NaiveConcurrent | Scenario::SparseShared
        )
    }
}

#[derive(Debug, Clone)]
struct Args {
    url: String,
    ref_name: String,
    files: Vec<String>,
    iterations: usize,
    scenario: Scenario,
    /// Number of concurrent sidecars in the `daemon-concurrent` /
    /// `naive-concurrent` scenarios. Ignored by `single` /
    /// `sequential`. Default `DEFAULT_CONCURRENCY`.
    concurrency: usize,
}

fn parse_args() -> Args {
    let mut url = DEFAULT_URL.to_owned();
    let mut ref_name = DEFAULT_REF.to_owned();
    let mut files: Vec<String> = DEFAULT_FILES.iter().map(|s| (*s).to_owned()).collect();
    let mut iterations = DEFAULT_ITERATIONS;
    let mut scenario = Scenario::Single;
    let mut concurrency = DEFAULT_CONCURRENCY;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => url = it.next().expect("--url needs a value"),
            "--ref" => ref_name = it.next().expect("--ref needs a value"),
            "--files" => {
                let v = it.next().expect("--files needs a comma-separated list");
                files = v
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                if files.is_empty() {
                    eprintln!("--files must contain at least one path");
                    std::process::exit(2);
                }
            }
            "--iterations" => {
                let v = it.next().expect("--iterations needs a value");
                iterations = v.parse().unwrap_or_else(|_| {
                    eprintln!("--iterations must be a positive integer");
                    std::process::exit(2)
                });
                if iterations == 0 {
                    eprintln!("--iterations must be > 0");
                    std::process::exit(2);
                }
            }
            "--scenario" => {
                let v = it.next().expect("--scenario needs a value");
                scenario = match v.as_str() {
                    "single" => Scenario::Single,
                    "sequential" => Scenario::Sequential,
                    "daemon-concurrent" => Scenario::DaemonConcurrent,
                    "naive-concurrent" => Scenario::NaiveConcurrent,
                    "sparse-single" => Scenario::SparseSingle,
                    "sparse-shared" => Scenario::SparseShared,
                    other => {
                        eprintln!(
                            "unknown scenario: {other} (expected: single, sequential, daemon-concurrent, naive-concurrent, sparse-single, sparse-shared)"
                        );
                        std::process::exit(2);
                    }
                };
            }
            "--concurrency" => {
                let v = it.next().expect("--concurrency needs a value");
                concurrency = v.parse().unwrap_or_else(|_| {
                    eprintln!("--concurrency must be a positive integer");
                    std::process::exit(2)
                });
                if concurrency == 0 {
                    eprintln!("--concurrency must be > 0");
                    std::process::exit(2);
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: bench_mount [--url <URL>] [--ref <NAME>] [--files a,b,c] [--iterations N] [--scenario single|sequential|daemon-concurrent|naive-concurrent|sparse-single|sparse-shared] [--concurrency N]\n  default URL: {DEFAULT_URL}\n  default ref: {DEFAULT_REF}\n  default files: {}\n  default iterations: {DEFAULT_ITERATIONS}\n  default scenario: single\n  default concurrency: {DEFAULT_CONCURRENCY} (only used by *-concurrent and sparse-shared scenarios)",
                    DEFAULT_FILES.join(","),
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    Args {
        url,
        ref_name,
        files,
        iterations,
        scenario,
        concurrency,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!("bench_mount: only runs on Linux/macOS (FUSE)");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    if std::env::var("PROJGIT_NETWORK_TESTS").as_deref() != Ok("1") {
        eprintln!("SKIP: set PROJGIT_NETWORK_TESTS=1 to enable this bench");
        return Ok(());
    }
    if !git_available() {
        anyhow::bail!("git not available on PATH");
    }
    let args = parse_args();

    eprintln!(
        "bench_mount: {} @ {} ({} iterations, scenario={:?}, files={:?}{})\n",
        args.url,
        args.ref_name,
        args.iterations,
        args.scenario,
        args.files,
        if args.scenario.uses_concurrency() {
            format!(", concurrency={}", args.concurrency)
        } else {
            String::new()
        },
    );

    if args.scenario.is_concurrent() {
        run_concurrent_main(&args)
    } else if args.scenario.is_sparse() {
        run_sparse_main(&args)
    } else {
        run_paired_main(&args)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_paired_main(args: &Args) -> anyhow::Result<()> {
    let mut projgit_samples: Vec<ProjgitSample> = Vec::with_capacity(args.iterations);
    let mut git_samples: Vec<GitSample> = Vec::with_capacity(args.iterations);

    for i in 1..=args.iterations {
        eprintln!("== iteration {i}/{} ==", args.iterations);

        eprint!("  projgit...   ");
        let p = bench_projgit(args)?;
        eprintln!("ok");
        projgit_samples.push(p);

        eprint!("  git baseline ");
        let g = bench_git_baseline(args)?;
        eprintln!("ok");
        git_samples.push(g);
    }

    eprintln!();
    print_report(args, &projgit_samples, &git_samples);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_concurrent_main(args: &Args) -> anyhow::Result<()> {
    let mut samples: Vec<ConcurrentSample> = Vec::with_capacity(args.iterations);
    for i in 1..=args.iterations {
        eprintln!(
            "== iteration {i}/{} (N={}) ==",
            args.iterations, args.concurrency
        );
        eprint!("  setup (clone + daemon)...  ");
        let s = match args.scenario {
            Scenario::DaemonConcurrent => bench_projgit_daemon_concurrent(args)?,
            Scenario::NaiveConcurrent => bench_projgit_naive_concurrent(args)?,
            Scenario::Single
            | Scenario::Sequential
            | Scenario::SparseSingle
            | Scenario::SparseShared => unreachable!(),
        };
        eprintln!(
            "ok (setup {} ms, wall {} ms, fail {})",
            ms(s.setup),
            ms(s.wall_clock),
            s.failures,
        );
        samples.push(s);
    }
    eprintln!();
    print_concurrent_report(args, &samples);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_sparse_main(args: &Args) -> anyhow::Result<()> {
    match args.scenario {
        Scenario::SparseSingle => run_sparse_single(args),
        Scenario::SparseShared => run_sparse_shared(args),
        _ => unreachable!(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_sparse_single(args: &Args) -> anyhow::Result<()> {
    let mut samples: Vec<SparseSingleSample> = Vec::with_capacity(args.iterations);
    for i in 1..=args.iterations {
        eprintln!("== iteration {i}/{} ==", args.iterations);
        let s = bench_sparse_single(args)?;
        eprintln!(
            "  projgit:     setup {} ms, script {} ms, disk {} KiB",
            ms(s.projgit.setup),
            ms(s.projgit.script),
            s.projgit.disk_bytes / 1024,
        );
        eprintln!(
            "  partial-cat: setup {} ms, script {} ms, disk {} KiB",
            ms(s.partial_cat.setup),
            ms(s.partial_cat.script),
            s.partial_cat.disk_bytes / 1024,
        );
        eprintln!(
            "  depth1:      setup {} ms, script {} ms, disk {} KiB",
            ms(s.depth1.setup),
            ms(s.depth1.script),
            s.depth1.disk_bytes / 1024,
        );
        samples.push(s);
    }
    eprintln!();
    print_sparse_single_report(args, &samples);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_sparse_shared(args: &Args) -> anyhow::Result<()> {
    let mut samples: Vec<SparseSharedSample> = Vec::with_capacity(args.iterations);
    for i in 1..=args.iterations {
        eprintln!(
            "== iteration {i}/{} (N={}) ==",
            args.iterations, args.concurrency
        );
        let s = bench_sparse_shared(args)?;
        eprintln!(
            "  projgit-shared:          setup {} ms, wall {} ms, disk {} KiB, fail {}",
            ms(s.projgit_shared.setup),
            ms(s.projgit_shared.wall_clock),
            s.projgit_shared.disk_bytes / 1024,
            s.projgit_shared.failures,
        );
        eprintln!(
            "  partial-cat-independent: setup {} ms, wall {} ms, disk {} KiB, fail {}",
            ms(s.partial_cat_independent.setup),
            ms(s.partial_cat_independent.wall_clock),
            s.partial_cat_independent.disk_bytes / 1024,
            s.partial_cat_independent.failures,
        );
        samples.push(s);
    }
    eprintln!();
    print_sparse_shared_report(args, &samples);
    Ok(())
}

// -----------------------------------------------------------------------------
// projgit side
// -----------------------------------------------------------------------------

/// Wall-clock timings for a single projgit iteration. In `single`
/// scenario this is one fresh partial clone + one mount. In
/// `sequential` scenario this is one fresh partial clone + one mount
/// (recorded in the cold/warm fields) + a second mount of a fresh
/// `ObjectStore` against the same on-disk cache dir (recorded in
/// `mount2_cold_cat`).
#[derive(Debug, Clone)]
struct ProjgitSample {
    /// `git clone --filter=blob:none --no-checkout` time.
    partial_clone: Duration,
    /// First `read_dir` of mountpoint root.
    cold_readdir_root: Duration,
    /// First recursive walk via projgit.
    cold_walk: Duration,
    /// First `read_to_string` of `args.files`.
    cold_cat: Duration,
    /// Second `read_dir` of mountpoint root.
    warm_readdir_root: Duration,
    /// Second recursive walk.
    warm_walk: Duration,
    /// Second `read_to_string` of the same files.
    warm_cat: Duration,
    /// `Sequential` only: cold `read_to_string` of `args.files` from a
    /// freshly-constructed `ObjectStore` mounted against the same
    /// cache dir as the first mount. Empty in-process caches, warm
    /// on-disk CAS. This is the workload-doc §1.6 amortisation
    /// falsifier: if this approaches `warm_cat`, §1.6 holds; if it
    /// approaches `cold_cat`, the on-disk store isn't amortising and
    /// there's a real finding.
    mount2_cold_cat: Option<Duration>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bench_projgit(args: &Args) -> anyhow::Result<ProjgitSample> {
    use projgit_core::clone::{partial_clone, CloneOptions};

    let cache_dir = make_temp("projgit-bench-cache");
    let _cache_guard = DirGuard(cache_dir.clone());

    // Partial clone (counted separately so the table is honest about
    // where the time goes).
    let partial_clone_t = time_it(|| {
        let opts = CloneOptions::new(args.url.clone(), cache_dir.clone());
        partial_clone(&opts)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    })?;

    let mount1 = projgit_mount_once(args, &cache_dir, |store| {
        use projgit_core::{GitCliFetcher, HydratingObjectStore};
        use std::sync::Arc;
        let fetcher = GitCliFetcher::open(store.clone())?;
        Ok(Arc::new(HydratingObjectStore::new(store, fetcher)))
    })?;

    let mount2_cold_cat = match args.scenario {
        Scenario::Single => None,
        Scenario::Sequential => Some(projgit_remount_cold_cat(args, &cache_dir)?),
        Scenario::DaemonConcurrent
        | Scenario::NaiveConcurrent
        | Scenario::SparseSingle
        | Scenario::SparseShared => {
            unreachable!("*-concurrent and sparse-* dispatch via their own run_*_main")
        }
    };

    Ok(ProjgitSample {
        partial_clone: partial_clone_t,
        cold_readdir_root: mount1.cold_readdir_root,
        cold_walk: mount1.cold_walk,
        cold_cat: mount1.cold_cat,
        warm_readdir_root: mount1.warm_readdir_root,
        warm_walk: mount1.warm_walk,
        warm_cat: mount1.warm_cat,
        mount2_cold_cat,
    })
}

/// Cold + warm timings collected from one mount session.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone)]
struct MountTimings {
    cold_readdir_root: Duration,
    cold_walk: Duration,
    cold_cat: Duration,
    warm_readdir_root: Duration,
    warm_walk: Duration,
    warm_cat: Duration,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn projgit_mount_once<F, MakeHydrating>(
    args: &Args,
    cache_dir: &Path,
    make_hydrating: MakeHydrating,
) -> anyhow::Result<MountTimings>
where
    F: projgit_core::Fetcher + Send + Sync + 'static,
    MakeHydrating: FnOnce(
        std::sync::Arc<projgit_core::ObjectStore>,
    ) -> anyhow::Result<
        std::sync::Arc<projgit_core::HydratingObjectStore<F>>,
    >,
{
    use projgit_core::{
        clone::git_dir_for, ObjectStore, Projection, ProjectionFsProvider, RootOverlay,
    };
    use projgit_fuse::{mount_background, MountConfig};
    use std::sync::Arc;

    let mountpoint = make_temp("projgit-bench-mp");
    let _mp_guard = DirGuard(mountpoint.clone());

    let store = Arc::new(ObjectStore::open(git_dir_for(cache_dir))?);
    let hydrating = make_hydrating(store)?;
    let provider = Arc::new(ProjectionFsProvider::new(
        Projection::Ref(args.ref_name.clone()),
        hydrating,
        RootOverlay::new(),
        /* projection_id */ 1,
    )?);

    let session = mount_background(provider, &mountpoint, &MountConfig::default())?;
    wait_for_mount(&mountpoint, Duration::from_secs(10))?;

    let cold_readdir_root = time_it(|| {
        let _ = read_dir_names(&mountpoint)?;
        Ok::<_, anyhow::Error>(())
    })?;
    let cold_walk = time_it(|| {
        let _ = walk_count(&mountpoint)?;
        Ok::<_, anyhow::Error>(())
    })?;
    let cold_cat = time_it(|| {
        for f in &args.files {
            let _ = std::fs::read_to_string(mountpoint.join(f))?;
        }
        Ok::<_, anyhow::Error>(())
    })?;

    // Warm: same operations, now hitting the in-process caches and
    // local-store-resident blobs.
    let warm_readdir_root = time_it(|| {
        let _ = read_dir_names(&mountpoint)?;
        Ok::<_, anyhow::Error>(())
    })?;
    let warm_walk = time_it(|| {
        let _ = walk_count(&mountpoint)?;
        Ok::<_, anyhow::Error>(())
    })?;
    let warm_cat = time_it(|| {
        for f in &args.files {
            let _ = std::fs::read_to_string(mountpoint.join(f))?;
        }
        Ok::<_, anyhow::Error>(())
    })?;

    drop(session);
    Ok(MountTimings {
        cold_readdir_root,
        cold_walk,
        cold_cat,
        warm_readdir_root,
        warm_walk,
        warm_cat,
    })
}

/// Sequential-scenario second mount. Constructs a fresh
/// `ObjectStore`/`Fetcher`/`Provider` against the same on-disk cache
/// dir as the first mount, then measures only cold-cat of `args.files`.
/// See `ProjgitSample::mount2_cold_cat` for the interpretation.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn projgit_remount_cold_cat(args: &Args, cache_dir: &Path) -> anyhow::Result<Duration> {
    use projgit_core::{
        clone::git_dir_for, GitCliFetcher, HydratingObjectStore, ObjectStore, Projection,
        ProjectionFsProvider, RootOverlay,
    };
    use projgit_fuse::{mount_background, MountConfig};
    use std::sync::Arc;

    let mountpoint = make_temp("projgit-bench-mp2");
    let _mp_guard = DirGuard(mountpoint.clone());

    let store = Arc::new(ObjectStore::open(git_dir_for(cache_dir))?);
    let fetcher = GitCliFetcher::open(store.clone())?;
    let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
    let provider = Arc::new(ProjectionFsProvider::new(
        Projection::Ref(args.ref_name.clone()),
        hydrating,
        RootOverlay::new(),
        /* projection_id */ 2,
    )?);

    let session = mount_background(provider, &mountpoint, &MountConfig::default())?;
    wait_for_mount(&mountpoint, Duration::from_secs(10))?;

    let cold_cat = time_it(|| {
        for f in &args.files {
            let _ = std::fs::read_to_string(mountpoint.join(f))?;
        }
        Ok::<_, anyhow::Error>(())
    })?;

    drop(session);
    Ok(cold_cat)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_dir_names(p: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(p)? {
        v.push(e?.file_name().to_string_lossy().into_owned());
    }
    Ok(v)
}

/// Recursive directory walk through the mount that returns the count
/// of regular files seen. Resolves symlinks via `symlink_metadata` so
/// we never accidentally follow a `.git`-style cycle if one is added
/// later.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn walk_count(root: &std::path::Path) -> anyhow::Result<usize> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let m = entry.file_type()?;
            if m.is_dir() {
                stack.push(entry.path());
            } else if m.is_file() {
                files += 1;
            }
        }
    }
    Ok(files)
}

// -----------------------------------------------------------------------------
// git baseline side
// -----------------------------------------------------------------------------

/// Wall-clock timings for the analogous git operations against a
/// fresh `git clone --filter=blob:none --no-checkout` of the same
/// repo.
#[derive(Debug, Clone)]
struct GitSample {
    /// `git clone --filter=blob:none --no-checkout`.
    partial_clone: Duration,
    /// `git ls-tree <ref>` of the root tree.
    ls_tree_root: Duration,
    /// `git ls-tree -r <ref>` of the entire commit tree.
    walk_ls_tree_r: Duration,
    /// `git cat-file blob <ref>:<file>` for each entry of [`FILES`].
    cat_blobs: Duration,
}

fn bench_git_baseline(args: &Args) -> anyhow::Result<GitSample> {
    let dir = make_temp("projgit-bench-git");
    let _guard = DirGuard(dir.clone());

    let partial_clone = time_it(|| {
        run_git(
            None,
            &[
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                args.url.as_str(),
                dir.to_str().expect("utf-8 tmp path"),
            ],
        )
    })?;

    let ls_tree_root = time_it(|| run_git(Some(&dir), &["ls-tree", &args.ref_name]))?;
    let walk_ls_tree_r = time_it(|| run_git(Some(&dir), &["ls-tree", "-r", &args.ref_name]))?;
    let cat_blobs = time_it(|| {
        for f in &args.files {
            run_git(
                Some(&dir),
                &["cat-file", "blob", &format!("{}:{}", args.ref_name, f)],
            )?;
        }
        Ok::<_, anyhow::Error>(())
    })?;

    Ok(GitSample {
        partial_clone,
        ls_tree_root,
        walk_ls_tree_r,
        cat_blobs,
    })
}

fn run_git(cwd: Option<&std::path::Path>, args: &[&str]) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Reporting
// -----------------------------------------------------------------------------

fn print_report(args: &Args, projgit: &[ProjgitSample], git: &[GitSample]) {
    let p_clone = median(projgit.iter().map(|s| s.partial_clone).collect());
    let p_cold_ls = median(projgit.iter().map(|s| s.cold_readdir_root).collect());
    let p_cold_walk = median(projgit.iter().map(|s| s.cold_walk).collect());
    let p_cold_cat = median(projgit.iter().map(|s| s.cold_cat).collect());
    let p_warm_ls = median(projgit.iter().map(|s| s.warm_readdir_root).collect());
    let p_warm_walk = median(projgit.iter().map(|s| s.warm_walk).collect());
    let p_warm_cat = median(projgit.iter().map(|s| s.warm_cat).collect());

    let g_clone = median(git.iter().map(|s| s.partial_clone).collect());
    let g_ls_root = median(git.iter().map(|s| s.ls_tree_root).collect());
    let g_walk = median(git.iter().map(|s| s.walk_ls_tree_r).collect());
    let g_cat = median(git.iter().map(|s| s.cat_blobs).collect());

    println!("# bench_mount: {} @ {}\n", args.url, args.ref_name);
    println!(
        "Median of {} iterations, scenario `{:?}`. All times in milliseconds.\n",
        args.iterations, args.scenario,
    );
    println!("## One-time setup\n");
    println!(
        "| Step | projgit (`partial_clone`) | git (`clone --filter=blob:none --no-checkout`) |"
    );
    println!("|---|---:|---:|");
    println!("| Partial clone | {} | {} |", ms(p_clone), ms(g_clone));
    println!();
    println!("## Per-operation\n");
    println!("| Operation | projgit cold | projgit warm | git baseline |");
    println!("|---|---:|---:|---:|");
    println!(
        "| readdir of root | {} | {} | {} |",
        ms(p_cold_ls),
        ms(p_warm_ls),
        ms(g_ls_root)
    );
    println!(
        "| recursive walk | {} | {} | {} |",
        ms(p_cold_walk),
        ms(p_warm_walk),
        ms(g_walk)
    );
    println!(
        "| cat {} files | {} | {} | {} |",
        args.files.len(),
        ms(p_cold_cat),
        ms(p_warm_cat),
        ms(g_cat)
    );

    if args.scenario == Scenario::Sequential {
        let mount2: Vec<Duration> = projgit
            .iter()
            .filter_map(|s| s.mount2_cold_cat)
            .collect();
        if !mount2.is_empty() {
            let p_m2 = median(mount2);
            println!();
            println!("## Cross-process amortisation (workload-doc §1.6)\n");
            println!(
                "A second mount in the same process, against the same on-disk\ncache dir, with a freshly-constructed `ObjectStore` (in-process\ncaches empty, on-disk CAS warm from mount 1). If `mount 2 cold`\napproaches `mount 1 warm`, §1.6 holds; if it approaches\n`mount 1 cold`, the on-disk store isn't amortising.\n"
            );
            println!(
                "| Operation | mount 1 cold | mount 1 warm | **mount 2 cold (cross-process)** |"
            );
            println!("|---|---:|---:|---:|");
            println!(
                "| cat {} files | {} | {} | **{}** |",
                args.files.len(),
                ms(p_cold_cat),
                ms(p_warm_cat),
                ms(p_m2),
            );
        }
    }
}

fn ms(d: Duration) -> String {
    let m = d.as_secs_f64() * 1000.0;
    if m < 10.0 {
        format!("{m:.2}")
    } else {
        format!("{m:.1}")
    }
}

fn median(mut v: Vec<Duration>) -> Duration {
    assert!(!v.is_empty());
    v.sort();
    v[v.len() / 2]
}

// -----------------------------------------------------------------------------
// Phase C: concurrent scenarios
// -----------------------------------------------------------------------------

/// Wall-clock timings for one iteration of a `*-concurrent` scenario.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone)]
struct ConcurrentSample {
    /// One-time setup cost: partial-clone (and, for `daemon-concurrent`,
    /// daemon startup + Attach that triggers the daemon's own partial
    /// clone). Reported separately from the measurement window so the
    /// concurrent table stays comparable to the existing `single` /
    /// `sequential` tables.
    setup: Duration,
    /// Per-thread cold-cat wall-clock, recorded from inside each
    /// sidecar thread (mount + cold-cat + unmount). One entry per
    /// successful thread.
    per_thread: Vec<Duration>,
    /// Wall clock from "all N threads spawned" to "all N joined".
    /// The load-bearing headline number: how long it took to satisfy
    /// N concurrent consumers cold-reading the same files.
    wall_clock: Duration,
    /// Number of threads that failed (mount error, cat error, daemon
    /// hiccup, or — in the naive arm — a git lock collision).
    failures: usize,
}

/// `daemon-concurrent`: one in-thread daemon + N sidecars, each
/// holding `DaemonFetcher`, cold-catting `args.files` at the same
/// time. The daemon's existing `Coalescer` (the in-flight
/// single-flight inside `HydratingObjectStore::header()`) is the
/// architectural property under test (audit A3 closure).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bench_projgit_daemon_concurrent(args: &Args) -> anyhow::Result<ConcurrentSample> {
    use projgit_core::{
        HydratingObjectStore, ObjectStore, Projection, ProjectionFsProvider, RootOverlay,
    };
    use projgit_daemon::protocol::{read_message, write_message, Request, Response};
    use projgit_daemon::server::{run as daemon_run, DaemonConfig};
    use projgit_daemon::DaemonFetcher;
    use projgit_fuse::{mount_background, MountConfig};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    let n = args.concurrency;

    // --- setup window ---------------------------------------------------------
    let setup_start = Instant::now();

    // Fresh cache root for this iteration. The daemon writes its
    // partial clone into a hash-named subdir below this; both
    // get cleaned by DirGuard at iteration end.
    let cache_root = make_temp("projgit-bench-cache-d");
    let _cache_guard = DirGuard(cache_root.clone());
    let socket_path = make_temp("projgit-bench-sock").join("daemon.sock");
    // `make_temp` creates the dir; remove it so the socket bind has
    // a clean parent containing only the eventual socket file.
    let _ = std::fs::remove_file(&socket_path);

    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        socket_mode: 0o600,
        cache_dir: Some(cache_root.clone()),
    };
    let daemon_handle = thread::spawn(move || daemon_run(config));

    // Wait for the daemon to bind its socket.
    let bind_deadline = Instant::now() + Duration::from_secs(5);
    while !socket_path.exists() {
        if Instant::now() > bind_deadline {
            anyhow::bail!("daemon never created socket at {}", socket_path.display());
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Attach to the source URL. The daemon does the partial clone
    // here (or reuses if cached); we get back the on-disk git_dir
    // that every sidecar thread will open its own ObjectStore
    // against. Partial-clone cost lives inside `setup`, not
    // inside the measurement window.
    let git_dir = {
        let mut s = UnixStream::connect(&socket_path)?;
        write_message(
            &mut s,
            &Request::Attach {
                source: args.url.clone(),
            },
        )?;
        match read_message::<_, Response>(&mut s)? {
            Response::Attached { git_dir } => git_dir,
            other => anyhow::bail!("daemon attach: unexpected {other:?}"),
        }
    };

    // Pre-create N mountpoints so per-thread setup is just mount.
    let mountpoints: Vec<PathBuf> = (0..n).map(|_| make_temp("projgit-bench-mp-d")).collect();
    let _mp_guards: Vec<DirGuard> = mountpoints.iter().cloned().map(DirGuard).collect();

    let setup = setup_start.elapsed();

    // --- measurement window ---------------------------------------------------
    let barrier = Arc::new(Barrier::new(n + 1));
    let (tx, rx) = mpsc::channel::<Result<Duration, String>>();
    let mut handles = Vec::with_capacity(n);

    for (tid, mp) in mountpoints.iter().enumerate() {
        let barrier = barrier.clone();
        let tx = tx.clone();
        let socket_path = socket_path.clone();
        let git_dir = git_dir.clone();
        let ref_name = args.ref_name.clone();
        let files = args.files.clone();
        let mp = mp.clone();

        let handle = thread::Builder::new()
            .name(format!("sidecar-{tid}"))
            .spawn(move || {
                // All threads gate on the barrier so wall clock
                // measures contention, not thread-start staggering.
                barrier.wait();
                let t0 = Instant::now();
                let result = (|| -> anyhow::Result<()> {
                    let store = Arc::new(ObjectStore::open(&git_dir)?);
                    let fetcher = DaemonFetcher::new(socket_path);
                    let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
                    let provider = Arc::new(ProjectionFsProvider::new(
                        Projection::Ref(ref_name),
                        hydrating,
                        RootOverlay::new(),
                        (tid as u64) + 100,
                    )?);
                    let session =
                        mount_background(provider, &mp, &MountConfig::default())?;
                    wait_for_mount(&mp, Duration::from_secs(10))?;
                    for f in &files {
                        let _ = std::fs::read_to_string(mp.join(f))?;
                    }
                    drop(session);
                    Ok(())
                })();
                let elapsed = t0.elapsed();
                let _ = tx.send(result.map(|_| elapsed).map_err(|e| e.to_string()));
            })?;
        handles.push(handle);
    }
    drop(tx);

    // Release threads simultaneously; this is `t=0` for the
    // measurement window.
    barrier.wait();
    let measurement_start = Instant::now();

    let mut per_thread = Vec::with_capacity(n);
    let mut failures = 0usize;
    for r in rx {
        match r {
            Ok(d) => per_thread.push(d),
            Err(e) => {
                failures += 1;
                eprintln!("    thread failure: {e}");
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }
    let wall_clock = measurement_start.elapsed();

    // --- teardown -------------------------------------------------------------
    let mut s = UnixStream::connect(&socket_path)?;
    write_message(&mut s, &Request::Shutdown)?;
    let _: Response = read_message(&mut s)?;
    let _ = daemon_handle.join();

    Ok(ConcurrentSample {
        setup,
        per_thread,
        wall_clock,
        failures,
    })
}

/// `naive-concurrent`: no daemon. N independent local mounts sharing
/// one on-disk cache dir, each driving its own `GitCliFetcher`. This
/// is the actual A3 scenario the daemon was built to fix — the
/// fetchers race `git fetch` (lazy promisor) into one
/// `.git/objects/pack/` with no coordination. Failures (git lock,
/// pack contention, "fatal: could not parse object") are counted
/// rather than panicked-on, per design doc §6.1.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bench_projgit_naive_concurrent(args: &Args) -> anyhow::Result<ConcurrentSample> {
    use projgit_core::clone::{git_dir_for, partial_clone, CloneOptions};
    use projgit_core::{
        GitCliFetcher, HydratingObjectStore, ObjectStore, Projection, ProjectionFsProvider,
        RootOverlay,
    };
    use projgit_fuse::{mount_background, MountConfig};
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    let n = args.concurrency;

    // --- setup window ---------------------------------------------------------
    let setup_start = Instant::now();

    // Single shared cache_dir — the load-bearing property: every
    // thread's `git fetch` lands in the same `.git/objects/pack/`,
    // which is what audit A3 cares about.
    let cache_dir = make_temp("projgit-bench-cache-n");
    let _cache_guard = DirGuard(cache_dir.clone());
    {
        let opts = CloneOptions::new(args.url.clone(), cache_dir.clone());
        partial_clone(&opts)?;
    }
    let git_dir = git_dir_for(&cache_dir);

    let mountpoints: Vec<PathBuf> = (0..n).map(|_| make_temp("projgit-bench-mp-n")).collect();
    let _mp_guards: Vec<DirGuard> = mountpoints.iter().cloned().map(DirGuard).collect();

    let setup = setup_start.elapsed();

    // --- measurement window ---------------------------------------------------
    let barrier = Arc::new(Barrier::new(n + 1));
    let (tx, rx) = mpsc::channel::<Result<Duration, String>>();
    let mut handles = Vec::with_capacity(n);

    for (tid, mp) in mountpoints.iter().enumerate() {
        let barrier = barrier.clone();
        let tx = tx.clone();
        let git_dir = git_dir.clone();
        let ref_name = args.ref_name.clone();
        let files = args.files.clone();
        let mp = mp.clone();

        let handle = thread::Builder::new()
            .name(format!("naive-{tid}"))
            .spawn(move || {
                barrier.wait();
                let t0 = Instant::now();
                let result = (|| -> anyhow::Result<()> {
                    let store = Arc::new(ObjectStore::open(&git_dir)?);
                    let fetcher = GitCliFetcher::open(store.clone())?;
                    let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
                    let provider = Arc::new(ProjectionFsProvider::new(
                        Projection::Ref(ref_name),
                        hydrating,
                        RootOverlay::new(),
                        (tid as u64) + 200,
                    )?);
                    let session =
                        mount_background(provider, &mp, &MountConfig::default())?;
                    wait_for_mount(&mp, Duration::from_secs(10))?;
                    for f in &files {
                        let _ = std::fs::read_to_string(mp.join(f))?;
                    }
                    drop(session);
                    Ok(())
                })();
                let elapsed = t0.elapsed();
                let _ = tx.send(result.map(|_| elapsed).map_err(|e| e.to_string()));
            })?;
        handles.push(handle);
    }
    drop(tx);

    barrier.wait();
    let measurement_start = Instant::now();

    let mut per_thread = Vec::with_capacity(n);
    let mut failures = 0usize;
    for r in rx {
        match r {
            Ok(d) => per_thread.push(d),
            Err(e) => {
                failures += 1;
                eprintln!("    thread failure: {e}");
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }
    let wall_clock = measurement_start.elapsed();

    Ok(ConcurrentSample {
        setup,
        per_thread,
        wall_clock,
        failures,
    })
}

/// Format a per-N table summarising the concurrent scenarios. Mirrors
/// the existing `print_report` style: human-readable, copy-pastable
/// into baseline.md.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn print_concurrent_report(args: &Args, samples: &[ConcurrentSample]) {
    let setup_med = median(samples.iter().map(|s| s.setup).collect());
    let wall_med = median(samples.iter().map(|s| s.wall_clock).collect());
    let per_thread_all: Vec<Duration> = samples
        .iter()
        .flat_map(|s| s.per_thread.clone())
        .collect();
    let per_thread_p50 = if per_thread_all.is_empty() {
        Duration::ZERO
    } else {
        median(per_thread_all.clone())
    };
    let total_failures: usize = samples.iter().map(|s| s.failures).sum();

    println!("# bench_mount: {} @ {}\n", args.url, args.ref_name);
    println!(
        "Median of {} iterations, scenario `{:?}`, concurrency N={}. All times in milliseconds.\n",
        args.iterations, args.scenario, args.concurrency,
    );
    println!("## Setup (per iteration)\n");
    println!("| Step | Time |");
    println!("|---|---:|");
    println!(
        "| Partial clone + daemon attach (or partial clone only for `naive-*`) | {} |",
        ms(setup_med)
    );
    println!();
    println!("## Measurement window\n");
    println!(
        "| Scenario | N | Wall clock | Per-thread p50 | Failures (sum across iters) |"
    );
    println!("|---|---:|---:|---:|---:|");
    println!(
        "| `{:?}` | {} | {} | {} | {} |",
        args.scenario,
        args.concurrency,
        ms(wall_med),
        ms(per_thread_p50),
        total_failures,
    );
    if !per_thread_all.is_empty() {
        let mut sorted = per_thread_all.clone();
        sorted.sort();
        let min = sorted[0];
        let max = *sorted.last().unwrap();
        println!();
        println!(
            "(per-thread range across all iterations: min {} ms, max {} ms, n={})",
            ms(min),
            ms(max),
            per_thread_all.len()
        );
    }
}

// -----------------------------------------------------------------------------
// Sparse-access: single-agent
// -----------------------------------------------------------------------------

/// One iteration of `sparse-single`: setup / script / disk_bytes for
/// each of three configurations against the same target.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone)]
struct SparseSingleSample {
    /// projgit mount of a partial clone.
    projgit: SparseConfigSample,
    /// `git clone --filter=blob:none --no-checkout` +
    /// `git cat-file --batch` for each scripted read.
    partial_cat: SparseConfigSample,
    /// `git clone --depth=1` + direct filesystem reads.
    depth1: SparseConfigSample,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy)]
struct SparseConfigSample {
    /// Setup window: whatever cloning / mounting the config needs
    /// before the access script can run.
    setup: Duration,
    /// Script window: `ls` of the root + read every file in
    /// `args.files`.
    script: Duration,
    /// Bytes resident on disk after the script (cache dir for
    /// projgit / partial-cat; full clone for depth1).
    disk_bytes: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bench_sparse_single(args: &Args) -> anyhow::Result<SparseSingleSample> {
    eprint!("  projgit...     ");
    let projgit = sparse_single_projgit(args)?;
    eprintln!("ok");
    eprint!("  partial-cat... ");
    let partial_cat = sparse_single_partial_cat(args)?;
    eprintln!("ok");
    eprint!("  depth1...      ");
    let depth1 = sparse_single_depth1(args)?;
    eprintln!("ok");
    Ok(SparseSingleSample {
        projgit,
        partial_cat,
        depth1,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sparse_single_projgit(args: &Args) -> anyhow::Result<SparseConfigSample> {
    use projgit_core::clone::{git_dir_for, partial_clone, CloneOptions};
    use projgit_core::{
        GitCliFetcher, HydratingObjectStore, ObjectStore, Projection, ProjectionFsProvider,
        RootOverlay,
    };
    use projgit_fuse::{mount_background, MountConfig};
    use std::sync::Arc;

    let cache_dir = make_temp("projgit-bench-sparse-pj-cache");
    let _cache_guard = DirGuard(cache_dir.clone());
    let mountpoint = make_temp("projgit-bench-sparse-pj-mp");
    let _mp_guard = DirGuard(mountpoint.clone());

    let setup = time_it(|| -> anyhow::Result<()> {
        let opts = CloneOptions::new(args.url.clone(), cache_dir.clone());
        partial_clone(&opts)?;
        Ok(())
    })?;

    // Build the projgit stack and mount inside the script window —
    // matches `sparse-shared`, where per-thread mount is part of
    // the per-agent cost. Avoids over-crediting the projgit arm by
    // hiding mount cost in setup.
    let script = time_it(|| -> anyhow::Result<()> {
        let store = Arc::new(ObjectStore::open(git_dir_for(&cache_dir))?);
        let fetcher = GitCliFetcher::open(store.clone())?;
        let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
        let provider = Arc::new(ProjectionFsProvider::new(
            Projection::Ref(args.ref_name.clone()),
            hydrating,
            RootOverlay::new(),
            /* projection_id */ 1,
        )?);
        let session = mount_background(provider, &mountpoint, &MountConfig::default())?;
        wait_for_mount(&mountpoint, Duration::from_secs(10))?;

        // `ls` the root.
        let _ = read_dir_names(&mountpoint)?;
        // Read each scripted file.
        for f in &args.files {
            let _ = std::fs::read_to_string(mountpoint.join(f))?;
        }
        drop(session);
        Ok(())
    })?;

    let disk_bytes = disk_bytes_of(&cache_dir)?;
    Ok(SparseConfigSample {
        setup,
        script,
        disk_bytes,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sparse_single_partial_cat(args: &Args) -> anyhow::Result<SparseConfigSample> {
    let dir = make_temp("projgit-bench-sparse-pc-clone");
    let _guard = DirGuard(dir.clone());

    let setup = time_it(|| {
        run_git(
            None,
            &[
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                args.url.as_str(),
                dir.to_str().expect("utf-8 tmp path"),
            ],
        )
    })?;

    let script = time_it(|| -> anyhow::Result<()> {
        // `ls` equivalent: `git ls-tree <ref>` of the root.
        run_git(Some(&dir), &["ls-tree", &args.ref_name])?;
        // Read each file via a long-lived `git cat-file --batch`
        // child. Mirrors projgit's GitCliFetcher strategy so the
        // comparator isn't unfairly penalised by per-call git
        // process startup.
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("cat-file")
            .arg("--batch")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        {
            use std::io::{BufRead, BufReader, Read, Write};
            let mut stdin = child.stdin.take().expect("piped stdin");
            let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
            for f in &args.files {
                writeln!(stdin, "{}:{}", args.ref_name, f)?;
                stdin.flush()?;
                let mut header = String::new();
                let n = stdout.read_line(&mut header)?;
                if n == 0 {
                    anyhow::bail!("git cat-file --batch closed mid-stream");
                }
                // `<sha> <kind> <size>\n`; pull the trailing size.
                let size: usize = header
                    .trim_end()
                    .rsplit_once(' ')
                    .and_then(|(_, n)| n.parse().ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!("git cat-file --batch: bad header line: {header:?}")
                    })?;
                let mut buf = vec![0u8; size];
                stdout.read_exact(&mut buf)?;
                // Skip the trailing newline git emits after the payload.
                let mut nl = [0u8; 1];
                stdout.read_exact(&mut nl)?;
            }
            // Drop stdin so the child sees EOF and exits.
        }
        let _ = child.wait();
        Ok(())
    })?;

    let disk_bytes = disk_bytes_of(&dir)?;
    Ok(SparseConfigSample {
        setup,
        script,
        disk_bytes,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sparse_single_depth1(args: &Args) -> anyhow::Result<SparseConfigSample> {
    let dir = make_temp("projgit-bench-sparse-d1-clone");
    let _guard = DirGuard(dir.clone());

    let setup = time_it(|| {
        run_git(
            None,
            &[
                "clone",
                "--depth=1",
                "--branch",
                args.ref_name.as_str(),
                args.url.as_str(),
                dir.to_str().expect("utf-8 tmp path"),
            ],
        )
    })?;

    let script = time_it(|| -> anyhow::Result<()> {
        // `ls` the working tree root (skip `.git`).
        let _ = read_dir_names(&dir)?;
        // Plain reads — everything's already on disk.
        for f in &args.files {
            let _ = std::fs::read_to_string(dir.join(f))?;
        }
        Ok(())
    })?;

    let disk_bytes = disk_bytes_of(&dir)?;
    Ok(SparseConfigSample {
        setup,
        script,
        disk_bytes,
    })
}

/// Recursive sum of file sizes under `root`. Does NOT follow
/// symlinks (uses `symlink_metadata` semantics via the file_type
/// check). Bench-grade — not a `du -s` replacement, but good
/// enough for one-shot reporting.
fn disk_bytes_of(root: &std::path::Path) -> anyhow::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue, // raced cleanup; bench-grade.
        };
        for entry in read {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() || ft.is_symlink() {
                if let Ok(m) = entry.metadata() {
                    total += m.len();
                }
            }
        }
    }
    Ok(total)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn print_sparse_single_report(args: &Args, samples: &[SparseSingleSample]) {
    // Compute medians per (config, axis) independently. We use
    // microseconds to avoid losing sub-ms resolution on the
    // typically small `script` numbers.
    let med_axis = |xs: &[u128]| -> u128 {
        let mut v: Vec<u128> = xs.to_vec();
        v.sort();
        v[v.len() / 2]
    };
    let med_dur = |samples: &[SparseSingleSample], pick: fn(&SparseSingleSample) -> Duration| {
        let xs: Vec<u128> = samples.iter().map(|s| pick(s).as_micros()).collect();
        Duration::from_micros(med_axis(&xs) as u64)
    };
    let med_bytes = |samples: &[SparseSingleSample], pick: fn(&SparseSingleSample) -> u64| -> u64 {
        let xs: Vec<u128> = samples.iter().map(|s| pick(s) as u128).collect();
        med_axis(&xs) as u64
    };

    let pj_setup = med_dur(samples, |s| s.projgit.setup);
    let pj_script = med_dur(samples, |s| s.projgit.script);
    let pj_disk = med_bytes(samples, |s| s.projgit.disk_bytes);
    let pc_setup = med_dur(samples, |s| s.partial_cat.setup);
    let pc_script = med_dur(samples, |s| s.partial_cat.script);
    let pc_disk = med_bytes(samples, |s| s.partial_cat.disk_bytes);
    let d1_setup = med_dur(samples, |s| s.depth1.setup);
    let d1_script = med_dur(samples, |s| s.depth1.script);
    let d1_disk = med_bytes(samples, |s| s.depth1.disk_bytes);

    println!("# bench_mount: {} @ {}\n", args.url, args.ref_name);
    println!(
        "Sparse-access single-agent. Median of {} iterations. Times in ms; disk in KiB.\n",
        args.iterations
    );
    println!(
        "Script: `ls` mount root + read {} file(s): {:?}\n",
        args.files.len(),
        args.files
    );
    println!("| Config | setup | script | disk |");
    println!("|---|---:|---:|---:|");
    println!(
        "| `projgit` (mount of partial clone) | {} | {} | {} |",
        ms(pj_setup),
        ms(pj_script),
        pj_disk / 1024
    );
    println!(
        "| `partial-cat` (`--filter=blob:none` + `cat-file --batch`) | {} | {} | {} |",
        ms(pc_setup),
        ms(pc_script),
        pc_disk / 1024
    );
    println!(
        "| `depth1` (`--depth=1` clone, direct reads) | {} | {} | {} |",
        ms(d1_setup),
        ms(d1_script),
        d1_disk / 1024
    );
}

// -----------------------------------------------------------------------------
// Sparse-access: multi-agent (sparse-shared)
// -----------------------------------------------------------------------------

/// One iteration of `sparse-shared`: setup / wall_clock /
/// disk_bytes / failure-count for each of two configurations
/// against the same target with N agents running the same
/// scripted access pattern.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone)]
struct SparseSharedSample {
    /// N projgit sidecars sharing one in-thread daemon + one CAS.
    projgit_shared: SparseSharedConfig,
    /// N independent `--filter=blob:none` clones, each with its
    /// own per-thread `cat-file --batch` reader. No sharing.
    partial_cat_independent: SparseSharedConfig,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone)]
struct SparseSharedConfig {
    /// Setup before the measurement window: clones, daemon
    /// startup, attach.
    setup: Duration,
    /// Wall clock from barrier release to last-thread join.
    wall_clock: Duration,
    /// Per-thread script wall clocks; one entry per successful
    /// thread.
    per_thread: Vec<Duration>,
    /// Total bytes on disk after the measurement window, summed
    /// across all cache / clone dirs (one for projgit-shared, N
    /// for partial-cat-independent).
    disk_bytes: u64,
    /// Threads that errored before reporting a duration.
    failures: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bench_sparse_shared(args: &Args) -> anyhow::Result<SparseSharedSample> {
    eprint!("  projgit-shared...           ");
    let projgit_shared = sparse_shared_projgit(args)?;
    eprintln!("ok");
    eprint!("  partial-cat-independent...  ");
    let partial_cat_independent = sparse_shared_partial_cat_independent(args)?;
    eprintln!("ok");
    Ok(SparseSharedSample {
        projgit_shared,
        partial_cat_independent,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sparse_shared_projgit(args: &Args) -> anyhow::Result<SparseSharedConfig> {
    use projgit_core::{
        HydratingObjectStore, ObjectStore, Projection, ProjectionFsProvider, RootOverlay,
    };
    use projgit_daemon::protocol::{read_message, write_message, Request, Response};
    use projgit_daemon::server::{run as daemon_run, DaemonConfig};
    use projgit_daemon::DaemonFetcher;
    use projgit_fuse::{mount_background, MountConfig};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    let n = args.concurrency;

    // --- setup window ---------------------------------------------------------
    let setup_start = Instant::now();

    let cache_root = make_temp("projgit-bench-sparse-pjs-cache");
    let _cache_guard = DirGuard(cache_root.clone());
    let socket_path = make_temp("projgit-bench-sparse-pjs-sock").join("daemon.sock");
    let _ = std::fs::remove_file(&socket_path);

    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        socket_mode: 0o600,
        cache_dir: Some(cache_root.clone()),
    };
    let daemon_handle = thread::spawn(move || daemon_run(config));

    let bind_deadline = Instant::now() + Duration::from_secs(5);
    while !socket_path.exists() {
        if Instant::now() > bind_deadline {
            anyhow::bail!("daemon never created socket at {}", socket_path.display());
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Daemon Attach triggers the one shared partial clone.
    let git_dir = {
        let mut s = UnixStream::connect(&socket_path)?;
        write_message(
            &mut s,
            &Request::Attach {
                source: args.url.clone(),
            },
        )?;
        match read_message::<_, Response>(&mut s)? {
            Response::Attached { git_dir } => git_dir,
            other => anyhow::bail!("daemon attach: unexpected {other:?}"),
        }
    };

    let mountpoints: Vec<PathBuf> = (0..n)
        .map(|_| make_temp("projgit-bench-sparse-pjs-mp"))
        .collect();
    let _mp_guards: Vec<DirGuard> = mountpoints.iter().cloned().map(DirGuard).collect();

    let setup = setup_start.elapsed();

    // --- measurement window ---------------------------------------------------
    let barrier = Arc::new(Barrier::new(n + 1));
    let (tx, rx) = mpsc::channel::<Result<Duration, String>>();
    let mut handles = Vec::with_capacity(n);

    for (tid, mp) in mountpoints.iter().enumerate() {
        let barrier = barrier.clone();
        let tx = tx.clone();
        let socket_path = socket_path.clone();
        let git_dir = git_dir.clone();
        let ref_name = args.ref_name.clone();
        let files = args.files.clone();
        let mp = mp.clone();

        let handle = thread::Builder::new()
            .name(format!("sparse-pjs-{tid}"))
            .spawn(move || {
                barrier.wait();
                let t0 = Instant::now();
                let result = (|| -> anyhow::Result<()> {
                    let store = Arc::new(ObjectStore::open(&git_dir)?);
                    let fetcher = DaemonFetcher::new(socket_path);
                    let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
                    let provider = Arc::new(ProjectionFsProvider::new(
                        Projection::Ref(ref_name),
                        hydrating,
                        RootOverlay::new(),
                        (tid as u64) + 300,
                    )?);
                    let session =
                        mount_background(provider, &mp, &MountConfig::default())?;
                    wait_for_mount(&mp, Duration::from_secs(10))?;
                    // Sparse-access script: ls root + read each file.
                    let _ = read_dir_names(&mp)?;
                    for f in &files {
                        let _ = std::fs::read_to_string(mp.join(f))?;
                    }
                    drop(session);
                    Ok(())
                })();
                let elapsed = t0.elapsed();
                let _ = tx.send(result.map(|_| elapsed).map_err(|e| e.to_string()));
            })?;
        handles.push(handle);
    }
    drop(tx);

    barrier.wait();
    let measurement_start = Instant::now();

    let mut per_thread = Vec::with_capacity(n);
    let mut failures = 0usize;
    for r in rx {
        match r {
            Ok(d) => per_thread.push(d),
            Err(e) => {
                failures += 1;
                eprintln!("    thread failure: {e}");
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }
    let wall_clock = measurement_start.elapsed();
    let disk_bytes = disk_bytes_of(&cache_root)?;

    // --- teardown -------------------------------------------------------------
    let mut s = UnixStream::connect(&socket_path)?;
    write_message(&mut s, &Request::Shutdown)?;
    let _: Response = read_message(&mut s)?;
    let _ = daemon_handle.join();

    Ok(SparseSharedConfig {
        setup,
        wall_clock,
        per_thread,
        disk_bytes,
        failures,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sparse_shared_partial_cat_independent(args: &Args) -> anyhow::Result<SparseSharedConfig> {
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    let n = args.concurrency;

    // --- setup window ---------------------------------------------------------
    // Pre-create N empty target dirs so the per-thread script
    // window does its OWN clone + read pass. Setup here is the
    // structural setup (dir creation) only; the N clones run in
    // the measurement window because they're per-agent cost.
    // This matches the comparator's actual deployment shape:
    // each agent does its own clone, no sharing.
    let setup_start = Instant::now();
    let clone_dirs: Vec<PathBuf> = (0..n)
        .map(|_| make_temp("projgit-bench-sparse-pci-clone"))
        .collect();
    let _guards: Vec<DirGuard> = clone_dirs.iter().cloned().map(DirGuard).collect();
    // `make_temp` creates the dir; remove it so `git clone` has
    // a clean target.
    for d in &clone_dirs {
        let _ = std::fs::remove_dir_all(d);
    }
    let setup = setup_start.elapsed();

    // --- measurement window ---------------------------------------------------
    let barrier = Arc::new(Barrier::new(n + 1));
    let (tx, rx) = mpsc::channel::<Result<Duration, String>>();
    let mut handles = Vec::with_capacity(n);

    for (tid, dir) in clone_dirs.iter().enumerate() {
        let barrier = barrier.clone();
        let tx = tx.clone();
        let url = args.url.clone();
        let ref_name = args.ref_name.clone();
        let files = args.files.clone();
        let dir = dir.clone();

        let handle = thread::Builder::new()
            .name(format!("sparse-pci-{tid}"))
            .spawn(move || {
                barrier.wait();
                let t0 = Instant::now();
                let result = (|| -> anyhow::Result<()> {
                    // Clone (per-agent, no sharing).
                    run_git(
                        None,
                        &[
                            "clone",
                            "--filter=blob:none",
                            "--no-checkout",
                            url.as_str(),
                            dir.to_str().expect("utf-8 tmp path"),
                        ],
                    )?;
                    // Sparse-access script: ls-tree root + read
                    // each file via long-lived `cat-file --batch`.
                    run_git(Some(&dir), &["ls-tree", &ref_name])?;
                    let mut child = std::process::Command::new("git")
                        .arg("-C")
                        .arg(&dir)
                        .arg("cat-file")
                        .arg("--batch")
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .spawn()?;
                    {
                        use std::io::{BufRead, BufReader, Read, Write};
                        let mut stdin = child.stdin.take().expect("piped stdin");
                        let mut stdout =
                            BufReader::new(child.stdout.take().expect("piped stdout"));
                        for f in &files {
                            writeln!(stdin, "{}:{}", ref_name, f)?;
                            stdin.flush()?;
                            let mut header = String::new();
                            let n = stdout.read_line(&mut header)?;
                            if n == 0 {
                                anyhow::bail!("git cat-file --batch closed mid-stream");
                            }
                            let size: usize = header
                                .trim_end()
                                .rsplit_once(' ')
                                .and_then(|(_, n)| n.parse().ok())
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "git cat-file --batch: bad header line: {header:?}"
                                    )
                                })?;
                            let mut buf = vec![0u8; size];
                            stdout.read_exact(&mut buf)?;
                            let mut nl = [0u8; 1];
                            stdout.read_exact(&mut nl)?;
                        }
                    }
                    let _ = child.wait();
                    Ok(())
                })();
                let elapsed = t0.elapsed();
                let _ = tx.send(result.map(|_| elapsed).map_err(|e| e.to_string()));
            })?;
        handles.push(handle);
    }
    drop(tx);

    barrier.wait();
    let measurement_start = Instant::now();

    let mut per_thread = Vec::with_capacity(n);
    let mut failures = 0usize;
    for r in rx {
        match r {
            Ok(d) => per_thread.push(d),
            Err(e) => {
                failures += 1;
                eprintln!("    thread failure: {e}");
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }
    let wall_clock = measurement_start.elapsed();
    // Sum bytes across all N clone dirs.
    let mut disk_bytes = 0u64;
    for d in &clone_dirs {
        disk_bytes += disk_bytes_of(d)?;
    }

    Ok(SparseSharedConfig {
        setup,
        wall_clock,
        per_thread,
        disk_bytes,
        failures,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn print_sparse_shared_report(args: &Args, samples: &[SparseSharedSample]) {
    let med_axis = |xs: &[u128]| -> u128 {
        let mut v: Vec<u128> = xs.to_vec();
        v.sort();
        v[v.len() / 2]
    };
    let med_dur = |samples: &[SparseSharedSample], pick: fn(&SparseSharedSample) -> Duration| {
        let xs: Vec<u128> = samples.iter().map(|s| pick(s).as_micros()).collect();
        Duration::from_micros(med_axis(&xs) as u64)
    };
    let med_bytes = |samples: &[SparseSharedSample], pick: fn(&SparseSharedSample) -> u64| -> u64 {
        let xs: Vec<u128> = samples.iter().map(|s| pick(s) as u128).collect();
        med_axis(&xs) as u64
    };

    let pjs_setup = med_dur(samples, |s| s.projgit_shared.setup);
    let pjs_wall = med_dur(samples, |s| s.projgit_shared.wall_clock);
    let pjs_disk = med_bytes(samples, |s| s.projgit_shared.disk_bytes);
    let pjs_fail: usize = samples.iter().map(|s| s.projgit_shared.failures).sum();
    let pci_setup = med_dur(samples, |s| s.partial_cat_independent.setup);
    let pci_wall = med_dur(samples, |s| s.partial_cat_independent.wall_clock);
    let pci_disk = med_bytes(samples, |s| s.partial_cat_independent.disk_bytes);
    let pci_fail: usize = samples
        .iter()
        .map(|s| s.partial_cat_independent.failures)
        .sum();

    // Per-thread aggregations across all iterations.
    let pjs_per_thread: Vec<Duration> = samples
        .iter()
        .flat_map(|s| s.projgit_shared.per_thread.clone())
        .collect();
    let pci_per_thread: Vec<Duration> = samples
        .iter()
        .flat_map(|s| s.partial_cat_independent.per_thread.clone())
        .collect();
    let p50 = |xs: &[Duration]| -> Duration {
        if xs.is_empty() {
            return Duration::ZERO;
        }
        let mut v: Vec<u128> = xs.iter().map(|d| d.as_micros()).collect();
        v.sort();
        Duration::from_micros(v[v.len() / 2] as u64)
    };
    let pjs_p50 = p50(&pjs_per_thread);
    let pci_p50 = p50(&pci_per_thread);

    println!("# bench_mount: {} @ {}\n", args.url, args.ref_name);
    println!(
        "Sparse-access multi-agent. Median of {} iterations, N={}. Times in ms; disk in KiB.\n",
        args.iterations, args.concurrency
    );
    println!(
        "Script (per agent): `ls` mount root + read {} file(s): {:?}\n",
        args.files.len(),
        args.files
    );
    println!("| Config | setup | wall clock | per-thread p50 | disk total | failures |");
    println!("|---|---:|---:|---:|---:|---:|");
    println!(
        "| `projgit-shared` (1 daemon + N sidecars + 1 CAS) | {} | {} | {} | {} | {} |",
        ms(pjs_setup),
        ms(pjs_wall),
        ms(pjs_p50),
        pjs_disk / 1024,
        pjs_fail,
    );
    println!(
        "| `partial-cat-independent` (N independent clones) | {} | {} | {} | {} | {} |",
        ms(pci_setup),
        ms(pci_wall),
        ms(pci_p50),
        pci_disk / 1024,
        pci_fail,
    );
    let wall_ratio = pci_wall.as_secs_f64() / pjs_wall.as_secs_f64();
    let disk_ratio = pci_disk as f64 / pjs_disk.max(1) as f64;
    println!();
    println!(
        "Ratios (partial-cat-independent / projgit-shared): wall {wall_ratio:.2}x, disk {disk_ratio:.2}x"
    );
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn time_it<T, E, F>(f: F) -> Result<Duration, E>
where
    F: FnOnce() -> Result<T, E>,
{
    let t = Instant::now();
    f()?;
    Ok(t.elapsed())
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn make_temp(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Process-local monotonic counter so N parallel threads in the
    // `*-concurrent` scenarios can't collide on `(pid, nanos)` — the
    // same lesson the dotgit_index flake fix learned.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "{label}-{}-{}-{id}",
        std::process::id(),
        Instant::now().elapsed().as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_mount(mountpoint: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let parent_dev = mountpoint
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(0);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(m) = std::fs::metadata(mountpoint) {
            if m.dev() != parent_dev {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    anyhow::bail!("mountpoint never became a FUSE mount within {timeout:?}");
}
