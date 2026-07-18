# RESULTS — Phase 2 no-fork spike

Date: 2026-06-19. Environment: projgit devcontainer (Debian 12),
`git 2.53.0`, `/dev/fuse` present, `fuser 0.17`. All numbers from
`./run.sh` (small = 300 files) and `NFILES=20000 DIRS=200 ./run.sh
scale`. These are single-run, in-container measurements — *shape*
evidence, not calibrated benchmarks.

## Verdict

**The no-fork thesis holds.** Stock, unmodified `git` drives a virtual
(FUSE) worktree — `status`, `add`, `commit`, sparse-index, FSMonitor —
**correctly and fast with no `core.virtualFilesystem` patch and no
forked git.** Every milestone passed. The spike also surfaced three
concrete engineering requirements (below) that projgit's production VFS
must meet; none of them needs a fork.

This is the strongest "we improved on GVFS" result projgit can claim:
GVFS forked git for exactly the two reasons that **evaporate on our
substrate** — cheap side-effect-free `getattr` (no hydrate-on-stat) and
the upstream FSMonitor/sparse-index toolbox.

## Milestone-by-milestone

### M1 — does `git status` hydrate the virtual worktree? (no fork)

| step | reads (hydrate) | getattr | note |
|---|---:|---:|---|
| `ls` + `find -type d` | **0** | 72 | pure dir walk = tree metadata only |
| `read-tree HEAD` | 0 | 8 | populate index from tree, no worktree touch |
| `status` #1 (naive index) | **300** | 349 | one-time full hydration |
| `status` #2 (warm index) | **0** | 349 | steady state: zero content reads |
| `update-index --refresh` | 0 | 328 | stat-refresh needs no content |
| `status` #3 (post-refresh) | 0 | 349 | |

Correctness: `git status` reports **0 changed entries** — git sees a
clean worktree. ✅

**Finding #1 (the one-time hydration).** A *naive* index built with
`read-tree` (zero stat data) makes the **first** `status` hydrate every
file (300 reads) to confirm it matches HEAD; thereafter git caches the
stat and steady-state status is hydration-free. This one-time cost is
exactly what projgit's **eager index synthesis** (`dotgit-index.md`)
removes: synthesise the index with the correct blob *size* (from the
object header — no content) and a stable mtime, and even the first
status is stat-clean. The cheap-`getattr` claim is confirmed: git's
stat-only comparison is satisfied by the FUSE `getattr` without reading
bytes.

### M2 — sparse-index at scale (no fork)

20000 files, warm index:

| config | status time | hydration | getattr (scan) | on-disk index |
|---|---:|---:|---:|---:|
| full index | 513 ms | **0** | 20,411 | 1.7M |
| sparse-index (cone=`dir000`) | 1001 ms | 0 | 611 | **32K** |

(10000-file run: full index 864K → sparse 20K.)

Sparse-index **composes with the virtual worktree** and shrinks the
on-disk index **~53×** (1.7M → 32K) and the scan **~33×** (20411 → 611
getattr) — GVFS's #1 problem (a physical O(repo) index), solved by the
upstream feature, no fork. ✅

**Finding #2 (the cone must be honored by the projection).** The sparse
status was *slower*, not faster, because git printed *"the sparse index
is expanding to a full index"*: our spike VFS still **presents** the
out-of-cone files, so git sees worktree content outside the cone and
collapses the optimisation. The fix is a real integration requirement:
**projgit's projection must itself honor the sparse cone** (not surface
out-of-cone paths) so the sparse index stays collapsed. The mechanism
works; the VFS has to cooperate with it.

### M3 — FSMonitor answered from the daemon write log (no fork)

| status | reads | getattr (scan) |
|---|---:|---:|
| no fsmonitor | 0 | 349 |
| **fsmonitor (daemon-style hook)** | 0 | **30** |

With a `core.fsmonitor` hook that answers from the harness write log
(zero filesystem scanning), git **trusts the monitor and skips lstat**
on the whole tree: getattr scan drops **349 → 30**. ✅ projgit is
uniquely positioned to *be* this monitor — the daemon already knows
every write authoritatively, so it answers with an exact modified-paths
set and no scan.

**Finding #3 (the FSMonitor token must be timestamp-shaped, and there
is a one-query settle).** git's hook protocol calls the token opaque,
but empirically it **rejects small-integer tokens** (stores `0` and
silently ignores *all* reported deltas — a dangerous false-negative)
and **accepts nanosecond-timestamp tokens**, which it stores and
advances. With a timestamp token, precise-path reporting works. There
is also a documented **one-query lag**: a change made immediately after
a query is reported on the *next* query, not the current one — a
non-issue for an agent doing repeated statuses, but the daemon must mint
monotonic timestamp tokens and the integration must not depend on
single-shot freshness.

### M4 — materialize-on-write: edit → status → add → commit (no fork)

- Edit (`echo >> file` inside the mount): **materialize=1**, read-back
  returns the appended bytes. The untouched 299 files stayed virtual.
- `git status` (normal scan) **and** `git status` (fsmonitor, after
  settle) both report exactly ` M dir000/file000000.txt`. ✅
- `git add` + `git commit` succeed; `git cat-file HEAD:<path>` confirms
  the committed blob carries the appended content. ✅

FUSE puts us **synchronously in the write path** (the materialize
happens *before* the write completes) — the structural advantage over
ProjFS's after-the-fact, "cannot refuse" notifications that the design
§3.1 calls out.

**Finding #4 (cache coherence needs FUSE invalidation).** With a normal
attribute-cache TTL, the first post-write `git status` saw a *stale*
`getattr` (old size/mtime) and reported clean — a false negative. The
spike works around it with TTL=0; production must instead keep a useful
TTL and push a FUSE invalidation (`Notifier::inval_inode`) on every
write. This is exactly the invalidation seam the design flags in §10.7
(`checkout` under a live mount) — it shows up for plain writes too.

## What this means for the design

- §7 / §8 **no-fork thesis: validated.** The composition (cheap getattr
  + upstream sparse-index + FSMonitor-from-daemon + partial clone)
  drives stock git end to end. The fork's two reasons don't apply.
- The spike converts the design's open questions into **four concrete,
  bounded build requirements**, none of which is a fork:
  1. eager index synthesis (size-from-header, stable mtime) to kill the
     one-time hydration — projgit already has the `dotgit-index.md`
     machinery;
  2. the projection must honor the sparse cone so sparse-index stays
     collapsed;
  3. the daemon FSMonitor must mint monotonic timestamp tokens and
     report precise modified-paths (the §6 materialisation set);
  4. push FUSE invalidation on write/checkout (§10.7), not rely on TTL.

## Not covered (deliberately out of scope)

Crash-consistency / journaling of the upper layer (§10.3), root-ward
materialisation locking under a concurrent build (§10.4), `checkout` of
a different commit under a live mount (§10.7 beyond the write case),
WinFsp (§10.6), and `gix`-built commit trees (§10.5). These are
committed-build concerns, not gating questions — the spike's job was the
no-fork gate, and that gate is **open**.
