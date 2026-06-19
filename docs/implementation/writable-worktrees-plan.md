# Writable worktrees (Phase 2) — implementation plan

> Status: **living doc.** Tracks how the
> [writable-worktrees design](../design/writable-worktrees.md) actually
> gets built, now that the §10.1 **no-fork gate is PASSED** (spike:
> [`../../spikes/writable-nofork/`](../../spikes/writable-nofork/),
> verdict in its `RESULTS.md`).
>
> Last updated: 2026-06-19 (plan drafted; Stage 1 / R1 in progress).
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

### Stage 1 — R1: writable-mode index synthesis  *(in progress)*

**What.** A new `projgit_core::dotgit::build_writable_index_bytes(store,
commit_oid)` that produces seed `.git/index` bytes for a writable
worktree: entries carry real `mode` + `oid` (as today) **plus** real
`size` (from `ObjectStore::header`, no content read) and a stable
`mtime` (from `ObjectStore::commit_time`), and **do not** set
`ASSUME_VALID`. Paired with a `core.checkStat = minimal` config so git
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

**Defer to Stage 2:** writing the seed to a *real* per-worktree
`.git/index` (the read-only mount keeps the synthetic overlay index;
writable mounts need a real writable `.git`).

### Stage 2 — writable FUSE path

**What.** A writable mount mode in `projgit-fuse` (today hard-wired
read-only): an UPPER materialization layer (in-memory first, on-disk
overlay later) over the LOWER projection, implementing `create` /
`write` / `setattr` / `unlink` / `rename` / `mkdir` with
materialize-on-write (copy lower→upper on first write, then serve
upper-over-lower). Gated behind a `--writable` flag; the default
read-only path stays untouched. The per-worktree `.git` becomes real +
writable (seeded with Stage 1's index).

**Must prove:** edit a file in the mount → `git status` (normal scan)
reports exactly it → `git add` works; untouched files stay virtual.
(This is the spike's M4 minus FSMonitor.)

### Stage 3 — R4: FUSE invalidation on write

**What.** On every materializing write/setattr, push a
`fuser::Notifier::inval_inode` (attr + data) so the kernel never serves
a stale `getattr`/page after a write. Restores a useful attr-cache TTL
(the spike used TTL=0). Same mechanism seeds the harder Stage 7
checkout-invalidation.

**Must prove:** with a non-zero TTL, the first post-write `status`
detects the edit (no false-negative).

### Stage 4 — R3: daemon FSMonitor

**What.** A `core.fsmonitor` integration answered from the daemon's
authoritative write log: monotonic **timestamp** tokens (git rejects
integer tokens) + the precise modified-paths set (the materialized set
from §6). The query does zero filesystem scanning.

**Must prove:** clean `status` scan cost drops sharply (spike: 349→30
getattr); after an edit, the reported path is detected within the
documented one-query settle.

### Stage 5 — R2: projection honors the sparse cone

**What.** When a sparse cone is configured, the projection must not
surface out-of-cone paths (readdir/lookup hide them), so git's
sparse-index stays collapsed instead of expanding to a full index.

**Must prove:** sparse `status` is *faster* than full (not slower) and
the on-disk index stays small at scale.

### Stage 6 — commit path via gix

**What.** Turn the UPPER materialized set into trees + a commit object
via `gix`, written to the canonical odb, with the cache-tree extension
so commit cost is proportional to the change, not repo size.

**Must prove:** edit → add → commit produces a verified commit
(`cat-file HEAD:<path>` carries the change), matching the spike's M4
commit verification, now via gix rather than shelling to `git`.

### Stage 7 — checkout-under-live-mount + crash consistency

**What.** Swap the LOWER baseline under a live worktree (§10.7):
invalidate only the affected materialized entries via FUSE notify; and
the minimum journaling that lets the bounded UPPER overlay survive a
daemon/host crash (§10.3). The subtlest correctness work; deliberately
last.

**Must prove:** `git checkout <other-commit>` under a live mount serves
correct post-checkout bytes (no stale cache); a kill mid-write leaves a
recoverable overlay.

## 3. Out of scope (this plan)

Root-ward materialization locking under a concurrent multi-thousand-file
build (design §10.4 — revisit if a real workload stresses it), WinFsp
writable (§10.6 — separate Windows track), and making the agent-eval hot
loop writable (it stays read-only by design).
