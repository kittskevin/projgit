# Writable worktrees (Phase 2) — implementation plan

> Status: **living doc.** Tracks how the
> [writable-worktrees design](../design/writable-worktrees.md) actually
> gets built, now that the §10.1 **no-fork gate is PASSED** (spike:
> [`../../spikes/writable-nofork/`](../../spikes/writable-nofork/),
> verdict in its `RESULTS.md`).
>
> Last updated: 2026-07-18 (**Stages 1–7 shipped**; **CLI `--writable`
> shipped** with branch + remote wiring **and committed-work persistence
> across unmount** — the scratch git dir is reused, not recreated, and its
> objects are shared into the CAS. Remaining: the *uncommitted* in-memory
> upper, git-checkout integration, and the crash journal).
>
> Design in [`../design/writable-worktrees.md`](../design/writable-worktrees.md);
> this is one level down — concrete stages, file changes, and what each
> stage must prove before the next. Mirrors the plan pattern of
> [`cache-transform-tier-plan.md`](cache-transform-tier-plan.md) and
> [`projgitd-plan.md`](projgitd-plan.md).

## 0. Why this doc exists

The spike answered *can it be done without a fork* (yes) and converted
the design's open questions into **four bounded build requirements**.
This doc turns those requirements + the design's layering (§4–6) into an
ordered, reviewable build sequence, and pins the gating decisions.

### The four spike-surfaced requirements (the backbone)

| # | Requirement | Spike finding it answers |
|---|---|---|
| **R1** | **Writable-mode index synthesis** — real stat (size-from-header, stable commit-time mtime), **no `ASSUME_VALID`**, `core.checkStat=minimal`. | The naive `read-tree` index hydrates every file on the first `status`; real synthesized stat makes the first status clean *and* lets edits be detected. |
| **R2** | **Projection honors the sparse cone** — don't surface out-of-cone paths. | Sparse-index expanded back to a full index (slow) because the VFS still presented out-of-cone files. |
| **R3** | **Daemon FSMonitor** — monotonic **timestamp** tokens + precise modified-paths from the materialized set. | git rejects small-integer tokens (stores `0`, drops all deltas); timestamp tokens work; there is a one-query settle lag. |
| **R4** | **FUSE invalidation on write/checkout** — `Notifier::inval_inode`, not attr-cache TTL. | Stale attr cache after a write gave a false-negative `status`; spike used TTL=0 as a shortcut. |

### What already exists to build on

- **Read-only `.git/` synthesis** ([`dotgit.rs`](../../crates/projgit-core/src/dotgit.rs)):
  `a1_plus_overlay` already synthesizes a `.git/index` matching HEAD —
  but with `ASSUME_VALID` on every entry (correct for read-only, *wrong*
  for writable; R1 replaces this strategy for writable mounts).
- **`ObjectStore`**: `header()` (size without content — the size-from-
  header primitive R1 needs), `commit_time()`, `commit_tree()`.
- **The cache+transform tier** (Phase 1): the immutable derived LOWER
  baseline this layers strictly on top of.
- **projgitd** substrate: the per-host daemon that R3's FSMonitor and
  worktree creation reuse.
- **The spike** (`spikes/writable-nofork/`): a working reference for the
  end-to-end behavior each stage must reproduce in production code.

## 1. Phasing & seam constraints

Writable is an **opt-in layer** (design §0): read-only mounts (the
agent-eval hot loop) keep their thin, no-materialization-tax shape. Only
a mount created explicitly as a *worktree* pays the writable cost. Every
stage must preserve that split — no read-only regression.

## 2. Stage sequence

```
Stage 1  R1: writable-mode index synthesis (projgit-core)   <- START, testable now
Stage 2  Writable FUSE path: upper/overlay + write ops, --writable gate
Stage 3  R4: FUSE invalidation on write (Notifier::inval_inode)
Stage 4  R3: daemon FSMonitor (timestamp tokens, precise modified-paths)
Stage 5  R2: projection honors the sparse cone
Stage 6  Commit path via gix (materialized set -> trees -> commit)
Stage 7  checkout-under-live-mount (§10.7) + upper crash-consistency
```

Stop-the-line gates between stages are the "must prove" lines below.

### Stage 1 — R1: writable-mode index synthesis  *(SHIPPED 2026-06-19)*

**What.** `projgit_core::dotgit::build_writable_index_bytes(store,
commit_oid)` produces seed `.git/index` bytes for a writable worktree:
entries carry real `mode` + `oid` (as today) **plus** real `size` (from
`ObjectStore::header`, no content read) and a stable `mtime` (from
`ObjectStore::commit_time`), and **do not** set `ASSUME_VALID`. Paired
with `dotgit::WRITABLE_CORE_CONFIG` (`core.checkStat = minimal`) so git
compares only mtime+size (ignoring the dev/ino/uid/gid that a synthesized
index can't predict for a not-yet-existing mount).

**Why this is the start.** It's a pure function, unit-testable without a
mount, reuses existing `ObjectStore` primitives, and is the load-bearing
fix for the spike's one-time-hydration finding. The read-only
`a1_plus_overlay` path is left untouched (no regression).

**Must prove (Stage 1 exit):**
- The bytes round-trip: gix/git can read the index, `ls-files` shows all
  paths, entries have non-zero size = blob size and mtime = commit time.
- `ASSUME_VALID` is not set on any entry.
- Read-only `a1_plus_overlay` output is byte-for-byte unchanged.

*All three proven by `tests/dotgit_index.rs`
(`writable_index_*` + the unchanged read-only tests).* The live-mount
"hydration-free first status" claim is already evidenced by the spike
(its `update-index --refresh` produced exactly this size+mtime stat and
status #3 had `reads=0`); the production wiring lands in Stage 2.

**Defer to Stage 2:** writing the seed to a *real* per-worktree
`.git/index` (the read-only mount keeps the synthetic overlay index;
writable mounts need a real writable `.git`).

### Stage 2 — writable FUSE path  *(SHIPPED 2026-06-19)*

**What.** `projgit_fuse::WritableFs<F: FsProvider>` — a `fuser::Filesystem`
that layers an in-memory UPPER materialization store over the read-only
LOWER `FsProvider` projection, implementing `read` / `write` / `create`
/ `setattr` / `unlink` / `mkdir` / `rmdir` / `rename` with
materialize-on-write (copy lower→upper on first write, serve
upper-over-lower; new entries get fresh inodes in the synthetic space,
disjoint from lower tree inodes). `mount_writable_background` mounts it
read-write (no `MountOption::RO`); the default read-only `mount_background`
path is untouched. The per-worktree `.git` is external (seeded with
Stage 1's writable index).

**Proved (integration test `tests/writable_mount.rs`, real FUSE mount):**
clean `git status` on the fresh mount (no fork — also the end-to-end
validation of R1); edit a tracked file → `status` reports exactly
`M dir/a.txt` → `git add` stages it; create a new file → `readdir`
merges it → `status` shows `?? dir/new.txt` → `git add`; an untouched
file stays virtual (served from the lower projection).

**Deferred within Stage 2:** in-memory upper (on-disk overlay + crash
consistency = Stage 7); attr-cache TTL is 0 (Stage 3 / R4 restores it
with FUSE invalidation).

**CLI `--writable` (SHIPPED 2026-06-19).** `projgit mount --writable
<src> <mnt>` now works end to end: projgit creates a real, writable
scratch git dir on disk (detached `HEAD` at the projection commit,
`objects/info/alternates` → the shared store, a writable seeded index,
`core.worktree` = mountpoint, `core.checkStat = minimal`) and synthesizes
the mount's `.git` as a `gitdir:` **link file** to it, then mounts via
`mount_writable_background`. Stock git inside the mount does
`status`/`add`/`commit` against the on-disk git dir while the worktree is
the FUSE mount; cold blobs hydrate on read through the projection. Proved
by `crates/projgit-cli/tests/writable_mount_cli.rs` (real CLI subprocess:
clean status → edit + new file → `M`/`??` → `add` → `commit` → verified
content). First-cut limits: in-memory upper (edits lost on unmount unless
committed); not combinable with `--subtree`/`--no-dotgit`/`--daemon-socket`.

**Remote + branch wiring (SHIPPED 2026-07-18).** The writable scratch
git dir now starts on a **named branch** (symbolic `HEAD` →
`refs/heads/<branch>`: the `--ref` branch, or the clone's default HEAD
branch; `--commit` stays detached) and inherits the source's
`remote.origin.url`, wiring branch upstream. So the full dev loop works:
`edit → git commit → git push` to a remote branch, no manual `git remote
add` / `switch -c`. Proved by
`writable_mount_cli.rs::cli_writable_mount_commit_and_push_to_branch`
(bare remote + source: mount on `main`, edit, commit, `git push`, and the
bare remote receives the content).

**Committed-work persistence across unmount (SHIPPED 2026-07-18).** The
scratch git dir now lives under `<cache>/worktrees/` (keyed by
mountpoint) and is **reused, not recreated** — if a valid one exists, its
refs + index are kept and the LOWER projection is re-pinned to its
persisted `HEAD`. Crucially, the scratch `objects` is a **symlink into
the shared clone CAS** (replacing the old read-only `alternates`), so
commits land in the durable, shared store where the projection's
`ObjectStore` can read them back on remount (and push still works).
Proved by
`writable_mount_cli.rs::cli_writable_mount_persists_commit_across_remount`
(commit inside the mount, unmount, remount the same mountpoint → `HEAD`,
clean `status`, and the edited + new files are all restored). Remaining:
the *uncommitted* in-memory upper (edits not yet committed are still lost
on unmount — the on-disk upper / journal below).

### Stage 3 — R4: FUSE invalidation on write  *(SHIPPED 2026-06-19)*

**What.** `WritableFs` now carries an optional off-thread invalidator:
every materializing `write`/`setattr` enqueues an `inval_inode`, and
`create`/`mkdir`/`unlink`/`rename` enqueue an `inval_entry`, drained by a
dedicated worker thread holding the post-mount `fuser::Notifier`
(calling the notifier from inside a handler can deadlock the kernel).
With invalidation in place the attr/entry cache TTL is restored to a
useful value (1s) via `mount_writable_background`; `WritableFs::new`
(no invalidator) keeps TTL=0 and stays correct.

**Proved** (`tests/writable_mount.rs`, new step 5): a **same-size**
in-place edit — where size is unchanged so only the mtime distinguishes
it, and the kernel would otherwise serve a cached `getattr` within the
TTL — is correctly detected by `git status` as `M dir/b.txt`.

### Stage 4 — R3: daemon FSMonitor  *(SHIPPED 2026-06-19)*

**What.** `WritableFs` maintains an authoritative **write-log**: it
reconstructs each inode's worktree-relative path from `lookup` calls,
and every mutating handler records the changed path into a cumulative
set behind a **monotonic nanosecond token** (git rejects small-integer
tokens). When `MountConfig::fsmonitor_file` is set, the overlay rewrites
that file (`<token>\0 <path>\0 ...`) on every change; a `core.fsmonitor`
hook (query protocol v2) streams it to git, which then skips scanning.

**Proved** (`tests/writable_mount.rs::writable_mount_fsmonitor_write_log`,
real mount + a hook): clean `status` under fsmonitor stays clean (no
false positives); after an in-mount edit the write-log lists the path
and git reports `M dir/a.txt` (one settle query absorbs git's documented
post-change lag). The scan-cost reduction itself was quantified in the
spike (349→30 getattr).

**Deferred:** answering the fsmonitor query over a live socket from a
long-running daemon (vs the file the hook reads) — the write-log is the
same; only the transport differs.

### Stage 5 — R2: projection honors the sparse cone  *(SHIPPED 2026-06-19)*

**What.** `MountConfig::sparse_cone` (cone-mode directories). When
non-empty, `WritableFs` applies cone-mode visibility in `readdir` and
`lookup`: files in the root and in directories leading to a cone dir are
shown, cone directories are shown recursively, and everything else is
hidden (out-of-cone `lookup` returns `ENOENT`, out-of-cone entries are
dropped from `readdir`). This is what keeps git's sparse-index collapsed
— the spike showed it expands to a full index when the VFS surfaces
out-of-cone worktree content.

**Proved** (`tests/writable_mount.rs::writable_mount_sparse_cone_hides_out_of_cone`):
with `cone = [dirA]`, the root lists `README.md` + `dirA` but not `dirB`;
`dirA` contents are visible; `dirB` is neither stat-able nor listable.

**Index pairing (shipped).** `dotgit::build_writable_index_bytes_sparse`
seeds `SKIP_WORKTREE` (as an extended/V3 index entry) on every out-of-cone
path, so git knows the hidden files are intentionally absent and `status`
stays clean (without it, git reports the hidden files as *deleted*).
Tested in `tests/dotgit_index.rs::sparse_index_sets_skip_worktree_outside_cone`.

### Stage 6 — commit path  *(SHIPPED 2026-06-19, correctness)*

**What.** The full `edit → git add → git commit` cycle works against the
writable overlay using stock git's own commit machinery (objects written
to the canonical odb via the external git-dir). 

**Proved** (`tests/writable_mount.rs`, step 3b): after editing a tracked
file and creating a new one, `git commit` succeeds; `git cat-file -p
HEAD:dir/a.txt` carries the edit, `HEAD:dir/new.txt` has the created
content, and `git status` is clean again post-commit.

**Deferred:** the *optimization* of deriving trees directly from the
materialized set via `gix` (with the cache-tree extension) so commit
cost is proportional to the change rather than git re-reading the
worktree. Correctness is in hand; this is a perf follow-up.

### Stage 7 — checkout-under-live-mount + crash consistency  *(swap + edit-survival SHIPPED 2026-06-19; follow-ups remain)*

**Shipped.** `WritableFs` now shares its `lower` baseline + `up` state
(both `Arc<Mutex<…>>`), and `mount_writable_background_with_handle`
returns a `WritableHandle` whose `swap_baseline(new_lower)` swaps the
LOWER projection under the live mount — a `checkout` of a different
commit. The upper layer is **path-keyed** (`edits: HashMap<String, Edit>`
+ `whiteouts: HashSet<String>`), so local edits **survive a checkout**
(EdenFS semantics: a local edit shadows the checked-out version); only
the per-baseline inode cache is cleared on swap. The swap enqueues
precise `inval_entry(parent, name)` + `inval_inode` for every previously
looked-up inode (tracked via `inode_parent_name`) through the Stage 3
off-thread invalidator, so the kernel re-resolves changed paths by name
against the new baseline, and advances the FSMonitor token.

**Proved** (`tests/writable_mount.rs::writable_mount_swap_baseline_serves_new_commit`,
real mount): mount commit A (`README=A`, `dir/x.txt=one`), `swap_baseline`
to commit B — the mount re-virtualizes (`README=B`, `dir/x.txt=two`, the
B-only `dir/y.txt` appears); then edit `dir/x.txt` under B and swap back
to A — `README` re-virtualizes to A but the local edit survives the
checkout (`dir/x.txt` = `two\nEDIT\n`, shadowing A's `one\n`).

**Remaining follow-ups.**
1. **git-checkout integration**: have git's `checkout` *drive* the swap
   (update HEAD/index + `swap_baseline`) instead of materializing the
   whole diff into the worktree — the daemon observing the HEAD change
   + sparse/skip-worktree is the likely seam.
2. **Crash consistency / uncommitted-edit durability**: *committed* work
   now persists (scratch dir reuse + shared-CAS objects, above), but the
   upper is still in-memory, so *uncommitted* edits are lost on unmount.
   Move the upper to an on-disk overlay, then add a minimal journal
   (§10.3). Small blast radius (immutable LOWER), but nonzero.

**Recommended order for the follow-ups:** on-disk upper (uncommitted-edit
durability) → git-checkout integration → journal.

## 3. Out of scope (this plan)

Root-ward materialization locking under a concurrent multi-thousand-file
build (design §10.4 — revisit if a real workload stresses it), WinFsp
writable (§10.6 — separate Windows track), and making the agent-eval hot
loop writable (it stays read-only by design).
