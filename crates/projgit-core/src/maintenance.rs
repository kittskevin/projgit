//! Background-maintenance helpers for the shared CAS.
//!
//! Implements the object-store half of the Scalar playbook (see
//! `docs/design/cache-transform-tier.md` §14): multi-pack-index +
//! incremental repack + commit-graph. This is what converts
//! content-addressing's *logical* cross-commit dedup into *physical*
//! dedup — one disk copy / one page-cache page per object, shared
//! across every commit that references it — and keeps object lookup
//! fast as many per-commit fetch-packs accumulate.
//!
//! MVP shells to the system `git` (the CAS is stock-git format, so
//! `git maintenance` is the natural tool and keeps the store
//! tooling-readable); a gix-native path is a later "drop the git CLI"
//! item. The daemon runs this off the serving path; it is safe beside
//! live `mmap` readers because git writes new packs / MIDX /
//! commit-graph via temp-file + atomic rename and never mutates a
//! published pack.

use std::path::Path;
use std::process::Command;

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
}
