# Design: dotgit `.git/index` synthesis (A1+)

> Status: **planned 2026-05-17**. Companion to
> [`dotgit-synthesis.md`](dotgit-synthesis.md) (the parent design and the
> A0/A1/A2/A3 ladder). This doc covers the A1+ rung specifically: a
> synthetic, read-only `.git/index` matching HEAD that makes `git status`,
> `git diff`, and `git ls-files` behave as for a clean checkout — without
> taking on any of the writable-illusion machinery that A3 implies.

## 0. Why this doc exists

The original [`dotgit-synthesis.md`](dotgit-synthesis.md) ladder modeled
the `.git/` surface as a single sequence:

```
A0 → A1 → A2 → A3
marker  minimal  ref-aware  writable illusion
```

Building A1 and using it for the first time surfaced a flaw in that
framing: it conflates **two orthogonal axes** of UX completeness, and the
real-world UX wins fall along the *missing* axis.

| Axis | What it controls | Where the original ladder put it |
|---|---|---|
| **Ref visibility** | Symbolic `HEAD` → branch name, populated `refs/heads/<name>`. Enables `git branch --show-current`, IDE branch indicator, `git log --all` seeing the one ref. | A2 (own rung) |
| **Working-tree comparison cleanliness** | A populated `.git/index` so `git status`, `git diff*`, and `git ls-files` behave as for a clean checkout. | A3 (bundled with "writable illusion") |

The axis-2 work was put in A3 because the doc's author at the time treated
"index" as "*writable* index supporting `git add`", which does need the
~2000 LOC of lock-file emulation, ORIG_HEAD updates, reflog writes, etc.
But a **read-only index that matches HEAD with the `ASSUME_VALID` flag
set** is a different beast: it fixes the read verbs without touching
write-verb machinery.

A1+ is that missing rung. It sits between A1 and A2 in the original ladder
but is orthogonal to A2 along the ref-visibility axis: you can ship A1, or
A1+, or A1 + A2, or A1+ + A2, in any order. They compose.

## 1. The problem A1+ solves (measured)

Against the current shipped A1 default, with `rust-lang/log` mounted:

```text
$ git status --short | wc -l                         → 36
$ git status                                         → "Changes to be committed:
                                                         deleted: <every file>"
$ git diff                                           → empty (0 lines)
$ git diff --cached | wc -l                          → 2,897
$ git ls-files | wc -l                               → 0
```

Root cause: there is no `.git/index` file. Git's default behavior with a
missing index is "empty index." HEAD's tree is populated, so the diff
against empty looks like every file was deleted from the index → every
file appears "staged for commit (deletion)" in `git status`. The working
tree files do exist (they're the projection), but with an empty index
there's no "before" picture for plain `git diff` either, so that
correctly comes out empty.

The actively misleading parts are `git status`, `git diff --cached`, and
`git ls-files`. A reader who casually opens a projgit mount and runs
`git status` sees a wall of red and might infer the mount is broken or
that history has been rewritten. That is the failure mode A1+ closes.

## 2. The A1+ contract

A1+ synthesizes one additional file in the `RootOverlay` produced by
`crate::dotgit::a1_plus_overlay(...)`:

```text
.git/index    ← gix-index V2 file containing one entry per blob /
                symlink / gitlink in HEAD's tree, each with the
                ASSUME_VALID flag set.
```

Everything else from A1 (HEAD, config, packed-refs, refs/, objects/info/
alternates) is unchanged.

**Expected post-mount behavior** (no user setup beyond `projgit mount`):

| Command | A1 (today) | A1+ (this doc) |
|---|---|---|
| `git rev-parse HEAD` | ✅ | ✅ |
| `git log -- src/foo.rs` | ✅ | ✅ |
| `git status --porcelain` | 36 lines of fake deletions | 0 lines |
| `git status` (full) | "Changes to be committed: deleted: ..." | "nothing to commit, working tree clean" |
| `git diff` | empty | empty |
| `git diff --cached` | 2,897-line diff | empty |
| `git ls-files` | empty | full file list |
| `git ls-files -v` | empty | every entry prefixed with `h` (assume-unchanged) |
| `git add <file>` | "permission denied" | "permission denied" (unchanged) |

A1+ does not enable any write verbs. Lock-file emulation, ORIG_HEAD,
reflog, and friends remain out of scope, consistent with projgit's
read-only invariant.

## 3. Why `ASSUME_VALID` is the central mechanism

The naive approach — populate the index with real stat info (size, mode,
mtime, ctime, dev, ino, uid, gid) — has a fatal problem: the FUSE adapter
echoes the *requesting process's* uid/gid back as file ownership (see the
`fix(fuse): echo request uid/gid as file ownership` commit, which is what
lets `safe.directory` pass). So a different user querying the same mount
sees different uid/gid in `stat`. Any uid/gid we baked into the index at
mount time would mismatch some readers, forcing git to re-hash the entire
working tree on every `git status` to figure out whether the file is
"really" modified. For a 100K-file repo that's catastrophic.

`ASSUME_VALID` (bit `1 << 15` in the per-entry flags, set by
`git update-index --assume-unchanged` in normal git workflows) tells git
"trust this entry; do not stat the worktree to verify." With it set on
every entry:

- Git skips the stat check entirely; uid/gid skew doesn't matter.
- `git status` short-circuits to "clean" without touching any blob bytes.
- `git diff` short-circuits to empty for the same reason.

The semantic match is near-exact: from a user's perspective, every file in
the projgit mount really *is* always equal to its HEAD blob. The mount is
read-only by construction; the entry can't drift from HEAD because the
file *is* the projected blob.

The flag is per-entry, in-memory state during build (gix-index's
`entry::Flags` 32-bit struct). Only the persisted 16-bit subset is
serialized to disk, and `ASSUME_VALID = 0x8000` is part of that subset,
so the on-disk bytes propagate the flag to git.

## 4. Mechanism

`gix-index 0.35.0` (already transitively in the workspace via `gix`)
provides everything needed:

```rust
// crates/projgit-core/src/dotgit.rs

pub fn a1_plus_overlay(
    store: &ObjectStore,
    commit_oid: ObjectId,
    objects_dir: &Path,
    commit_time: SystemTime,
) -> Result<RootOverlay, IndexBuildError> {
    let mut overlay = a1_overlay(commit_oid, objects_dir);
    let index_bytes = build_index_bytes(store, commit_oid, commit_time)?;
    splice_dotgit_child(&mut overlay, "index", index_bytes);
    Ok(overlay)
}

fn build_index_bytes(
    store: &ObjectStore,
    commit_oid: ObjectId,
    commit_time: SystemTime,
) -> Result<Vec<u8>, IndexBuildError> {
    let tree_oid = store.commit_tree(commit_oid)?;
    let mut state = store.with_repo(|repo| {
        gix_index::State::from_tree(
            &tree_oid,
            repo,                                     // impls gix_object::Find
            gix_validate::path::component::Options::default(),
        )
    })?;
    // Canonicalize timestamp for byte-deterministic output across
    // mounts of the same projection.
    state.set_timestamp(filetime::FileTime::from_system_time(commit_time));
    for entry in state.entries_mut() {
        entry.flags |= gix_index::entry::Flags::ASSUME_VALID;
    }
    let mut bytes = Vec::with_capacity(estimated_size(&state));
    state.write_to(&mut bytes, gix_index::write::Options::default())?;
    Ok(bytes)
}
```

Cost per build:

- One tree-walk through HEAD via `breadthfirst` (gix-index's
  `from_tree` does this internally). Reads tree objects from the
  shared object store; no blob hydration.
- One pass over the entry vector to set the flag.
- One serialization pass.

For `rust-lang/log` (~36 entries) the whole thing is < 5 ms. For
`tokio-rs/tokio` (~700 entries) sub-50 ms. For a 100K-file monorepo,
seconds; once per mount; pays for itself the first time anything runs
`git status`.

Memory: the index buffer is ~62 bytes header overhead per entry plus
the path string. Rough budget:

| Tree size | Approx index size |
|---|---|
| 36 files (rust-lang/log) | ~3 KiB |
| 700 files (tokio) | ~60 KiB |
| 10,000 files | ~1 MiB |
| 100,000 files | ~10 MiB |
| 1,000,000 files | ~100 MiB |

Held as `Vec<u8>` inside the `RootOverlay::SyntheticEntry::File` for the
lifetime of the mount. Acceptable for the workload (mounts last
minutes-to-hours; even the 1M-file extreme is one tenth of typical
container memory limits). No cap; users hitting it can `--no-dotgit`.

## 5. Determinism: timestamp canonicalization

`gix_index::State::from_tree` defaults to `filetime::FileTime::now()` for
the State's `timestamp` field, which feeds into the index file header.
Two mounts of the same projection at different wall-clock times would
otherwise produce slightly different index bytes. Setting it to the
projection's `commit_time` instead gives byte-identical output for the
same `(tree_oid, commit_time)` pair, which:

1. Lets tests assert on the bytes directly.
2. Opens the door to cross-mount caching (future optimization: key by
   `tree_oid`, store the bytes once, share across mounts of the same
   projection on the same host).
3. Matches the rest of projgit's behavior — `mtime` on every entry
   already comes from `commit_time`.

## 6. What A1+ deliberately doesn't do

- **No A2 ref visibility.** `HEAD` is still detached (just the OID, no
  symbolic ref). `git branch --show-current` still prints "HEAD" rather
  than the projection's ref name. That's A2's job; ship it separately.
- **No write support.** `git add`, `git commit`, `git stash` still fail.
  Lock-file emulation, reflog, ORIG_HEAD updates, hooks dir — all A3,
  all rejected per `dotgit-synthesis.md` §9.4 until a real write path
  lands.
- **No subtree-projection support.** Subtree mounts skip dotgit
  synthesis entirely, same as A1; the index would have to describe the
  subtree's own tree (a different OID from the projection's full HEAD
  tree). Tractable; deferred.
- **No sparse-index / V4 format.** gix-index defaults to V2, which every
  git version understands. V4 is a perf optimization for huge repos;
  not worth the compatibility cost today.
- **No submodule handling.** Gitlinks in the tree become `Mode::COMMIT`
  entries in the index. Without per-submodule HEAD synthesis, `git
  status` shows them as "modified." Acceptable for the current fixture
  repo (no submodules); revisit if a future test target has them.
- **No cross-mount index caching.** Each mount rebuilds. Tree walk is
  in the noise next to partial-clone time; cache infrastructure
  ($XDG_CACHE) would be its own design question.

## 7. Risks

### 7.1 Stat-info drift across users

**Mitigated** by `ASSUME_VALID`. Without the flag, the index's baked-in
stat would mismatch FUSE-reported stat for any user other than the one
who mounted, forcing per-file rehash. With the flag, git skips the stat
check; nothing to drift.

### 7.2 `git update-index --refresh` would try to write `.git/index.lock`

**Same shape as the existing A1 risk for `git add`** (lock file fails
because the mount is read-only). User sees a clear "permission denied"
or "unable to create lock file" message. The read-only contract holds.

### 7.3 Index file size for adversarial repos

Discussed in §4. ~100 MiB at 1M files. No cap; `--no-dotgit` is the
escape hatch. Document in the CLI help text.

### 7.4 Gitlink entries

The projection renders submodules as empty directories.
`gix_index::State::from_tree` creates an `EntryKind::Commit` entry for
each gitlink. `git status` will list each gitlink as "modified" because
the working-tree side (empty directory) doesn't match the index's commit
OID. Documented as a known A1+ limitation; submodule handling is
out of scope until A2 grows submodule awareness.

### 7.5 Empty trees

Trees with zero entries produce zero-entry indexes. `gix-index` handles
this; the resulting file is just the header + signature. `git status`
reports "nothing to commit, working tree clean." No special case needed.

## 8. Test plan

### 8.1 Unit tests (`crates/projgit-core/src/dotgit.rs`)

- `index_round_trips`: build via `build_index_bytes`, parse back via
  `gix_index::State::from_bytes`, assert entry count and paths match
  HEAD's tree.
- `index_has_assume_valid_on_every_entry`: every entry in the parsed
  state has the bit set.
- `index_timestamp_equals_commit_time`: two builds with the same
  `(tree_oid, commit_time)` produce byte-identical output.
- `executable_file_entry_has_mode_file_executable`.
- `symlink_entry_has_mode_symlink`.
- `empty_tree_produces_empty_index`: header + signature only; parses
  cleanly with zero entries.

### 8.2 Integration test

In `crates/projgit-core/tests/projection_fs.rs` (or sibling): build an
A1+ overlay against a small fixture repo, walk `.git/index` through
the projection layer, parse the resulting bytes, assert entries.

### 8.3 Live end-to-end test

`crates/projgit-fuse/tests/mount_real_remote.rs::mount_real_remote_with_dotgit_a1_plus_shows_clean_status`:

1. Partial-clone `rust-lang/log` (same fixture as the existing test).
2. Mount via FUSE with `a1_plus_overlay`.
3. Run `git status --porcelain` — assert exit 0, empty stdout.
4. Run `git status` — assert "nothing to commit, working tree clean"
   appears.
5. Run `git diff` — assert exit 0, empty stdout.
6. Run `git diff --cached` — assert exit 0, empty stdout.
7. Run `git ls-files` — assert exit 0, non-empty stdout.
8. Re-run the existing A1 assertions (`git rev-parse HEAD`, `git log`)
   to confirm A1+ is a strict superset.

Network-gated by `PROJGIT_NETWORK_TESTS=1`, `#[ignore]` like the rest.

## 9. Documentation updates landing with the implementation

- This file becomes `Status: shipped`.
- [`dotgit-synthesis.md`](dotgit-synthesis.md) §4: name the axis-split
  insight, add an A1+ row to the variants table.
- [`dotgit-synthesis.md`](dotgit-synthesis.md) §5: add an A1+ column to
  the UX matrix (three rows go green).
- [`dotgit-synthesis.md`](dotgit-synthesis.md) §6: add A1+ row to the
  complexity table (~200 LOC).
- [`dotgit-synthesis.md`](dotgit-synthesis.md) §9.5: record A1+ as the
  new shipped state; A2 still explicitly deferred.

## 10. Open follow-ups

- **A2 ref visibility.** Now cleanly separable from the `git status`
  win. Worth doing for the IDE branch indicator UX, but the read-side
  cleanliness — which is what most users notice — already lands with
  A1+. Lower priority than it was.
- **Subtree A1+.** Compute the subtree's own tree OID; from_tree on
  that. Probably ~30 LOC on top of this work. Defer until someone asks.
- **Cross-mount index caching.** Keyed by `(tree_oid, commit_time)`;
  store under `$XDG_CACHE_HOME/projgit/index/<oid>.idx`. Useful if
  bench shows the build cost matters at 100K+ entries. Defer.
- **`update-index` graceful failure message.** Could synthesize a
  better error in the FUSE adapter when something tries to create
  `.git/index.lock`. Tiny polish.
