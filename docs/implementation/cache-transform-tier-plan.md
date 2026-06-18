# Cache + transform tier — implementation plan

> Status: **living doc.** Tracks how the
> [cache-transform-tier design](../design/cache-transform-tier.md)
> actually gets built. Updated as each stage lands or surfaces
> something that changes downstream stages.
>
> Last updated: 2026-06-18 (created; no stages started).
>
> Design in [`../design/cache-transform-tier.md`](../design/cache-transform-tier.md);
> this is one level down — concrete steps, file changes, commit
> boundaries, decision points. **Phase 1 is read-only**
> worktrees/checkouts; **Phase 2 (writable mounts) is deliberately left
> open** and lives in
> [`../design/writable-worktrees.md`](../design/writable-worktrees.md).

## 0. Why this doc exists

Mirrors the plan pattern of
[`sparse-access-plan.md`](sparse-access-plan.md) and
[`projgitd-plan.md`](projgitd-plan.md): design = what + why; plan =
how + what to learn from each step before the next.

The design doc settled the model (split *acquire* from *serve*; flip
to eager-bulk-plus-derive) and the architectures (**A** lazy-warm
floor, **B** prefetch-warm default, **C** full-hydrate policy). What it
left for this doc (design §15): a Stage-style sequence that reuses the
existing [`projgitd`](projgitd.md) substrate and pins the two gating
open questions first.

**The phasing decision (this doc's core claim):** ship the entire
read-only tier (A→B, C as policy) as **Phase 1** and prove the
amortisation + cold-path wins on the §1 workload *before* touching
writes. Phase 1 must be built so that **none of its seams foreclose the
writable Phase 2** in [`../design/writable-worktrees.md`](../design/writable-worktrees.md)
(§6 below enumerates the seams to preserve).

## 1. Pre-flight (~15 min)

Before writing code:

1. Re-read design
   [`../design/cache-transform-tier.md`](../design/cache-transform-tier.md)
   §4 (miss path), §8 (orthogonal mechanics), §9 (what "derived"
   means), §14–15 (open questions + next steps).
2. Re-read [`projgitd-plan.md`](projgitd-plan.md) — Phase 1 reuses the
   per-host daemon + sidecar substrate. `cached` ≈ the projgitd
   daemon; the "VFS daemon" ≈ the sidecar.
3. Skim the existing ingest machinery this relocates (not rewrites):
   - `crates/projgit-core` — `Fetcher` / `GitCliFetcher`, the
     `Coalescer` (from [`fetch-coalescing.md`](../design/fetch-coalescing.md)),
     `PrefetchClaims` (from
     [`prefetch-coalescing.md`](../design/prefetch-coalescing.md)),
     `tree_cache` / `blob_cache`.
   - `crates/projgit-daemon` — `server.rs`, `ActiveBackend` (the
     `Clone`/Arc fix from the 2026-06-18 data-plane session).
4. Confirm the bench harness reused for validation:
   `crates/projgit-cli/examples/bench_mount.rs` (barrier-N-thread,
   concurrent report, disk-bytes accounting).

## 2. Phasing overview

```
Phase 1 — READ-ONLY tier (this plan)
  Stage 0  Pin wire protocol + derived-index format        (design Q1,Q2)
  Stage 1  cached process + miss-trigger RPC  ............  Architecture A (floor)
  Stage 2  transform/derive: trees, blobs, stat  .........  the "derived index"
  Stage 3  prefetch-warm: relocate prefetch into cached ..  Architecture B (default)
  Stage 4  full-hydrate as an escalation policy  .........  Architecture C (policy)
  Stage 5  maintenance loop: MIDX + repack + commit-graph .  cross-commit dedup

Phase 2 — WRITABLE mounts (design space OPEN)
  → ../design/writable-worktrees.md ; gated by the no-fork spike.
    Phase 1 must preserve the seams in §6 so Phase 2 is additive.
```

Stop-the-line gate between phases: Phase 2 does **not** start until the
[`../design/writable-worktrees.md`](../design/writable-worktrees.md)
§10.1 no-fork spike is run, regardless of Phase 1 status.

## 3. Stage 0 — Pin the two gating decisions (no serving code yet)

Design §15 says open questions 1–2 gate everything. Resolve them as a
short written decision in the design doc before Stage 1.

### 3.1 Goal

A committed decision on (a) the daemon↔`cached` **miss-trigger wire**
and (b) the **derived-index on-disk format**, each with a one-paragraph
rationale, written into design §14 open-questions (flip them from
"open" to "decided").

### 3.2 Decisions to make

1. **Miss-trigger wire (design Q1).** Reuse the existing `Fetcher`
   trait wire ([`fetchers.md`](../design/fetchers.md)) vs a new minimal
   "need OID(s) + dir hint, reply ready/bytes" message.
   - *Default lean:* extend the existing projgitd RPC with a
     `fetch(OIDs, hint)` that returns `ready` (trigger-and-reread, design
     §8) — smallest delta over today's daemon.
   - *Decision point:* if Stage 2's derived format wants `cached` to
     return *derived* records (not raw bytes), the wire must carry a
     record kind. Decide now whether the wire is OID→bytes or
     OID→(kind, record).
2. **Derived-index format (design Q2).** On-disk layout, OID-keying,
   one shared store vs sharded.
   - *Default lean:* one host-shared store beside the canonical odb
     (design §3 invariant 2); content-addressed files keyed by OID so
     writes are atomic-rename and never need invalidation (design §10).
   - *Decision point:* expanded-tree and stat records can live in a
     single sidecar DB or as loose OID-keyed files; pick based on
     readdir-lookup latency, not write convenience.

### 3.3 Stop condition

Both questions have a written decision + rationale in design §14.
**No `cached` code until this lands** — these choices set every
interface downstream.

## 4. Stage 1 — `cached` + miss-trigger RPC (Architecture A, the floor)

Design §5: trees eager at mount; blobs fetched on miss *through*
`cached`; daemon has **no** backend call (kills the `state.active`
serialization the 2026-06-18 session diagnosed).

### 4.1 Goal

A mount served by a thin VFS daemon that, on a blob miss, triggers a
fetch through `cached` (coalesced, single-connection) and then
re-reads the shared store directly. Trees are prepped at mount so
`readdir`/`stat` never hit the network (design §4 softener 1).

### 4.2 Concrete changes

1. **`cached` role in `projgit-daemon`** — formalize the daemon as the
   sole upstream owner: it holds the `GitCliFetcher` + `Coalescer` +
   `PrefetchClaims`; sidecars hold none. (Mostly a *responsibility*
   move — projgitd already owns the fetcher; this removes any
   fetch path from the sidecar.)
2. **Miss-trigger RPC** — implement the Stage-0 wire: sidecar
   `read` miss → `cached.fetch(OID, hint=dir)` → coalesce/join
   in-flight → one batched pack request for siblings → write odb +
   (Stage 2) derived → reply `ready`.
3. **Sidecar hot path is IPC-free** (design §3 invariant 1): after
   `ready`, the sidecar re-reads the shared store via mmap; `cached` is
   never in the per-read path on a hit.
4. **Eager trees at mount** — at mount, `cached` fetches the commit's
   reachable *trees* (not blobs) so enumeration is network-free; the
   sidecar's tree index is fully local.

### 4.3 Stop condition

On the bench target: `os.walk` of a cold mount is network-free after
mount-time tree prep; a cold blob read blocks **once per OID across all
mounts** (not once per mount); the daemon-side `state.active`
serialization is gone (no backend call in the sidecar). Validate the
"once per OID across mounts" with the existing barrier-N-thread bench
(N sidecars, same commit).

### 4.4 Decision points

- If trigger-and-reread shows measurable double-read overhead on large
  blobs, note it for the Stage-2/§8 stream-through optimization — do
  not build streaming yet.
- Confirm `cached` survives a sidecar crash and vice-versa (design §8
  process boundary). If not separable cleanly, record why.

## 5. Stage 2 — transform / derive (the "derived index")

Design §9: OID-keyed derived structures that make serving a local
lookup instead of a parse/chain-walk. This is idea (C) transform —
the unconditional-win half.

### 5.1 Goal

`readdir`, `stat`, and `read` are served from **derived** records, not
from re-parsing trees or reconstructing delta chains on the hot path.
Canonical odb stays the source of truth (design §3 invariant 2).

### 5.2 Concrete changes (build in this order)

1. **Precomputed stat** (cheapest, highest leverage) — size + mode
   (+ optional content hash) per entry so `stat()` never touches a
   blob. This is the local stand-in for Mononoke's pushed-down tree
   metadata (design §9).
2. **Expanded directory listings** — tree object → ready-to-serve
   `readdir` entries (name, mode, type, child OID), so listing is a
   lookup, not a parse.
3. **Undeltified, path-addressable blobs** — reconstruct delta chains
   once at ingest so `read()` is an mmap.

Each writes through `cached` at ingest into the Stage-0 derived store,
keyed by OID; the sidecar reads them directly.

### 5.3 Stop condition

Warm `readdir`/`stat`/`read` touch only derived records (verify with a
trace/counter that no tree-parse or delta-walk happens on the hot
path). Derived store is rebuildable from the odb alone (delete it,
re-derive, identical serving) — proving the "present-or-rebuild, never
stale" property (design §10).

### 5.4 Decision points

- Measure derived-storage amplification vs the odb. If undeltified
  blobs blow up disk on the ~140 GB target, gate blob-undeltification
  behind a policy (keep stat + expanded-trees always-on).

## 6. Stage 3 — prefetch-warm (Architecture B, the default)

Design §6: `cached` runs predictive prefetch so most reads are hits
before the syscall arrives. This **relocates**
[`prefetch.md`](../design/prefetch.md)'s T1–T5 ladder into `cached` as
its eagerness policy — the existing `PrefetchClaims`/`Coalescer` work
is the ingest machinery, not wasted (design §11).

### 6.1 Goal

On the §1 workload, the common case is a hit: prefetch converts
would-be-misses into hits ahead of demand. On-demand (Stage 1) remains
the correctness floor (design §4 / [`prefetch.md`](../design/prefetch.md)
non-goal).

### 6.2 Concrete changes

1. Move the prefetch engine to run *inside* `cached`, fed by sidecar
   `readdir`/`read` access signals over the Stage-0 wire.
2. Apply **tiered eagerness** (design §6.1): trees eager; blobs
   warm-lazy driven by prefetch hints — *not* eager-everything.
3. Single-flight becomes intrinsic ("fetch each OID once" in `cached`)
   rather than racing across sidecars (design §11).

### 6.3 Stop condition

Bench shows the cold-read tail shrinks vs Stage 1 on the realistic
mixed workload, **without** disk/bandwidth blowing past `∝ touched +
bounded prefetch slack` on the sparse-unique tail (design §13 risk
row). Quantify prefetch slack as a ratio; cap it.

## 7. Stage 4 — full-hydrate as an escalation policy (Architecture C)

Design §7: C is the degenerate extreme of B (prefetch policy =
"fetch everything reachable"), justified *only* by saved
materialization + sharing under high concurrent-mount demand on one
commit — never by fetch efficiency (its fetch cost = a clone).

### 7.1 Goal

`cached` can *escalate* a hot commit to full hydrate when it observes
high concurrent-mount demand, and *not* otherwise. Catastrophic for
sparse-unique on a large repo, so it must be demand-gated, off by
default.

### 7.2 Concrete changes

1. A policy hook in `cached`: signal = N concurrent mounts on one
   commit over a threshold ⇒ hydrate reachable blobs once, serve purely
   local thereafter.
2. Guardrail: never auto-escalate above a configured repo-size / mount
   threshold; require an explicit opt-in flag above it.

### 7.3 Stop condition

A→B unchanged when the signal is absent; with the signal present on a
high-overlap same-commit bench, post-hydrate reads are network-free and
mount stays under criterion #1 (root tree sync, rest streamed — design
§6.1 residual cost).

## 8. Stage 5 — maintenance loop (cross-commit physical dedup)

Design §14: content-addressing gives *logical* cross-commit dedup, but
*physical* dedup (one disk copy, one page-cache page per object) needs
the CAS kept packed. Many per-commit fetch-packs otherwise accumulate
overlapping objects. This stage adds the **Scalar object-store
maintenance** `cached` runs in the background — the piece that makes the
cross-commit amortisation real on the §1 "different commits" workload.

### 8.1 Goal

`cached` keeps the shared CAS physically deduped and fast to look up as
many commits' packs accumulate: each object lives once, lookups stay
O(log total) across all packs, history queries are fast — all off the
serving path, without disrupting in-flight mmap readers.

### 8.2 Concrete changes

1. **MIDX** (`git multi-pack-index write`) — one OID→(pack, offset)
   index across all per-commit fetch-packs, so lookup doesn't scan N
   idx files. Enables "many cheap packs + fast lookup" without forcing
   a full repack.
2. **Incremental repack** (`git repack`, Scalar-style: collapse the
   small packs, leave the big base) — the physical-dedup lever: one
   disk copy / one page-cache page per object, shared across all
   commits referencing it.
3. **commit-graph** (`git commit-graph write`) — fast `git log` /
   merge-base inside mounts (criterion #4), via the synthesised
   `.git/`.
4. **Scheduling** (design Q5) — run on a timer / idle / pack-count
   threshold. The immutable-write discipline (design §14: temp + fsync
   + atomic rename, never mutate a published pack) makes a repack+MIDX
   swap safe under live mmap readers; the open part is *cadence*, not
   safety.
5. **Promisor-safe pruning** — never prune promisor objects in a
   partial clone.

MVP shells to `git maintenance run --task=...` on the shared CAS;
gix-native maintenance is a later "drop the git CLI" item. Formats stay
stock git, so the CAS stays tooling-readable (design §3 invariant 2 /
problem-statement §5).

### 8.3 Stop condition

On a multi-commit bench (mount several nearby commits of one repo):
total CAS disk ≈ union of objects (not Σ per-commit), object lookup
stays flat as pack count grows (MIDX), and a second mount of a
maintained commit sees warm page-cache hits for shared objects.
Maintenance never stalls a serving read (verify no reader error during
a repack+MIDX swap).

### 8.4 Decision points

- If incremental repack of the big base pack ever dominates, bound it
  by pack-size / geometric-repack settings; never block serving on it.
- Shell-to-git vs gix-native: decide based on whether the git CLI
  dependency is acceptable in `cached` (it already is for
  `GitCliFetcher`).

## 9. Phase 2 seam — what Phase 1 must NOT foreclose

Phase 1 is read-only, but every stage must preserve the seams the
writable Phase 2 ([`../design/writable-worktrees.md`](../design/writable-worktrees.md))
needs, so writable is **additive**, not a rewrite:

1. **LOWER stays immutable + content-addressed.** The derived store
   (Stage 2) is OID-keyed and never mutated in place — this *is* the
   writable LOWER baseline (writable §4.1). Do not add any
   mount-specific mutation to it.
2. **Sidecar = mount identity.** Keep one daemon ("VFS daemon") per
   mount as the unit that *could* later own a writable UPPER overlay
   (writable §4.2). Do not collapse all mounts into one server in a way
   that loses per-mount identity.
3. **`.git/` synthesis stays a per-mount overlay.** The A1 RootOverlay
   ([`dotgit-synthesis.md`](../design/dotgit-synthesis.md)) is where the
   future virtual index + FSMonitor config live (writable §4.3). Keep
   it per-mount and extensible.
4. **`getattr`/`readdir` stay cheap + side-effect-free.** The no-fork
   thesis (writable §7) depends on stat never hydrating. Do not add
   fetch-on-stat as a Stage-1/2 shortcut.
5. **Derived store is shareable across mounts.** N mounts on one commit
   share one LOWER (Stage 1.4 / Stage 4) — this is exactly the
   O(1)-worktree property (writable §5). Keep sharing intrinsic.

If any Phase 1 stage is tempted to violate one of these for short-term
simplicity, record the trade in this doc and flag it against
writable-worktrees §10 before proceeding.

## 10. Status & next steps

Phase 1 proposed, not started. **Start at Stage 0** — pin the
miss-trigger wire + derived-index format into design §14, because every
downstream interface depends on them. Do not write `cached` serving
code until Stage 0 lands.

Phase 2 (writable) is intentionally undecided and gated on the
[`../design/writable-worktrees.md`](../design/writable-worktrees.md)
§10.1 no-fork spike, which can be run independently of Phase 1 progress.
