//! Integration test for `HydratingObjectStore::warm_tree_closure`:
//! a mount-time eager tree warm makes every tree in the commit's
//! closure resident (so `readdir` / `stat` are network-free) without
//! fetching blobs. Needs `git` on PATH; the fixture is built locally
//! so a `NoopFetcher` suffices (every object is already present).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use projgit_core::object_store::ObjectStore;
use projgit_core::{HydratingObjectStore, NoopFetcher};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
fn next_unique_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn git(repo: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git available on PATH");
    if !out.status.success() {
        panic!(
            "git {args:?} failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.stdout
}

/// Three trees: root, `src/`, `src/util/`.
fn build_nested_fixture() -> (PathBuf, gix::ObjectId) {
    let base = std::env::temp_dir().join(format!(
        "projgit-warm-{}-{}",
        std::process::id(),
        next_unique_id(),
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    git(&base, &["init", "-q", "-b", "main"]);
    git(&base, &["config", "user.email", "test@example.invalid"]);
    git(&base, &["config", "user.name", "Test"]);

    std::fs::write(base.join("README.md"), b"# fixture\n").unwrap();
    let util_dir = base.join("src").join("util");
    std::fs::create_dir_all(&util_dir).unwrap();
    std::fs::write(base.join("src").join("main.c"), b"int main(void){return 0;}\n").unwrap();
    std::fs::write(util_dir.join("helper.c"), b"void helper(){}\n").unwrap();

    git(&base, &["add", "-A"]);
    git(&base, &["commit", "-q", "-m", "initial"]);

    let head_hex = String::from_utf8(git(&base, &["rev-parse", "HEAD"])).unwrap();
    let head = gix::ObjectId::from_hex(head_hex.trim().as_bytes()).expect("valid hex");
    (base, head)
}

struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn warm_tree_closure_walks_every_tree() {
    let (repo, head) = build_nested_fixture();
    let _guard = DirGuard(repo.clone());

    let store = Arc::new(ObjectStore::open(repo.join(".git")).expect("open store"));
    let root_tree = store.commit_tree(head).expect("commit tree");
    let hydrating = HydratingObjectStore::new(store, NoopFetcher::new());

    let stats = hydrating.warm_tree_closure(root_tree);

    // root + src + src/util = 3 trees, walked across 3 BFS levels.
    assert_eq!(stats.trees_warmed, 3, "root + src + src/util");
    assert_eq!(stats.levels, 3, "root -> src -> src/util");
    assert_eq!(stats.errors, 0, "all trees present locally");
}
