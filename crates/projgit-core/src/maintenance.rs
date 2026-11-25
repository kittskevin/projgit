//! Background-maintenance helpers for the shared CAS.
//!
//! Implements the object-store half of the Scalar playbook (see
//! `docs/design/cache-transform-tier.md` §14): multi-pack-index +
//! incremental repack + commit-graph, plus bounded LRU (`atime`/`mtime`)
//! eviction for the on-disk uncompressed L2 blob cache
//! (`<git-dir>/blobs/<xx>/<oid>`). This is what converts
//! content-addressing's *logical* cross-commit dedup into *physical*
//! dedup — one disk copy / one page-cache page per object, shared
//! across every commit that references it — and keeps object lookup
//! fast as many per-commit fetch-packs accumulate while bounding the
//! uncompressed L2 disk footprint.
//!
//! MVP shells to the system `git` (the CAS is stock-git format, so
//! `git maintenance` is the natural tool and keeps the store
//! tooling-readable); a gix-native path is a later "drop the git CLI"
//! item. The daemon runs this off the serving path; it is safe beside
//! live `mmap` readers because git writes new packs / MIDX /
//! commit-graph via temp-file + atomic rename and never mutates a
//! published pack.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Summary of a single [`prune_uncompressed_blobs`] pass over `<git-dir>/blobs/`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BlobEvictionStats {
    /// Total uncompressed blob files discovered before eviction.
    pub scanned_files: usize,
    /// Total bytes across all discovered uncompressed blob files before eviction.
    pub scanned_bytes: u64,
    /// Number of blob files unlinked to meet the byte budget.
    pub evicted_files: usize,
    /// Total bytes reclaimed by unlinking evicted blob files.
    pub evicted_bytes: u64,
    /// Number of blob files remaining after eviction.
    pub remaining_files: usize,
    /// Total bytes remaining across uncompressed blob files after eviction.
    pub remaining_bytes: u64,
    /// Number of file-removal errors encountered (logged/ignored so eviction continues).
    pub remove_errors: usize,
}

#[derive(Debug)]
struct BlobCandidate {
    last_used: SystemTime,
    path: PathBuf,
    size: u64,
}

/// Prune the on-disk uncompressed L2 blob cache (`<git-dir>/blobs/<xx>/<oid>`)
/// down to at most `max_bytes` using LRU (`atime`/`mtime`) eviction.
///
/// Only inspects 2-character ASCII hex fanout directories (`00`..`ff`) under
/// `<git-dir>/blobs/`, so `<git-dir>/blobs/headers.bin`,
/// `<git-dir>/blobs/tmp/`, and canonical packfiles under
/// `<git-dir>/objects/pack/` are never touched.
///
/// Safe to run concurrently with active `FOPEN_PASSTHROUGH` or `virtio-fs`
/// `DAX` readers: unlinking a materialized blob removes only its directory
/// entry while open file descriptors remain valid until closed, and any
/// future cold read transparently re-inflates the blob from `.git/objects/pack/`.
pub fn prune_uncompressed_blobs(
    git_dir: &Path,
    max_bytes: u64,
) -> std::io::Result<BlobEvictionStats> {
    let blobs_dir = git_dir.join("blobs");
    if !blobs_dir.is_dir() {
        return Ok(BlobEvictionStats::default());
    }

    let mut stats = BlobEvictionStats::default();
    let mut candidates: Vec<BlobCandidate> = Vec::new();

    for fanout_entry in std::fs::read_dir(&blobs_dir)? {
        let Ok(fanout_entry) = fanout_entry else {
            continue;
        };
        let file_name = fanout_entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !(name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit())) {
            continue;
        }
        let fanout_path = fanout_entry.path();
        if !fanout_path.is_dir() {
            continue;
        }
        let Ok(children) = std::fs::read_dir(&fanout_path) else {
            continue;
        };
        for blob_entry in children {
            let Ok(blob_entry) = blob_entry else {
                continue;
            };
            let path = blob_entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"))
            {
                continue;
            }
            let Ok(meta) = blob_entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let size = meta.len();
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let atime = meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
            let last_used = match meta.created() {
                Ok(created)
                    if mtime < created && atime <= created + std::time::Duration::from_secs(2) =>
                {
                    mtime
                }
                _ => mtime.max(atime),
            };

            stats.scanned_files += 1;
            stats.scanned_bytes = stats.scanned_bytes.saturating_add(size);
            candidates.push(BlobCandidate {
                last_used,
                path,
                size,
            });
        }
    }

    stats.remaining_files = stats.scanned_files;
    stats.remaining_bytes = stats.scanned_bytes;

    if stats.scanned_bytes <= max_bytes {
        return Ok(stats);
    }

    candidates.sort_by(|a, b| {
        a.last_used
            .cmp(&b.last_used)
            .then_with(|| a.path.cmp(&b.path))
    });

    for candidate in candidates {
        if stats.remaining_bytes <= max_bytes {
            break;
        }
        match remove_blob_file(&candidate.path) {
            Ok(()) => {
                stats.evicted_files += 1;
                stats.evicted_bytes = stats.evicted_bytes.saturating_add(candidate.size);
                stats.remaining_files = stats.remaining_files.saturating_sub(1);
                stats.remaining_bytes = stats.remaining_bytes.saturating_sub(candidate.size);
            }
            Err(_) => {
                stats.remove_errors += 1;
            }
        }
    }

    Ok(stats)
}

fn remove_blob_file(path: &Path) -> std::io::Result<()> {
    if std::fs::remove_file(path).is_ok() {
        return Ok(());
    }
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(perms.mode() | 0o200);
        }
        #[cfg(not(unix))]
        #[allow(clippy::permissions_set_readonly_false)]
        {
            perms.set_readonly(false);
        }
        let _ = std::fs::set_permissions(path, perms);
    }
    std::fs::remove_file(path)
}

/// Errors from [`run_maintenance`].
#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    /// `git` is not on `PATH` or could not be spawned.
    #[error("git CLI not available: {0}")]
    GitUnavailable(String),
    /// `git maintenance` exited non-zero.
    #[error("git maintenance failed (status {status}): {stderr}")]
    Failed {
        /// Process exit code (`-1` if terminated by signal).
        status: i32,
        /// Captured stderr, trimmed.
        stderr: String,
    },
}

/// Run incremental object-store maintenance on `git_dir`:
/// `git maintenance run --task=incremental-repack --task=commit-graph`.
///
/// - **incremental-repack** writes a multi-pack-index and geometrically
///   repacks small packs, so each object becomes physically singular
///   (cross-commit disk + page-cache dedup) while lookup stays
///   `O(log total)` across packs.
/// - **commit-graph** keeps `git log` / merge-base fast inside mounts.
///
/// Promisor-safe (never prunes promisor objects). Idempotent: a run
/// with nothing to do is a cheap no-op.
pub fn run_maintenance(git_dir: &Path) -> Result<(), MaintenanceError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args([
            "maintenance",
            "run",
            "--task=incremental-repack",
            "--task=commit-graph",
        ])
        .output()
        .map_err(|e| MaintenanceError::GitUnavailable(e.to_string()))?;

    if out.status.success() {
        Ok(())
    } else {
        Err(MaintenanceError::Failed {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn run_maintenance_succeeds_idempotently_and_preserves_history() {
        let base = std::env::temp_dir().join(format!("projgit-maint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        if Command::new("git")
            .args(["init", "-q", "-b", "main", base.to_str().unwrap()])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("SKIP: git CLI not available");
            return;
        }
        git(&base, &["config", "user.email", "t@e.invalid"]);
        git(&base, &["config", "user.name", "T"]);
        // A few commits, each repacked, so there is real history + packs
        // for maintenance to index.
        for i in 0..3 {
            std::fs::write(base.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            git(&base, &["add", "-A"]);
            git(&base, &["commit", "-q", "-m", &format!("c{i}")]);
            git(&base, &["repack", "-a", "-d", "-q"]);
        }

        let git_dir = base.join(".git");
        run_maintenance(&git_dir).expect("maintenance run ok");
        // Safe / cheap to repeat.
        run_maintenance(&git_dir).expect("maintenance run idempotent");

        // Maintenance must not lose or corrupt history. (We don't assert
        // specific MIDX / commit-graph files: the `git maintenance` tasks
        // are heuristic about when they write, which is intentional.)
        let out = Command::new("git")
            .arg("-C")
            .arg(&git_dir)
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .expect("git rev-list");
        assert!(out.status.success(), "repo readable after maintenance");
        let count: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("commit count");
        assert_eq!(count, 3, "all 3 commits still reachable after maintenance");

        let _ = std::fs::remove_dir_all(&base);
    }

    fn set_blob_timestamp(path: &Path, time: SystemTime) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o644);
        }
        #[cfg(not(unix))]
        #[allow(clippy::permissions_set_readonly_false)]
        {
            perms.set_readonly(false);
        }
        std::fs::set_permissions(path, perms).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let times = std::fs::FileTimes::new()
            .set_accessed(time)
            .set_modified(time);
        file.set_times(times).unwrap();
        file.set_modified(time).unwrap();
    }

    #[test]
    fn prune_uncompressed_blobs_noop_when_under_budget_and_evicts_oldest_first() {
        use std::time::Duration;

        let base = std::env::temp_dir().join(format!("projgit-prune-blobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let blobs_dir = base.join("blobs");

        // Missing blobs/ dir is a clean no-op.
        let empty_stats = prune_uncompressed_blobs(&base, 1024).unwrap();
        assert_eq!(empty_stats, BlobEvictionStats::default());

        // Create fanout directories and non-blob files (headers.bin + tmp/*)
        let dir_0a = blobs_dir.join("0a");
        let dir_ff = blobs_dir.join("ff");
        let dir_tmp = blobs_dir.join("tmp");
        std::fs::create_dir_all(&dir_0a).unwrap();
        std::fs::create_dir_all(&dir_ff).unwrap();
        std::fs::create_dir_all(&dir_tmp).unwrap();

        let headers_bin = blobs_dir.join("headers.bin");
        std::fs::write(&headers_bin, b"PGHD\x00\x00\x00\x01dummy-header-memo").unwrap();
        let staging_tmp = dir_tmp.join("inflight.tmp");
        std::fs::write(&staging_tmp, vec![0xAAu8; 500]).unwrap();
        let fanout_tmp = dir_0a.join("stray.tmp");
        std::fs::write(&fanout_tmp, vec![0xBBu8; 500]).unwrap();

        let blob_oldest = dir_0a.join("0a11111111111111111111111111111111111111");
        let blob_middle = dir_0a.join("0a22222222222222222222222222222222222222");
        let blob_newest = dir_ff.join("ff33333333333333333333333333333333333333");

        std::fs::write(&blob_oldest, vec![1u8; 100]).unwrap();
        std::fs::write(&blob_middle, vec![2u8; 200]).unwrap();
        std::fs::write(&blob_newest, vec![3u8; 300]).unwrap();

        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_blob_timestamp(&blob_oldest, t0);
        set_blob_timestamp(&blob_middle, t0 + Duration::from_secs(60));
        set_blob_timestamp(&blob_newest, t0 + Duration::from_secs(120));

        // 1. Under or equal to budget (600 bytes total): no-op.
        let noop = prune_uncompressed_blobs(&base, 600).unwrap();
        assert_eq!(
            noop,
            BlobEvictionStats {
                scanned_files: 3,
                scanned_bytes: 600,
                evicted_files: 0,
                evicted_bytes: 0,
                remaining_files: 3,
                remaining_bytes: 600,
                remove_errors: 0,
            }
        );
        assert!(blob_oldest.exists());
        assert!(blob_middle.exists());
        assert!(blob_newest.exists());

        // 2. Budget = 500 bytes: only the oldest (100B) should be evicted, leaving 500B.
        let pass1 = prune_uncompressed_blobs(&base, 500).unwrap();
        assert_eq!(
            pass1,
            BlobEvictionStats {
                scanned_files: 3,
                scanned_bytes: 600,
                evicted_files: 1,
                evicted_bytes: 100,
                remaining_files: 2,
                remaining_bytes: 500,
                remove_errors: 0,
            }
        );
        assert!(!blob_oldest.exists(), "oldest blob must be evicted first");
        assert!(blob_middle.exists(), "middle blob must remain");
        assert!(blob_newest.exists(), "newest blob must remain");

        // 3. Budget = 300 bytes: middle (200B) evicted, newest (300B) remains.
        let pass2 = prune_uncompressed_blobs(&base, 300).unwrap();
        assert_eq!(
            pass2,
            BlobEvictionStats {
                scanned_files: 2,
                scanned_bytes: 500,
                evicted_files: 1,
                evicted_bytes: 200,
                remaining_files: 1,
                remaining_bytes: 300,
                remove_errors: 0,
            }
        );
        assert!(!blob_middle.exists());
        assert!(blob_newest.exists());

        // headers.bin and tmp files must never be touched even with max_bytes = 0.
        let pass3 = prune_uncompressed_blobs(&base, 0).unwrap();
        assert_eq!(pass3.evicted_files, 1);
        assert_eq!(pass3.remaining_files, 0);
        assert_eq!(pass3.remaining_bytes, 0);
        assert!(headers_bin.is_file(), "headers.bin must never be deleted");
        assert!(staging_tmp.is_file(), "blobs/tmp/* must never be deleted");
        assert!(fanout_tmp.is_file(), "*.tmp files must be skipped");

        let _ = std::fs::remove_dir_all(&base);
    }
}
