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
//! Per the design ladder in `docs/design/dotgit-synthesis.md`, this is
//! variant A1. The richer A2 (symbolic HEAD pointing at a real ref + the
//! ref file populated) and A3 (writable illusion) are deferred follow-ups.

use crate::overlay::{RootOverlay, SyntheticEntry};
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
}
