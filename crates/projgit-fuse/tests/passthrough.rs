//! Runtime integration tests for Linux 6.9+ FUSE passthrough (`FOPEN_PASSTHROUGH`).
//!
//! Exercises:
//! 1. Disabling passthrough via `MountConfig::passthrough = false`.
//! 2. Concurrent multi-threaded opens of the same inode (`n_threads = Some(4)`),
//!    verifying there are no `-EIO` failures from `FOPEN_KEEP_CACHE`, mismatched
//!    `BackingId`s, or mixed caching/passthrough opens.
//! 3. Size-gated hydrate-on-`open()` (`passthrough_max_bytes`) + `cached_backing_file`
//!    promotion once a large blob is materialized on disk.
//! 4. Bounded idle `BackingId` LRU eviction + explicit `evict_idle_backing_files()`
//!    coordinated with `prune_uncompressed_blobs()`.
//! 5. Unprivileged `EPERM` latching (only 1 fallback instead of failing on every open).
//! 6. Stacking kernel `overlayfs` (`--writable-scratch`) on top of a passthrough
//!    FUSE mount (`max_stack_depth = 1`).
//!
//! Run unprivileged (exercises fallback latching or passthrough if CAP_SYS_ADMIN):
//! ```sh
//! cargo test -p projgit-fuse --test passthrough -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Run privileged on Linux 6.9+ to strictly require active `FOPEN_PASSTHROUGH`:
//! ```sh
//! sudo -E PROJGIT_REQUIRE_PASSTHROUGH=1 \
//!   cargo test -p projgit-fuse --test passthrough -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(any(target_os = "linux", target_os = "macos"))]

use projgit_core::{
    prune_uncompressed_blobs, HydratingObjectStore, NoopFetcher, ObjectStore, Projection,
    ProjectionFsProvider, RootOverlay,
};
use projgit_fuse::{mount_managed_background, MountConfig};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn require_passthrough() -> bool {
    std::env::var("PROJGIT_REQUIRE_PASSTHROUGH")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "projgit-test")
        .env("GIT_AUTHOR_EMAIL", "test@projgit.invalid")
        .env("GIT_COMMITTER_NAME", "projgit-test")
        .env("GIT_COMMITTER_EMAIL", "test@projgit.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_temp_dir(name: &str) -> (PathBuf, DirGuard) {
    let dir = std::env::temp_dir().join(format!("projgit-pt-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let guard = DirGuard(dir.clone());
    (dir, guard)
}

fn wait_for_mount(mountpoint: &Path, timeout: Duration) -> bool {
    let parent_dev = mountpoint
        .parent()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev())
        .unwrap_or(0);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(m) = std::fs::metadata(mountpoint) {
            if m.dev() != parent_dev {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn wait_for_no_active_opens(
    session: &projgit_fuse::ManagedMountSession,
    timeout: Duration,
) -> projgit_fuse::PassthroughStats {
    let start = Instant::now();
    loop {
        let stats = session.passthrough_stats().expect("passthrough stats");
        if stats.active_inodes == 0 && stats.active_standard_inodes == 0 {
            return stats;
        }
        if start.elapsed() >= timeout {
            return stats;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Build a fixture repo with 3 small files (< 1 KiB) and 1 packed large file (128 KiB).
fn build_passthrough_fixture(name: &str) -> (PathBuf, DirGuard, gix::ObjectId, Vec<u8>) {
    let (repo, guard) = make_temp_dir(name);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "core.autocrlf", "false"]);

    std::fs::write(repo.join("a.txt"), b"alpha file contents\n").unwrap();
    std::fs::write(repo.join("b.txt"), b"beta file contents\n").unwrap();
    std::fs::write(repo.join("c.txt"), b"gamma file contents\n").unwrap();

    let big_bytes: Vec<u8> = (0..(128 * 1024)).map(|i| (i % 251) as u8).collect();
    std::fs::write(repo.join("big.bin"), &big_bytes).unwrap();

    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "passthrough fixture"]);
    git(&repo, &["repack", "-a", "-d", "-q"]);

    let head_hex = String::from_utf8(git(&repo, &["rev-parse", "HEAD"])).unwrap();
    let head = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).expect("valid hex");
    (repo, guard, head, big_bytes)
}

#[test]
#[ignore = "requires FUSE (/dev/fuse)"]
fn passthrough_disabled_via_config_serves_files_with_zero_hits() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let (repo, _repo_guard, head, _big) = build_passthrough_fixture("disabled");
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(Projection::Commit(head), hydrating, RootOverlay::new(), 1)
            .unwrap(),
    );

    let (mp, _mp_guard) = make_temp_dir("mp-disabled");
    let cfg = MountConfig {
        passthrough: false,
        ..MountConfig::default()
    };

    let session = mount_managed_background(provider, &mp, &cfg).expect("mount_managed_background");
    assert!(wait_for_mount(&mp, Duration::from_secs(5)));

    assert_eq!(
        std::fs::read_to_string(mp.join("a.txt")).unwrap(),
        "alpha file contents\n"
    );

    let stats = session.passthrough_stats().expect("passthrough stats");
    assert!(!stats.requested);
    assert!(!stats.negotiated);
    assert!(!stats.disabled_by_kernel);
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.fallbacks, 0);
    drop(session);
}

#[test]
#[ignore = "requires FUSE (/dev/fuse); set PROJGIT_REQUIRE_PASSTHROUGH=1 with CAP_SYS_ADMIN"]
fn passthrough_concurrent_opens_size_gate_and_idle_eviction() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }

    let (repo, _repo_guard, head, big_bytes) = build_passthrough_fixture("concurrent");
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(Projection::Commit(head), hydrating, RootOverlay::new(), 1)
            .unwrap(),
    );

    let (mp, _mp_guard) = make_temp_dir("mp-concurrent");
    let cfg = MountConfig {
        passthrough: true,
        passthrough_max_bytes: 1024, // 1 KiB: small files hydrate-on-open, 128 KiB big.bin skips until resident
        passthrough_idle_cache_capacity: 2,
        n_threads: cfg!(target_os = "linux").then_some(4),
        ..MountConfig::default()
    };

    let session = mount_managed_background(provider, &mp, &cfg).expect("mount_managed_background");
    assert!(wait_for_mount(&mp, Duration::from_secs(5)));

    // 1. Concurrent opens of the same small file while all 8 handles stay open simultaneously.
    //    Directly exercises Bug A (FOPEN_KEEP_CACHE), Bug B (duplicate open_backing / EIO),
    //    and Bug C (mixed caching/passthrough mode).
    //    Every thread unconditionally crosses both barriers even on error so one failure
    //    cannot strand the remaining threads.
    let barrier = Arc::new(Barrier::new(8));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let path = mp.join("a.txt");
        let b = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || -> std::io::Result<String> {
            let file_res = std::fs::File::open(&path);
            b.wait();
            let read_res = file_res.and_then(|mut f| {
                let mut s = String::new();
                f.read_to_string(&mut s)?;
                Ok(s)
            });
            b.wait();
            read_res
        }));
    }
    for t in threads {
        let s = t
            .join()
            .expect("reader thread panicked")
            .expect("concurrent open/read a.txt");
        assert_eq!(s, "alpha file contents\n");
    }

    // 2. Open large unmaterialized file (128 KiB > 1 KiB cap): must succeed via standard
    //    read_blob_range (which materializes it into L2) without stalling open().
    let read_big_1 = std::fs::read(mp.join("big.bin")).expect("first read big.bin");
    assert_eq!(read_big_1, big_bytes);

    // Wait for async FUSE_RELEASE of the Standard open before reopening big.bin, so the
    // second open sees `open_count == 0` and promotes via `cached_backing_file()`.
    let _ = wait_for_no_active_opens(&session, Duration::from_secs(2));

    // Second open of big.bin: now resident in L2, so cached_backing_file() promotes it to passthrough!
    let read_big_2 = std::fs::read(mp.join("big.bin")).expect("second read big.bin");
    assert_eq!(read_big_2, big_bytes);

    // 3. Open b.txt and c.txt to exceed `passthrough_idle_cache_capacity = 2`.
    assert_eq!(
        std::fs::read_to_string(mp.join("b.txt")).unwrap(),
        "beta file contents\n"
    );
    assert_eq!(
        std::fs::read_to_string(mp.join("c.txt")).unwrap(),
        "gamma file contents\n"
    );

    // Wait for async FUSE_RELEASEs so all handles transition from active -> idle_lru.
    let stats_before_evict = wait_for_no_active_opens(&session, Duration::from_secs(2));
    assert_eq!(stats_before_evict.active_inodes, 0);
    assert_eq!(stats_before_evict.active_standard_inodes, 0);
    eprintln!("passthrough stats before evict: {stats_before_evict:?}");
    assert!(stats_before_evict.requested);

    if require_passthrough() {
        assert!(
            stats_before_evict.negotiated,
            "PROJGIT_REQUIRE_PASSTHROUGH=1 set, expected FUSE_PASSTHROUGH negotiated (needs Linux 6.9+)"
        );
        assert!(
            !stats_before_evict.disabled_by_kernel,
            "PROJGIT_REQUIRE_PASSTHROUGH=1 set, expected CAP_SYS_ADMIN (got disabled_by_kernel)"
        );
        assert!(
            stats_before_evict.hits >= 11,
            "expected >= 11 passthrough hits (8 concurrent a.txt + 2nd big.bin + b.txt + c.txt), got {}",
            stats_before_evict.hits
        );
        assert_eq!(
            stats_before_evict.fallbacks, 0,
            "expected zero fallbacks when passthrough is active"
        );
        assert!(
            stats_before_evict.skipped_too_large >= 1,
            "expected first open of 128 KiB big.bin to skip hydrate-on-open"
        );
        assert!(
            stats_before_evict.idle_cached_inodes <= 2,
            "idle backing cache must obey capacity=2, got {}",
            stats_before_evict.idle_cached_inodes
        );
        assert!(
            stats_before_evict.idle_evictions >= 1,
            "expected LRU idle eviction after opening 4 distinct files with cap=2"
        );
    } else if stats_before_evict.disabled_by_kernel {
        // Unprivileged Linux 6.9+: must latch off after the first EPERM
        // rather than retrying open_backing() on every single open().
        // Under 8 concurrent first-opens serialized by the per-inode slot,
        // exactly 1 fallback occurs before `disabled_by_kernel` latches.
        assert_eq!(
            stats_before_evict.fallbacks, 1,
            "expected EPERM to latch after 1 fallback, got {}",
            stats_before_evict.fallbacks
        );
    }

    // 4. Explicit idle eviction + L2 blob prune to 0 bytes, then re-read to prove
    //    transparent re-inflation works cleanly.
    let _evicted = session.evict_idle_backing_files();
    let stats_after_evict = session.passthrough_stats().expect("passthrough stats");
    assert_eq!(stats_after_evict.idle_cached_inodes, 0);

    let prune = prune_uncompressed_blobs(&repo.join(".git"), 0).expect("prune_uncompressed_blobs");
    assert_eq!(prune.remaining_bytes, 0);

    assert_eq!(
        std::fs::read_to_string(mp.join("a.txt")).unwrap(),
        "alpha file contents\n"
    );

    drop(session);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires Linux CAP_SYS_ADMIN + overlayfs + /dev/fuse"]
fn passthrough_overlayfs_writable_scratch_stacking() {
    if !git_available() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    if !require_passthrough() {
        eprintln!("SKIP: set PROJGIT_REQUIRE_PASSTHROUGH=1 (with CAP_SYS_ADMIN) to run overlayfs stacking test");
        return;
    }

    let (repo, _repo_guard, head, _big) = build_passthrough_fixture("overlay");
    let store = Arc::new(ObjectStore::open(&repo).unwrap());
    let hydrating = Arc::new(HydratingObjectStore::new(store, NoopFetcher));
    let provider = Arc::new(
        ProjectionFsProvider::new(Projection::Commit(head), hydrating, RootOverlay::new(), 1)
            .unwrap(),
    );

    let (mp, _mp_guard) = make_temp_dir("mp-overlay");
    let (scratch, _scratch_guard) = make_temp_dir("scratch-overlay");
    let cfg = MountConfig {
        passthrough: true,
        writable_scratch: Some(scratch),
        ..MountConfig::default()
    };

    let session = mount_managed_background(provider, &mp, &cfg)
        .expect("mount_managed_background with writable_scratch");
    assert!(wait_for_mount(&mp, Duration::from_secs(5)));

    // 1. Read unmodified file through overlayfs -> FUSE passthrough (max_stack_depth=1).
    assert_eq!(
        std::fs::read_to_string(mp.join("a.txt")).unwrap(),
        "alpha file contents\n"
    );
    let stats = session.passthrough_stats().expect("passthrough stats");
    eprintln!("overlayfs + passthrough stats: {stats:?}");
    assert!(stats.negotiated && !stats.disabled_by_kernel);
    assert!(
        stats.hits >= 1,
        "expected overlayfs lower read to hit FUSE passthrough"
    );
    assert_eq!(stats.fallbacks, 0);

    // 2. Copy-up write + new file creation in overlayfs upperdir.
    std::fs::write(mp.join("a.txt"), b"edited via overlayfs\n").expect("copy-up write");
    std::fs::write(mp.join("created.txt"), b"brand new file\n").expect("create file");
    assert_eq!(
        std::fs::read_to_string(mp.join("a.txt")).unwrap(),
        "edited via overlayfs\n"
    );
    assert_eq!(
        std::fs::read_to_string(mp.join("created.txt")).unwrap(),
        "brand new file\n"
    );

    drop(session);
}
