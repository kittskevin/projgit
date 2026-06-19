//! `.git/` synthesis (variant A1 from `docs/design/dotgit-synthesis.md`).
//!
//! Produces a [`RootOverlay`] containing a minimal but functional `.git/`
//! directory pointing at the projection's commit and at the shared on-disk
//! object store via `objects/info/alternates`. Tools that look upward for
//! `.git/` see the projection as a real Git repository in detached-HEAD
//! mode.
//!
//! # What works under A1
//!
//! - `git rev-parse HEAD`, `git log`, `git cat-file <oid>`,
//!   `git show <oid>`, `git diff <ref>..<ref>`.
//! - `cargo build` records the projection's commit OID in build metadata.
//! - `ripgrep`, `fd`, IDE repo-root detection treat the mount as a repo
//!   and respect the projection's `.gitignore`.
//!
//! # What still does not work
//!
//! Write verbs (`git add`, `git commit`, `git stash`) fail because the
//! mount is read-only. This is consistent with the rest of projgit's
//! contract — every blob in the projected tree is read-only too.
//!
//! # Known risk
//!
//! The absolute path of the shared object store leaks through
//! `.git/objects/info/alternates`. Acceptable for the local per-eval-
//! container use case but worth noting before sharing logs from a mount.
//!
//! Per the design ladder in `docs/design/dotgit-synthesis.md`, this module
//! ships **A1** ([`a1_overlay`]), **A1+** ([`a1_plus_overlay`], which
//! adds a clean read-only `.git/index` matching HEAD; see
//! `docs/design/dotgit-index.md`), and **A2** ref visibility
//! ([`apply_a2_ref_visibility`], applied on top of either A1 or A1+
//! when the projection is a branch — turns the detached `HEAD` into a
//! symbolic `ref: refs/heads/<branch>` and creates the corresponding
//! loose ref file). A3 (writable illusion) is deferred.

use crate::object_store::ObjectStore;
use crate::overlay::{RootOverlay, SyntheticEntry};
use bstr::BString;
use gix::ObjectId;
use std::path::Path;

/// Minimal `[core]` config that satisfies git's repository detection.
const A1_CONFIG: &str = "\
[core]
\trepositoryformatversion = 0
\tfilemode = true
\tbare = false
\tlogallrefupdates = false
";

/// Build an A1 `.git/` overlay for a projection.
///
/// `commit_oid` is what `HEAD` will contain (in detached form, i.e. just
/// the OID hex on its own line). `objects_dir` must be the absolute path
/// to the *shared* `objects/` directory of the partial clone (typically
/// `<git-dir>/objects`); it goes into `.git/objects/info/alternates` so
/// that git's object lookup can reach the partial-clone packs from
/// inside the mount.
///
/// The returned overlay has exactly one top-level entry, `.git/`, with
/// this structure:
///
/// ```text
/// .git/
/// ├── HEAD                       (detached: "<commit-oid>\n")
/// ├── config                     (minimal [core] config)
/// ├── packed-refs                (empty)
/// ├── refs/
/// │   ├── heads/                 (empty)
/// │   └── tags/                  (empty)
/// └── objects/
///     └── info/
///         └── alternates         ("<objects_dir>\n")
/// ```
pub fn a1_overlay(commit_oid: ObjectId, objects_dir: &Path) -> RootOverlay {
    // Leaves.
    let head = SyntheticEntry::file(format!("{commit_oid}\n").into_bytes());
    let config = SyntheticEntry::file(A1_CONFIG.as_bytes().to_vec());
    let packed_refs = SyntheticEntry::file(Vec::<u8>::new());
    let alternates_line = format!("{}\n", objects_dir.display());
    let alternates = SyntheticEntry::file(alternates_line.into_bytes());

    // refs/heads, refs/tags — empty directories.
    let mut refs = SyntheticEntry::directory();
    refs.insert_child("heads", SyntheticEntry::directory());
    refs.insert_child("tags", SyntheticEntry::directory());

    // objects/info/alternates
    let mut info = SyntheticEntry::directory();
    info.insert_child("alternates", alternates);
    let mut objects = SyntheticEntry::directory();
    objects.insert_child("info", info);

    // Top-level .git/
    let mut dotgit = SyntheticEntry::directory();
    dotgit.insert_child("HEAD", head);
    dotgit.insert_child("config", config);
    dotgit.insert_child("packed-refs", packed_refs);
    dotgit.insert_child("refs", refs);
    dotgit.insert_child("objects", objects);

    let mut overlay = RootOverlay::new();
    overlay.insert(".git", dotgit);
    overlay
}

/// Build an **A1+** `.git/` overlay for a projection.
///
/// A1+ is A1 plus a synthetic, read-only `.git/index` matching HEAD's
/// tree, with every entry's `ASSUME_VALID` flag set. Tools that look
/// at the index (`git status`, `git diff`, `git diff --cached`,
/// `git ls-files`) treat the mount as a clean checkout instead of
/// showing the entire tree as "staged for deletion" (which is what
/// happens at A1, where the index is missing and git defaults to an
/// empty index).
///
/// The serialized bytes are deterministic per `commit_oid`: the on-disk
/// index format only records (path, mode, OID, zero-stat) per entry,
/// none of which depend on wall-clock time. Two mounts of the same
/// projection produce byte-identical index bytes — useful for tests
/// and for a future cross-mount index byte cache.
///
/// See `docs/design/dotgit-index.md` for the full design, including
/// the axis-split rationale that motivates A1+ as a separate rung
/// between A1 and A2 rather than a slice of A3.
pub fn a1_plus_overlay(
    store: &ObjectStore,
    commit_oid: ObjectId,
    objects_dir: &Path,
) -> Result<RootOverlay, IndexBuildError> {
    let mut overlay = a1_overlay(commit_oid, objects_dir);
    let index_bytes = build_index_bytes(store, commit_oid)?;
    splice_dotgit_child(&mut overlay, "index", index_bytes);
    Ok(overlay)
}

/// Apply **A2 ref visibility** to an A1 / A1+ overlay in place.
///
/// Replaces the detached `.git/HEAD` (a bare OID) with a symbolic
/// `ref: <branch_full_name>\n`, and creates the corresponding loose
/// ref file at `.git/<branch_full_name>` containing `<commit_oid>\n`.
/// Per `docs/design/dotgit-synthesis.md` §4.1 table row A2: orthogonal
/// to A1+ (the index axis), composes on top of either A1 or A1+.
///
/// What this unlocks inside the mount:
///
/// - `git branch --show-current` returns `<branch>` instead of empty.
/// - `git symbolic-ref HEAD` returns `<branch_full_name>`.
/// - `git rev-parse <branch_full_name>` works.
/// - IDE branch indicators show the branch name instead of "detached
///   HEAD".
/// - `git log --all` sees this one ref (vs A1, which sees no refs).
///
/// `branch_full_name` **must** start with `refs/heads/`. The caller is
/// responsible for normalising user input (short names like `main`
/// → `refs/heads/main`) and for restricting application to branch
/// projections (not tags, not `--commit`, not `HEAD` in detached
/// mode). Tag projections deliberately stay on A1's detached HEAD
/// because git refuses to set HEAD to a tag ref and IDEs would
/// misrender it as a branch indicator anyway.
///
/// Idempotent on the HEAD replacement; calling twice with different
/// `branch_full_name`s overwrites the first call's ref file and
/// leaves the previous one orphaned in the overlay tree. Don't do
/// that.
///
/// # Panics
///
/// - if `branch_full_name` does not start with `refs/heads/`
/// - if the overlay does not contain a top-level `.git/` directory
///   (i.e. it was not produced by [`a1_overlay`] / [`a1_plus_overlay`])
pub fn apply_a2_ref_visibility(
    overlay: &mut RootOverlay,
    branch_full_name: &str,
    commit_oid: ObjectId,
) {
    assert!(
        branch_full_name.starts_with("refs/heads/"),
        "branch_full_name must start with `refs/heads/`, got `{branch_full_name}`",
    );

    // 1. Symbolic HEAD: `ref: refs/heads/<branch>\n`.
    let head_bytes = format!("ref: {branch_full_name}\n").into_bytes();
    splice_dotgit_child(overlay, "HEAD", head_bytes);

    // 2. Loose ref file at `.git/<branch_full_name>` containing
    //    `<oid>\n`. The path always has at least three segments
    //    (`refs`, `heads`, plus one branch component), and may have
    //    more for nested branches like `feature/foo`.
    let ref_path: Vec<&str> = branch_full_name.split('/').collect();
    debug_assert!(ref_path.len() >= 3 && ref_path[0] == "refs" && ref_path[1] == "heads");
    let ref_content = format!("{commit_oid}\n").into_bytes();
    splice_nested_dotgit_file(overlay, &ref_path, ref_content);
}

/// Splice a single file into the `.git/` directory of an overlay that
/// was returned by [`a1_overlay`]. Panics if the overlay doesn't have
/// a `.git/` directory at the top level (only the in-tree
/// `a1_overlay` / `a1_plus_overlay` produce such overlays today, so a
/// missing `.git/` would mean we plumbed the wrong overlay in).
fn splice_dotgit_child(overlay: &mut RootOverlay, name: &str, content: Vec<u8>) {
    let dotgit = overlay.get_mut(b".git").expect("overlay missing .git/");
    let SyntheticEntry::Directory { children } = dotgit else {
        panic!(".git entry is not a directory");
    };
    children.insert(BString::from(name), SyntheticEntry::file(content));
}

/// Splice a file at a nested `/`-separated path inside `.git/`,
/// creating any intermediate directories that don't exist yet (or
/// reusing existing ones — e.g. `refs/heads/` is already populated
/// by [`a1_overlay`] and gets reused). `path` must have at least one
/// component (the file name) and is interpreted as components *under*
/// `.git/`: passing `["refs", "heads", "main"]` writes
/// `.git/refs/heads/main`.
///
/// Used by [`apply_a2_ref_visibility`] to drop the loose ref file at
/// `.git/refs/heads/<branch>` while reusing the already-empty
/// `refs/heads/` directory the A1 overlay creates.
fn splice_nested_dotgit_file(overlay: &mut RootOverlay, path: &[&str], content: Vec<u8>) {
    assert!(!path.is_empty(), "splice_nested_dotgit_file needs a non-empty path");
    let dotgit = overlay.get_mut(b".git").expect("overlay missing .git/");
    let SyntheticEntry::Directory { children: dotgit_children } = dotgit else {
        panic!(".git entry is not a directory");
    };

    // Walk / create intermediates. `current` always points at the
    // children-map we're about to insert into.
    let mut current: &mut std::collections::BTreeMap<BString, SyntheticEntry> = dotgit_children;
    for &component in &path[..path.len() - 1] {
        let key = BString::from(component);
        let entry = current
            .entry(key)
            .or_insert_with(SyntheticEntry::directory);
        match entry {
            SyntheticEntry::Directory { children } => {
                current = children;
            }
            _ => panic!(
                "splice_nested_dotgit_file: `.git/{}` is not a directory",
                path[..path.len() - 1].join("/"),
            ),
        }
    }
    let leaf_name = path[path.len() - 1];
    current.insert(BString::from(leaf_name), SyntheticEntry::file(content));
}

/// Build the `.git/index` byte payload for the A1+ overlay.
///
/// Walks HEAD's tree once via [`gix::Repository::index_from_tree`]
/// (which internally calls `gix_index::State::from_tree`), sets
/// `ASSUME_VALID` on every entry so git doesn't try to stat the
/// worktree, and serializes via `gix::index::File::write_to` (which
/// adds the trailing SHA1 trailer git expects).
///
/// The `ASSUME_VALID` flag is what makes this safe to ship without
/// real stat info per entry: git would otherwise see uid/gid skew
/// (the FUSE adapter echoes the requesting process's identity, which
/// will differ from any baked-in identity) and force a per-file
/// re-hash on every `git status`. With the flag set, git short-
/// circuits to "clean" without touching the worktree.
fn build_index_bytes(
    store: &ObjectStore,
    commit_oid: ObjectId,
) -> Result<Vec<u8>, IndexBuildError> {
    let tree_oid = store
        .commit_tree(commit_oid)
        .map_err(IndexBuildError::Store)?;

    let repo = store.handle();
    let file = repo
        .index_from_tree(&tree_oid)
        .map_err(|e| IndexBuildError::FromTree(Box::new(e)))?;
    let (mut state, _path) = file.into_parts();

    for entry in state.entries_mut() {
        entry.flags |= gix::index::entry::Flags::ASSUME_VALID;
    }

    // Wrap the mutated state back in a File so `File::write_to` adds
    // the trailing SHA1 trailer git expects; the path is bogus
    // (the bytes never touch disk).
    let file = gix::index::File::from_state(state, std::path::PathBuf::new());
    let mut bytes = Vec::new();
    file.write_to(&mut bytes, gix::index::write::Options::default())
        .map_err(IndexBuildError::Write)?;
    Ok(bytes)
}

/// Build the **writable-mode** `.git/index` byte payload for a worktree
/// mount.
///
/// Unlike [`build_index_bytes`] (the read-only A1+ index, which sets
/// `ASSUME_VALID` on every entry so git never stats the worktree), this
/// produces an index suited to a *writable* mount: entries carry real
/// `mode` + `oid` **plus** real `size` (from [`ObjectStore::header`] —
/// no content read) and a stable `mtime` (the projection's commit time),
/// and **do not** set `ASSUME_VALID`.
///
/// The reasoning (validated by the `spikes/writable-nofork` spike,
/// `RESULTS.md` finding #1): with `ASSUME_VALID` git would never notice
/// an edit, which is fatal for a writable worktree. Without it, git's
/// normal stat comparison must be satisfied for *unmodified* files or
/// the first `git status` re-hashes (hydrates) every file. Supplying the
/// true blob size + a stable mtime that matches what the FUSE backend
/// reports for un-materialised files makes that first status clean
/// **without** reading any content, while a real edit (changed size /
/// mtime) is still detected.
///
/// Pair this with `core.checkStat = minimal` (see [`WRITABLE_CORE_CONFIG`])
/// so git compares only mtime + size, ignoring the `dev` / `ino` /
/// `uid` / `gid` that a synthesized index cannot predict ahead of the
/// mount existing.
///
/// `size` is read per entry via the header cache; on a partial clone
/// this faults the object header in (sizes, not full bytes on a GVFS
/// backend) — the intended eager warm for a worktree.
pub fn build_writable_index_bytes(
    store: &ObjectStore,
    commit_oid: ObjectId,
) -> Result<Vec<u8>, IndexBuildError> {
    build_writable_index_bytes_inner(store, commit_oid, &[])
}

/// Like [`build_writable_index_bytes`] but for a **sparse** (cone-mode)
/// mount: every entry whose path is outside `cone` gets the
/// `SKIP_WORKTREE` flag set, so git knows those files are intentionally
/// absent from the worktree. Pair this with the FUSE projection hiding
/// the same out-of-cone paths (writable-worktrees-plan.md Stage 5 / R2):
/// without `SKIP_WORKTREE`, git would report the hidden files as
/// *deleted*; with it, `status` stays clean and the sparse-index stays
/// collapsed.
///
/// `cone` is a list of cone-mode directories (worktree-relative, no
/// trailing slash). An empty `cone` is equivalent to
/// [`build_writable_index_bytes`].
pub fn build_writable_index_bytes_sparse(
    store: &ObjectStore,
    commit_oid: ObjectId,
    cone: &[String],
) -> Result<Vec<u8>, IndexBuildError> {
    build_writable_index_bytes_inner(store, commit_oid, cone)
}

/// Whether an index file `path` is inside the cone (i.e. its parent
/// directory is shown). Root files are always in; otherwise the parent
/// dir must be within a cone dir or an ancestor leading to one. Empty
/// cone => everything in.
fn index_path_in_cone(path: &str, cone: &[String]) -> bool {
    if cone.is_empty() {
        return true;
    }
    let parent = match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    };
    if parent.is_empty() {
        return true;
    }
    cone.iter().any(|c| {
        parent == c
            || parent.starts_with(&format!("{c}/"))
            || c.starts_with(&format!("{parent}/"))
    })
}

fn build_writable_index_bytes_inner(
    store: &ObjectStore,
    commit_oid: ObjectId,
    cone: &[String],
) -> Result<Vec<u8>, IndexBuildError> {
    use std::time::UNIX_EPOCH;

    let tree_oid = store
        .commit_tree(commit_oid)
        .map_err(IndexBuildError::Store)?;
    let commit_time = store
        .commit_time(commit_oid)
        .map_err(IndexBuildError::Store)?;
    let since = commit_time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let stamp = gix::index::entry::stat::Time {
        secs: since.as_secs().min(u32::MAX as u64) as u32,
        nsecs: since.subsec_nanos(),
    };

    let repo = store.handle();
    let file = repo
        .index_from_tree(&tree_oid)
        .map_err(|e| IndexBuildError::FromTree(Box::new(e)))?;
    let (mut state, _path) = file.into_parts();

    // Collect paths first (borrows `state` immutably); then mutate entries
    // by index (mutable borrow). The two never overlap.
    let paths: Vec<bstr::BString> = if cone.is_empty() {
        Vec::new()
    } else {
        state.entries().iter().map(|e| e.path(&state).to_owned()).collect()
    };

    for (i, entry) in state.entries_mut().iter_mut().enumerate() {
        let (_kind, size) = store.header(entry.id).map_err(IndexBuildError::Store)?;
        entry.stat.size = size.min(u32::MAX as u64) as u32;
        entry.stat.mtime = stamp;
        entry.stat.ctime = stamp;
        // NB: deliberately NOT setting ASSUME_VALID — a writable mount
        // needs git to notice edits.
        if !cone.is_empty() {
            let path = String::from_utf8_lossy(&paths[i]);
            if !index_path_in_cone(&path, cone) {
                entry.flags |= gix::index::entry::Flags::EXTENDED
                    | gix::index::entry::Flags::SKIP_WORKTREE;
            }
        }
    }

    let file = gix::index::File::from_state(state, std::path::PathBuf::new());
    let mut bytes = Vec::new();
    file.write_to(&mut bytes, gix::index::write::Options::default())
        .map_err(IndexBuildError::Write)?;
    Ok(bytes)
}

/// Minimal `[core]` config for a **writable** worktree mount. Adds
/// `checkStat = minimal` on top of the read-only [`A1_CONFIG`] so git
/// compares only mtime + size against the index (the fields a
/// synthesized writable index can actually match — see
/// [`build_writable_index_bytes`]), ignoring dev/ino/uid/gid.
pub const WRITABLE_CORE_CONFIG: &str = "\
[core]
\trepositoryformatversion = 0
\tfilemode = true
\tbare = false
\tlogallrefupdates = false
\tcheckStat = minimal
";

/// Errors from [`a1_plus_overlay`] / `build_index_bytes`.
#[derive(Debug, thiserror::Error)]
pub enum IndexBuildError {
    /// The shared object store rejected a read needed during the build
    /// (peeling the commit to a tree, reading a blob header for its
    /// size, or a tree walked by `gix_index::State::from_tree`).
    #[error("object store error while building index: {0}")]
    Store(#[from] crate::error::ObjectStoreError),
    /// [`gix::Repository::index_from_tree`] rejected the input
    /// (invalid path component, unreadable tree object, config
    /// boolean error, etc.). Boxed to keep this enum small — the
    /// underlying type is a multi-variant gix error with its own
    /// nested source chain.
    #[error("gix index_from_tree failed: {0}")]
    FromTree(Box<gix::repository::index_from_tree::Error>),
    /// Serializing the index to bytes failed.
    #[error("gix_index::File::write_to failed: {0}")]
    Write(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstr::BStr;
    use std::path::PathBuf;

    fn fixture_oid() -> ObjectId {
        ObjectId::from_hex(b"1234567890abcdef1234567890abcdef12345678").unwrap()
    }

    /// Walk a `/`-separated path inside a freshly-built overlay and return
    /// the leaf entry. Panics on any mis-step so tests stay terse.
    fn walk<'a>(overlay: &'a RootOverlay, path: &str) -> &'a SyntheticEntry {
        let mut parts = path.split('/');
        let first = parts.next().expect("non-empty path");
        let mut current = overlay
            .get(first.as_bytes())
            .unwrap_or_else(|| panic!("missing top-level `{first}`"));
        for component in parts {
            current = match current {
                SyntheticEntry::Directory { children } => children
                    .get(BStr::new(component.as_bytes()))
                    .unwrap_or_else(|| panic!("missing component `{component}` in `{path}`")),
                _ => panic!("non-directory ancestor while walking `{path}`"),
            };
        }
        current
    }

    fn file_content(entry: &SyntheticEntry) -> &[u8] {
        match entry {
            SyntheticEntry::File { content, .. } => content,
            other => panic!("expected file, got {other:?}"),
        }
    }

    #[test]
    fn overlay_top_level_is_only_dotgit() {
        let o = a1_overlay(fixture_oid(), Path::new("/cache/repo/objects"));
        let names: Vec<&[u8]> = o.names().map(|n| n.as_slice()).collect();
        assert_eq!(names, vec![b".git" as &[u8]]);
    }

    #[test]
    fn head_is_detached_oid_with_trailing_newline() {
        let o = a1_overlay(fixture_oid(), Path::new("/cache/repo/objects"));
        let head = walk(&o, ".git/HEAD");
        assert_eq!(
            file_content(head),
            b"1234567890abcdef1234567890abcdef12345678\n"
        );
    }

    #[test]
    fn alternates_carries_absolute_objects_dir() {
        let o = a1_overlay(
            fixture_oid(),
            Path::new("/var/cache/projgit/log-58b87cfa/.git/objects"),
        );
        let alt = walk(&o, ".git/objects/info/alternates");
        assert_eq!(
            file_content(alt),
            b"/var/cache/projgit/log-58b87cfa/.git/objects\n"
        );
    }

    #[test]
    fn config_declares_repository_format_version() {
        let o = a1_overlay(fixture_oid(), Path::new("/cache/repo/objects"));
        let cfg = file_content(walk(&o, ".git/config"));
        let s = std::str::from_utf8(cfg).unwrap();
        assert!(s.contains("[core]"));
        assert!(s.contains("repositoryformatversion = 0"));
    }

    #[test]
    fn packed_refs_exists_and_is_empty() {
        let o = a1_overlay(fixture_oid(), Path::new("/cache/repo/objects"));
        let pr = walk(&o, ".git/packed-refs");
        assert!(file_content(pr).is_empty());
    }

    #[test]
    fn refs_has_empty_heads_and_tags_subdirs() {
        let o = a1_overlay(fixture_oid(), Path::new("/cache/repo/objects"));
        let heads = walk(&o, ".git/refs/heads");
        let tags = walk(&o, ".git/refs/tags");
        for d in [heads, tags] {
            match d {
                SyntheticEntry::Directory { children } => assert!(children.is_empty()),
                _ => panic!("refs subdir is not a directory"),
            }
        }
    }

    #[test]
    fn overlay_does_not_shadow_arbitrary_real_paths() {
        // Only `.git/` is reserved. Real tree entries with other names
        // (e.g. `src/`, `README.md`) must remain visible.
        let o = a1_overlay(fixture_oid(), Path::new("/x"));
        assert!(o.would_collide(b".git"));
        assert!(!o.would_collide(b"src"));
        assert!(!o.would_collide(b"README.md"));
        assert!(!o.would_collide(b".gitignore"));
    }

    #[test]
    fn alternates_path_does_not_normalize_separators() {
        // Behaviour: we pass through `objects_dir.display()` verbatim.
        // On Linux that's forward slashes; on Windows it would be
        // backslashes but git accepts both. Documented here so a
        // future Windows backend doesn't get a surprising regression.
        let o = a1_overlay(fixture_oid(), &PathBuf::from("/a/b/c"));
        let alt = walk(&o, ".git/objects/info/alternates");
        assert_eq!(file_content(alt), b"/a/b/c\n");
    }

    // ---- A2 ref visibility ----------------------------------------------

    #[test]
    fn a2_replaces_head_with_symbolic_ref() {
        let mut o = a1_overlay(fixture_oid(), Path::new("/x/objects"));
        apply_a2_ref_visibility(&mut o, "refs/heads/main", fixture_oid());
        let head = file_content(walk(&o, ".git/HEAD"));
        assert_eq!(head, b"ref: refs/heads/main\n");
    }

    #[test]
    fn a2_creates_loose_ref_file_with_oid() {
        let mut o = a1_overlay(fixture_oid(), Path::new("/x/objects"));
        apply_a2_ref_visibility(&mut o, "refs/heads/main", fixture_oid());
        let ref_file = file_content(walk(&o, ".git/refs/heads/main"));
        assert_eq!(
            ref_file,
            b"1234567890abcdef1234567890abcdef12345678\n",
            "loose ref file must contain `<oid>\\n`",
        );
    }

    #[test]
    fn a2_supports_nested_branch_names() {
        // `feature/foo` needs a `.git/refs/heads/feature/` directory
        // to be created on the fly because A1 only creates the empty
        // `refs/heads/` parent.
        let mut o = a1_overlay(fixture_oid(), Path::new("/x/objects"));
        apply_a2_ref_visibility(&mut o, "refs/heads/feature/foo", fixture_oid());

        let head = file_content(walk(&o, ".git/HEAD"));
        assert_eq!(head, b"ref: refs/heads/feature/foo\n");

        let ref_file = file_content(walk(&o, ".git/refs/heads/feature/foo"));
        assert_eq!(
            ref_file,
            b"1234567890abcdef1234567890abcdef12345678\n",
        );

        // The intermediate `feature/` is a directory, not a file, and
        // the empty `tags/` is still around from A1.
        match walk(&o, ".git/refs/heads/feature") {
            SyntheticEntry::Directory { children } => {
                assert_eq!(children.len(), 1, "feature/ should contain only foo");
            }
            other => panic!("refs/heads/feature is not a directory: {other:?}"),
        }
        match walk(&o, ".git/refs/tags") {
            SyntheticEntry::Directory { children } => {
                assert!(children.is_empty(), "refs/tags/ must remain empty");
            }
            other => panic!("refs/tags is not a directory: {other:?}"),
        }
    }

    #[test]
    fn a2_preserves_other_a1_files() {
        // Applying A2 must not disturb A1's config / packed-refs /
        // objects/info/alternates entries.
        let mut o = a1_overlay(fixture_oid(), Path::new("/cache/repo/.git/objects"));
        apply_a2_ref_visibility(&mut o, "refs/heads/main", fixture_oid());

        assert!(file_content(walk(&o, ".git/config")).starts_with(b"[core]"));
        assert!(file_content(walk(&o, ".git/packed-refs")).is_empty());
        assert_eq!(
            file_content(walk(&o, ".git/objects/info/alternates")),
            b"/cache/repo/.git/objects\n",
        );
    }

    #[test]
    #[should_panic(expected = "must start with `refs/heads/`")]
    fn a2_panics_on_non_branch_ref() {
        let mut o = a1_overlay(fixture_oid(), Path::new("/x/objects"));
        apply_a2_ref_visibility(&mut o, "refs/tags/v1", fixture_oid());
    }

    #[test]
    #[should_panic(expected = "must start with `refs/heads/`")]
    fn a2_panics_on_short_branch_name() {
        // The function takes the *full* ref name; the caller is
        // responsible for normalising short names like `main` →
        // `refs/heads/main`.
        let mut o = a1_overlay(fixture_oid(), Path::new("/x/objects"));
        apply_a2_ref_visibility(&mut o, "main", fixture_oid());
    }
}
