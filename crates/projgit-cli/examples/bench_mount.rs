//! Network-gated benchmark: time representative access patterns through
//! projgit vs. system git's partial-clone path.
//!
//! Runs against `https://github.com/rust-lang/log` by default; pass
//! `--url <URL> --ref <NAME>` for a different target. Doubly gated:
//! needs `git` on `PATH` and `PROJGIT_NETWORK_TESTS=1` set.
//!
//! ```sh
//! PROJGIT_NETWORK_TESTS=1 \
//!   cargo run -p projgit-cli --example bench_mount --release
//! ```
//!
//! Cfg-gated to Linux + macOS because the projgit side mounts via
//! FUSE; on other targets the binary prints a friendly message.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "https://github.com/rust-lang/log";
const DEFAULT_REF: &str = "master";
const FILES: &[&str] = &["Cargo.toml", "src/lib.rs", "LICENSE-APACHE"];
const ITERATIONS: usize = 3;

#[derive(Debug, Clone)]
struct Args {
    url: String,
    ref_name: String,
}

fn parse_args() -> Args {
    let mut url = DEFAULT_URL.to_owned();
    let mut ref_name = DEFAULT_REF.to_owned();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => url = it.next().expect("--url needs a value"),
            "--ref" => ref_name = it.next().expect("--ref needs a value"),
            "-h" | "--help" => {
                eprintln!(
                    "usage: bench_mount [--url <URL>] [--ref <NAME>]\n  default URL: {DEFAULT_URL}\n  default ref: {DEFAULT_REF}",
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    Args { url, ref_name }
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
        "bench_mount: {} @ {} ({} iterations)\n",
        args.url, args.ref_name, ITERATIONS
    );

    let mut projgit_samples: Vec<ProjgitSample> = Vec::with_capacity(ITERATIONS);
    let mut git_samples: Vec<GitSample> = Vec::with_capacity(ITERATIONS);

    for i in 1..=ITERATIONS {
        eprintln!("== iteration {i}/{ITERATIONS} ==");

        eprint!("  projgit...   ");
        let p = bench_projgit(&args)?;
        eprintln!("ok");
        projgit_samples.push(p);

        eprint!("  git baseline ");
        let g = bench_git_baseline(&args)?;
        eprintln!("ok");
        git_samples.push(g);
    }

    eprintln!();
    print_report(&args, &projgit_samples, &git_samples);
    Ok(())
}

// -----------------------------------------------------------------------------
// projgit side
// -----------------------------------------------------------------------------

/// Wall-clock timings for a single projgit iteration (one fresh
/// partial clone + one mount).
#[derive(Debug, Clone)]
struct ProjgitSample {
    /// `git clone --filter=blob:none --no-checkout` time.
    partial_clone: Duration,
    /// First `read_dir` of mountpoint root.
    cold_readdir_root: Duration,
    /// First recursive walk via projgit.
    cold_walk: Duration,
    /// First `read_to_string` of [`FILES`].
    cold_cat: Duration,
    /// Second `read_dir` of mountpoint root.
    warm_readdir_root: Duration,
    /// Second recursive walk.
    warm_walk: Duration,
    /// Second `read_to_string` of the same files.
    warm_cat: Duration,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bench_projgit(args: &Args) -> anyhow::Result<ProjgitSample> {
    use projgit_core::{
        clone::{git_dir_for, partial_clone, CloneOptions},
        GitCliFetcher, HydratingObjectStore, ObjectStore, Projection, ProjectionFsProvider,
        RootOverlay,
    };
    use projgit_fuse::{mount_background, MountConfig};
    use std::sync::Arc;

    let cache_dir = make_temp("projgit-bench-cache");
    let _cache_guard = DirGuard(cache_dir.clone());
    let mountpoint = make_temp("projgit-bench-mp");
    let _mp_guard = DirGuard(mountpoint.clone());

    // Partial clone (counted separately so the table is honest about
    // where the time goes).
    let partial_clone_t = time_it(|| {
        let opts = CloneOptions::new(args.url.clone(), cache_dir.clone());
        partial_clone(&opts)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    })?;

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

    let cold_readdir_root = time_it(|| {
        let _ = read_dir_names(&mountpoint)?;
        Ok::<_, anyhow::Error>(())
    })?;
    let cold_walk = time_it(|| {
        let _ = walk_count(&mountpoint)?;
        Ok::<_, anyhow::Error>(())
    })?;
    let cold_cat = time_it(|| {
        for f in FILES {
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
        for f in FILES {
            let _ = std::fs::read_to_string(mountpoint.join(f))?;
        }
        Ok::<_, anyhow::Error>(())
    })?;

    drop(session);
    Ok(ProjgitSample {
        partial_clone: partial_clone_t,
        cold_readdir_root,
        cold_walk,
        cold_cat,
        warm_readdir_root,
        warm_walk,
        warm_cat,
    })
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
        for f in FILES {
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
        "Median of {} iterations. All times in milliseconds.\n",
        ITERATIONS
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
        FILES.len(),
        ms(p_cold_cat),
        ms(p_warm_cat),
        ms(g_cat)
    );
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
    let p = std::env::temp_dir().join(format!(
        "{label}-{}-{}",
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
