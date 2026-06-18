# Design: Writable Worktrees — GVFS-style, Without GVFS's Anchors

> Status: **proposed 2026-06-18.** Captures the writable-mount research
> from session: how projgit could go "all the way" to GVFS-style
> writable virtual worktrees with fast worktree creation, what to reuse
> from GVFS/VFS for Git, and where there is room to improve on what GVFS
> got wrong. This is a **design-space document**, not a committed plan —
> the read-only [`cache-transform-tier.md`](cache-transform-tier.md) is
> Phase 1; this is the open Phase 2 it must not foreclose.
>
> Companion to [`../problem-statement.md`](../problem-statement.md)
> (§4.4 GVFS / Scalar prior art, §4.5 EdenFS, §5 the locked read-only
> decision), [`cache-transform-tier.md`](cache-transform-tier.md) (the
> immutable derived baseline this layers on),
> [`dotgit-synthesis.md`](dotgit-synthesis.md) (the synthesized `.git/`
> this extends), and [`windows-symlinks.md`](windows-symlinks.md) /
> [`winfsp-implementation-plan.md`](winfsp-implementation-plan.md)
> (the Windows substrate).

## 0. Why this document exists (and the honest reframe)

[`../problem-statement.md`](../problem-statement.md) §5 locked
**read-only** as the MVP, and [`cache-transform-tier.md`](cache-transform-tier.md)
§10 leaned on it hard: read-only + content-addressed is *why* the
transform tier needs no invalidation, no journal, no FSCK. Going
writable **buys all of that back.** There is no free lunch.

So the framing here is deliberately narrow:

> Writable is an **opt-in layer on top of** the cache-transform tier.
> Read-only mounts (the agent-eval hot loop) keep their simplicity and
> pay none of the materialization tax. Only a mount explicitly created
> as a *worktree* pays it.

With that established, the real question is not "should projgit become
writable" but: **if we did, could we pay the writable tax more cheaply
than GVFS did?** The research says yes — for reasons specific to our
substrate (FUSE/WinFsp, stock-git, content-addressing), and grounded in
what Microsoft itself upstreamed *away* from the GVFS fork.

## 1. What "writable, GVFS-style" means here

Three write models exist; we pick one:

| Model | Behavior | Fit |
|---|---|---|
| **Commit-on-write** (gitfs) | every write becomes a commit | ✗ wrong semantics |
| **Overlay / union** | RO lower + writable upper, reads fall through | partial — the storage mechanism, not the whole answer |
| **Materialization** (EdenFS / GVFS) | track diverged-from-object entries, stage via index, explicit `commit` | ✓ this — real worktree semantics |

"GVFS-style" = **a virtual working tree you can edit, `git status`,
`git add`, `git commit`, and `git checkout` a different commit in** —
where unmodified files are virtual (not on disk) and only touched/
modified files are materialized. The user edits; git sees a normal
worktree; the bytes for untouched files never hit disk.

## 2. What GVFS got right — reuse directly

GVFS's lasting contribution is a toolbox that is now **upstream and
default in stock git**, which [`../problem-statement.md`](../problem-statement.md)
§4.4 already credits. Verified state (2026-06):

- `microsoft/git` is still a live fork (365 commits ahead of
  git-for-windows on `vfs-2.54.0`), but its README states the GVFS
  *protocol* bits are "not appropriate to include in core Git because
  partial clone is the official version of that functionality."
- The **sparse-index** blog reports the index hit **180 MB on a
  2M-file monorepo**, dropping to **<10 MB** with sparse-index, and
  `git status` from **1.3 s → <200 ms**.

Reuse, in priority order:

1. **Partial clone** — the official lazy-object fetch; already
   projgit's `Fetcher` contract ([`fetchers.md`](fetchers.md)).
2. **Sparse-index / cone mode** — directory (tree) entries in the index
   instead of expanding every file. The fix for GVFS's #1 problem.
3. **FSMonitor** — git skips scanning files the monitor did not flag.
   **projgit is uniquely positioned to *be* the monitor** (§7).
4. **commit-graph, multi-pack-index, background maintenance,
   incremental repack** — enable, don't build.
5. **The modified-paths idea** — GVFS's side-channel list of
   hydrated/changed paths. Its principled successor is **EdenFS-style
   materialization** (§6).
6. **The enlistment split** — Scalar puts the worktree in `src/` and
   build artifacts *outside* it. That is EdenFS Redirections and
   exactly [`../problem-statement.md`](../problem-statement.md) §5's
   "writes via overlay" instinct: keep write-heavy generated output
   off the virtual mount.

## 3. What GVFS got wrong — and whether we dodge it

| GVFS pain | Root cause | projgit dodge? |
|---|---|---|
| Index 180 MB, slow `status` | physical O(repo) index | **Yes** — sparse-index upstream + default; *and* we can make it derived (§4.3) |
| Required a **git fork** | `core.virtualFilesystem` hook + GVFS protocol | **Largely** — the fork's #1 reason doesn't apply to FUSE (§7) |
| ProjFS placeholder/tombstone hell, FSCK-every-boot, can't-refuse-writes | ProjFS substrate, async post-hoc notifications | **Yes** — FUSE/WinFsp puts us *in* the write path (§3.1) |
| Server coupling (Azure Repos GVFS protocol) | custom protocol | **Yes** — stock-git partial clone is our contract |

### 3.1 The ProjFS escape

The EdenFS Windows study is a catalogue of ProjFS write-side pain:
unordered post-hoc notifications EdenFS "cannot refuse," working-set
growth that never forgets files, FSCK on every boot, invalidation that
desyncs when a file handle is held open. GVFS rides the same substrate
and inherits the same pain.

projgit chose **FUSE (Linux/macOS) + WinFsp (Windows)**, which put us
*synchronously in the write path*. We can intercept a write to **deny,
redirect, or materialize** it before it completes — rather than being
notified after the fact and reconciling. This is the single biggest
correctness advantage for going writable, and it is structural, not
incidental.

## 4. The writable layering

Built on [`cache-transform-tier.md`](cache-transform-tier.md)'s
immutable derived baseline:

```
   ┌────────────────────────────────────────────────────────────┐
   │  UPPER (writable, per-worktree)                             │
   │   materialized set: created / modified / deleted entries    │
   │   small, bounded by uncommitted change                      │
   └───────────────────────────┬────────────────────────────────┘
                               │  overlay (read falls through on miss)
   ┌───────────────────────────▼────────────────────────────────┐
   │  LOWER (immutable, shared across all worktrees on commit C) │
   │   cache-transform tier's derived projection of C:           │
   │   expanded trees · undeltified blobs · precomputed stat     │
   │   content-addressed ⇒ valid forever ⇒ shared, never copied  │
   └─────────────────────────────────────────────────────────────┘

   VIRTUAL INDEX   = derived from (LOWER tree ⊕ UPPER materialized set)
   FSMONITOR       = answered by the daemon from UPPER (authoritative)
   COMMIT          = UPPER materialized set → trees+commit via gix
```

### 4.1 Lower — immutable, shared

The derived baseline from the transform tier. Because it is
OID-keyed and content-addressed, **N worktrees on commit C share one
copy** and differ only in their UPPER overlays. No per-worktree
checkout, no per-worktree index file.

### 4.2 Upper — the materialized set

A per-worktree record of entries that have diverged from the baseline:
created, modified, deleted. Stored on local disk (an overlay dir, or
genuine `overlayfs` upper on Linux). Writes land here synchronously
(§3.1) and the path is materialized up to the root (§6).

### 4.3 The index, made virtual

Stock git already has sparse-index (small *physical* index). We go one
step further: because we already synthesize `.git/`
([`dotgit-synthesis.md`](dotgit-synthesis.md)) and the tier already
holds expanded trees + precomputed stat, the index can be **derived on
demand from (baseline tree ⊕ materialized set)** rather than
materialized as a file — a virtual sparse-index backed by the tier,
with the cache-tree extension making `commit` fast. GVFS fought the
physical index's *size* forever; we can make it a *projection*.

### 4.4 Commit

`git commit` turns the UPPER materialized set into trees + a commit
object via `gix`, written to the canonical odb. Because writes already
produced odb blobs as they happened (content-addressed on write),
commit cost is **proportional to the change**, not to repo size.

## 5. Fast worktree creation

This is where the cache-tier model pays off hardest:

> **Worktree creation = pair an immutable baseline ref with an empty
> overlay and point a thin daemon at it.**

No checkout. No file copies. No index write. No clone. It is
O(1)-ish — not O(repo), not even O(sparse-cone). Contrast:

- `git worktree add` writes a full index + checks out the tree.
- `scalar clone` sets up sparse-checkout + maintenance + a partial
  clone.
- projgit worktree = `(baseline_ref, fresh_empty_overlay, daemon)`.

The "projection is fast" requirement falls out for free: the LOWER
projection is already computed once in the tier and shared immutably;
a new worktree references it.

## 6. Materialization model (EdenFS-derived, content-addressed)

Adopt EdenFS's **materialized / non-materialized** model (studied in
session) as the write-tracking mechanism:

- A write materializes the file (new blob) and propagates
  materialization **up to the root** — a parent's content hash changes
  when any child does.
- `git status` = diff(materialized set vs derived baseline) — cheap,
  because the materialized set is small and the baseline is
  precomputed.
- A renamed-but-unmodified file is materialized-by-location but
  byte-identical to a known object (EdenFS's orthogonality of
  "materialized" vs "modified-from-commit" applies).

**The content-addressed advantage:** EdenFS pays FSCK-on-every-boot and
an invalidation state machine because *its baseline can change* and the
working copy is the source of truth. projgit's LOWER baseline is
**immutable + content-addressed**, so:

- The baseline can never corrupt; only the small bounded UPPER overlay
  needs crash recovery — a far smaller blast radius than EdenFS
  scanning the whole overlay each Windows start.
- A derived structure is present (use it) or absent (rebuild from odb);
  it can never be *stale*.

## 7. The no-fork thesis (the headline improvement)

GVFS forked git for two things. One is irrelevant to us; the other
*evaporates on our substrate*.

1. **The GVFS protocol** (`gvfs-helper.c`) — irrelevant; we speak
   partial clone, "the official version."
2. **`core.virtualFilesystem`** (`virtualfilesystem.c`) — a hook
   telling git *which paths physically exist* so it skips `lstat`-ing
   millions of virtual files. **GVFS needed this because on ProjFS,
   `lstat`-ing a placeholder *hydrates* it** — stat is expensive *and
   stateful*. That is the "particular needs that prevent improvements"
   the sparse-index blog cites when explaining why VFS for Git could
   not even adopt sparse-index.

projgit's `getattr` / `readdir` are **blob-free, cheap, and
side-effect-free by design** (criterion #2: `readdir` returns tree
metadata without fetching content). **So the single biggest reason the
fork exists does not apply to us.** What remains — "don't make git scan
millions of files" — is solved by the **upstream FSMonitor protocol**,
and projgit is uniquely positioned to answer it: the VFS daemon already
knows every write authoritatively, so it can *be* the FSMonitor and
hand git an exact modified-paths set with **zero scanning**. GVFS
bolted a modified-paths file onto a forked git; we implement a
supported hook and answer it from the component that has ground truth.

**The composition that replaces the fork:**

| GVFS needed a fork for… | projgit uses… |
|---|---|
| index size | upstream **sparse-index** (+ virtual index, §4.3) |
| don't scan all files | upstream **FSMonitor**, answered from the daemon's write log |
| lazy object fetch | upstream **partial clone** (existing contract) |
| don't hydrate-on-stat | **cheap stateless `getattr`** — FUSE/WinFsp advantage over ProjFS |
| commit-graph / MIDX / maintenance | upstream — enable |

The honest caveat is in §10: this composition is **unproven for our
exact setup** and is the first spike, not a settled fact.

## 8. Where projgit can beat GVFS — the thesis

1. **No git fork** — compose upstream sparse-index + FSMonitor +
   partial clone; answer FSMonitor from the daemon's authoritative
   write log instead of a forked modified-paths side-channel.
2. **In-path writes** — FUSE/WinFsp intercept synchronously
   (deny / redirect / materialize); ProjFS notifies after the fact,
   unordered, "cannot refuse."
3. **No FSCK-on-boot** — immutable content-addressed baseline can't
   corrupt; only the small bounded overlay needs recovery.
4. **Virtual index** — derive it from the tier instead of fighting a
   physical one (GVFS's #1 problem).
5. **Cheap stateless `getattr`** — the reason `core.virtualFilesystem`
   existed does not apply.
6. **O(1) worktrees** — immutable shared baseline + empty overlay; no
   checkout.

## 9. Relationship to existing designs

- **[`cache-transform-tier.md`](cache-transform-tier.md)** — provides
  the immutable derived LOWER baseline. Writable is strictly a layer on
  top; the tier itself stays read-only and unaware of overlays. This
  doc is the Phase 2 that §14 of that doc deliberately left open.
- **[`dotgit-synthesis.md`](dotgit-synthesis.md)** — the synthesized
  `.git/` (A1 RootOverlay) extends to carry a virtual index +
  FSMonitor hook config. A2 (symbolic HEAD per ref) and A3 (writable
  illusion) become live again here.
- **[`../problem-statement.md`](../problem-statement.md)** — §5's
  "write path via overlayfs, a separate doc" is *this* doc; §4.4's GVFS
  retreat analysis is the prior art this builds on.

## 10. Non-goals & open questions

### Non-goals

- **Making the agent-eval hot loop writable.** That loop stays
  read-only; writable is opt-in per worktree mount (§0).
- **Reimplementing git's working-tree commands.** We surface a virtual
  worktree to *stock* git; we do not reimplement `status` / `add` /
  `commit`.
- **Server-side anything.** Still client-only, still stock-git remote.

### Open questions

1. **No-fork spike (the gating question).** Does stock-git +
   FSMonitor-from-daemon + virtual sparse-index actually compose
   without a `core.virtualFilesystem` patch? Where exactly does cheap
   `getattr` stop being enough? Build the spike before committing.
2. **FSMonitor mechanism** — implement the FSMonitor IPC/hook protocol
   in the daemon; what is the latency budget for answering a
   modified-paths query, and how does the UPPER set map to its wire
   format?
3. **Overlay crash consistency** — UPPER needs journaling. What is the
   minimum that survives a daemon/host crash mid-write?
4. **Materialization propagation cost** — under a build writing
   thousands of files, what is the cost of root-ward propagation, and
   what locking (cf. EdenFS rename-lock hierarchy + acquire counter)
   prevents the races EdenFS documents?
5. **Commit path via gix** — can `gix` build trees from the
   materialized set efficiently, and write a commit, without a full
   index expansion?
6. **WinFsp writable** — the Windows write path is real work even
   though we skip ProjFS's specific pain; scope separately.
7. **`checkout` a different commit** — swapping LOWER baselines under a
   live worktree: how to invalidate only the affected materialized
   entries, and what the VFS must tell the kernel (FUSE notify) so
   caches don't serve stale bytes.

## 11. Risks — what's genuinely hard (don't let the thesis oversell)

- **The no-fork claim needs proof.** `microsoft/git` is still a live
  fork for real reasons; some are Azure-only, but the composition in §7
  is a hypothesis until the §10.1 spike validates it. Target: "supported
  hooks, maybe a thin patch — not a fork."
- **Write correctness under concurrent tooling.** A build writing
  thousands of files stresses root-ward materialization and lock
  ordering. EdenFS's rename-lock hierarchy + acquire counter exist for
  exactly these races; we inherit those lessons, we don't escape them.
- **Overlay crash consistency** — smaller blast radius than EdenFS
  (immutable baseline + bounded overlay) but nonzero.
- **The read-only simplicity dividend is spent** on any writable mount
  — keep it opt-in so eval mounts stay thin.
- **`checkout` under a live mount** — invalidation is where EdenFS and
  GVFS both bled; FUSE gives us synchronous notify, but it is still the
  subtlest part.

## 12. Status & next steps

Design-space only; **not committed, and explicitly downstream of the
read-only Phase 1** in
[`cache-transform-tier.md`](cache-transform-tier.md) /
[`../implementation/cache-transform-tier-plan.md`](../implementation/cache-transform-tier-plan.md).

The one gating artifact before any writable work is the **§10.1
no-fork spike**: a throwaway harness that mounts a virtual worktree,
configures stock git with sparse-index + a daemon-backed FSMonitor, and
measures whether `git status` / `git add` / `git commit` behave
correctly and fast **without** `core.virtualFilesystem`. If that spike
fails, the writable story changes shape (thin patch, or accept a
narrower scope); if it succeeds, it is the strongest "improve on GVFS"
result projgit can claim.
