# Design: Cache + Transform Tier — Eager Acquisition, Local Serving

> Status: **proposed 2026-06-18.** Captures a model shift discussed in
> session: move object *acquisition* out of the serving daemon into a
> dedicated tier that eagerly fetches and transforms git data, leaving
> the VFS daemons as near-stateless local readers. This is an
> *evolution* of the [`projgitd.md`](projgitd.md) direction, not a
> replacement: projgitd already answered *who owns the upstream
> connection*; this doc answers *what flows through it, in what form,
> and what happens on a cache miss*.
>
> Companion to [`../problem-statement.md`](../problem-statement.md)
> (the agent-eval use case and the §3 lazy/enumerable/multiplex
> properties), [`projgitd.md`](projgitd.md) (per-host daemon owning the
> fetcher), [`prefetch.md`](prefetch.md) (the T1–T5 tier ladder this
> promotes from optimization to core data plane),
> [`prefetch-coalescing.md`](prefetch-coalescing.md) and
> [`fetch-coalescing.md`](fetch-coalescing.md) (the single-flight
> machinery that becomes intrinsic to the cache tier), and
> [`workload.md`](workload.md) (the amortisation claim).

## 0. Why this document exists

Today projgit's serving daemon *is* the fetcher. A filesystem syscall
pulls data through the whole stack on demand:

```
open()/read() → daemon → on-demand Fetcher → upstream RTT → pack write → re-read → serve
```

That shape leans on git's **weakest** axis — per-OID on-demand
promisor fetch (one-at-a-time-ish, server recomputes per request, no
derived data). The
[`fetch-coalescing.md`](fetch-coalescing.md) investigation concluded
the cold-read gap (success criterion #3, ~3× `git cat-file` cold) is
*structural* on this axis: the dominant cost is server-side per-OID
work plus local pack writes, which batching does not address.

This document proposes inverting the data plane so that the network is
no longer in the syscall critical path **for cache hits**, and so that
cache *misses* are served through a consolidated tier that coalesces
and batches — leaning on git's **strongest** axis (bulk pack
transfer) instead.

## 1. The shift, in one line

Split the daemon's two jobs — *acquire* and *serve* — into two tiers,
and flip acquisition from **lazy-per-object pull** to
**eager-bulk-plus-derive push**:

- **`cached`** (cache + transform tier, one per host): owns the only
  upstream connection, fetches eagerly and in bulk, and **transforms**
  git's transfer-optimized format (packs, delta chains) into
  VFS-optimized derived structures.
- **VFS daemons** (many): near-stateless readers over the local store.
  They never hold an upstream connection and never reconstruct a delta
  chain on the hot path.

Data flow flips from **pull (mount-driven, lazy)** to **push
(cache-driven, eager)**.

## 2. Three separable ideas (and their verdicts)

The proposal bundles three ideas that do not share the same risk
profile. Naming them separately is the most important analytical move
in this doc.

| Idea | What it is | Verdict |
|---|---|---|
| **(A) Consolidation** | One tier owns the upstream relationship; daemons never talk to the remote | **Unconditional win** (also closes audit A3 / criterion #6 structurally) |
| **(B) Eagerness** | That tier fetches *ahead of* demand, not *on* demand | **Workload-dependent** — pays under the §1 overlap, wastes on the sparse-unique tail |
| **(C) Transform/prep** | Derive VFS-optimized structures locally (expanded trees, undeltified/path-indexed blobs, precomputed stat) | **Unconditional win** for serving speed; costs CPU + derived storage |

Most benefit comes from (A) and (C). (B) is the one that fights the
[`../problem-statement.md`](../problem-statement.md) §3 lazy-fetch
charter, and §6 below scopes it carefully with **tiered eagerness**.

## 3. Tier model

```
        ┌──────────────────────────────────────────────────────────┐
        │  Remote (stock git: partial-clone promisor / GVFS v1)     │
        └───────────────────────────┬──────────────────────────────┘
                                     │  bulk pack fetch (git's strong axis)
                                     ▼
        ┌──────────────────────────────────────────────────────────┐
        │  cached  — cache + transform tier (ONE per host)          │
        │  • owns the ONLY upstream connection                      │
        │  • coalesce + batch (single-flight intrinsic, not bolted) │
        │  • transform: packs/deltas → derived acceleration structs │
        └───────────────────────────┬──────────────────────────────┘
                                     │  write canonical odb + derived index
                                     ▼
        ┌──────────────────────────────────────────────────────────┐
        │  Shared store (on-disk):                                  │
        │   • canonical git odb  (gix-compatible, tooling-readable) │
        │   • derived index      (acceleration structures, OID-keyed)│
        └───────────┬───────────────────────────────┬──────────────┘
        direct mmap │ (HOT PATH — no IPC)            │
                    ▼                                ▼
        ┌────────────────────┐            ┌────────────────────┐
        │ VFS daemon (mount) │   ...      │ VFS daemon (mount) │
        └─────────┬──────────┘            └─────────┬──────────┘
                  │ miss: trigger fetch RPC         │
                  └──────────────► cached ◄─────────┘
```

Two invariants:

1. **The hot path is IPC-free.** Daemons read the shared store
   directly (mmap). `cached` is contacted only to *trigger* a fetch on
   miss, never per read.
2. **The canonical odb stays canonical.** The derived index is an
   *acceleration layer beside* the stock git odb, not a replacement —
   preserving the [`../problem-statement.md`](../problem-statement.md)
   §5 "tools read our store directly" and `.git`-synthesis wins.

## 4. The miss path (the crux)

"Network leaves the syscall critical path" is precisely the
**cache-hit** claim. On a true cold miss, *something* must block on the
network. The architecture decides *what blocks, where, and how
expensively*.

A `read(ino, off, size)` resolves `ino → OID` from the daemon's
(eager, local) tree index, checks the store, and misses. Options:

| Option | Behavior | Verdict |
|---|---|---|
| **Fail** (`EIO`/short read) | Agent infers file is broken/empty | ✗ breaks "appears complete" |
| **Block in the daemon** (daemon fetches itself) | Network in critical path; N daemons race; no batching | ✗ this is *today's* model |
| **Block through the tier** (daemon asks `cached`, which coalesces + batches) | Network in critical path, but one connection, deduped, bulk-capable | ✓ the design |

So the honest, narrower claim:

> Hits are network-free. Misses still block, but the miss is **(a)
> rarer** because trees are eagerly prepped and blobs are prefetched,
> and **(b) much cheaper per byte** because the consolidated tier
> coalesces concurrent demand and batches sibling fetches instead of
> doing isolated per-OID promisor round-trips.

Even the miss path is strictly better than today, because today's miss
*is* the worst case (isolated, racing, unbatched). On-demand fetch
remains the **correctness floor** (consistent with
[`prefetch.md`](prefetch.md)'s non-goal "prefetch is opportunistic;
on-demand remains the source of truth").

Two structural softeners fall out of the eager-prep tier:

- **Trees are eager ⇒ `readdir`/`stat` never miss.** Only *content*
  reads can touch the network. `os.walk` (criterion #2) is
  network-free after mount-time prep.
- **Hot-path reads bypass IPC.** The miss RPC only *triggers* a fetch;
  the daemon then re-reads the store directly.

## 5. Architecture A — Lazy-warm (the correctness floor)

Trees eager at mount; blobs fetched on miss *through* `cached`.

```
read(ino,off,size)
  └─ daemon resolves OID, looks up store
       ├─ HIT  → mmap bytes → serve            (no network)
       └─ MISS → cached.fetch(OID, hint=dir)
                   ├─ join in-flight fetch for OID (coalesce)
                   └─ else ONE batched pack request (siblings)
                 cached writes odb + derived → replies "ready"
                 daemon re-reads store → serve  (network blocked ONCE, shared)
```

- **Network blocks:** only on first read of a never-fetched blob, once
  per OID across all mounts.
- **Cost:** proportional to what's touched — keeps the lazy charter.
- **Win vs today:** miss is coalesced + batched + single-connection;
  the daemon-side `state.active` serialization (the 2026-06-18
  data-plane bottleneck) disappears because the daemon has no backend
  call.

A is the floor: even with zero prefetch it is correct and already
beats the current model on the miss path.

## 6. Architecture B — Prefetch-warm (the default)

A, plus `cached` runs predictive prefetch (manifest-driven,
readdir-batched, learned patterns from [`prefetch.md`](prefetch.md)).
By the time a daemon reads a blob, it is usually already local.

```
daemon readdir/read ──access signal──▶ cached.prefetch engine
                                          │ warm ahead of demand
                                          ▼
                                       fetch/coalesce/batch ──▶ store
daemon ─ mostly HIT ─▶ store
daemon ─ rare miss ─▶ cached  (A's path as fallback)
```

This is where "network leaves the critical path" becomes true **in
practice** for the §1 workload — prefetch converts would-be-misses
into hits *before* the syscall arrives, rather than only for repeat
reads.

### 6.1 Tiered eagerness (how B avoids the sparse-unique tax)

Full eager caching of a 140 GB commit when a mount touches 4 MB is
pure waste — the case lazy wins. B does **not** mean "eager
everything"; it means eager on the cheap, high-leverage data and
warm-lazy on the rest:

- **Trees / metadata: eager.** Small, and they make `os.walk` + `stat`
  instant without touching a blob (criterion #2, and enumerability —
  §3 property 2). Always worth it.
- **Blobs: warm-lazy, driven by prefetch hints.** First touch still
  fetches, but through `cached`'s bulk path, shared across mounts,
  never on the daemon's critical path. [`prefetch.md`](prefetch.md)
  becomes the *spec for `cached`'s eagerness policy*, not a sidecar.

Residual costs to name: **first-mount-of-a-cold-commit** pays ingest
latency (a risk against criterion #1 — mitigate by syncing only the
root tree synchronously and streaming the rest), and the transform
tier costs CPU + derived storage.

## 7. Architecture C — Full hydrate (demoted to a policy)

C = bulk-fetch the entire commit's reachable blobs once, transform,
then serve purely local. **It is the degenerate extreme of B** with
the prefetch policy set to *fetch everything reachable* — not a
separate architecture.

Its object acquisition is **identical to `git clone`**. What it saves
is everything *after* acquisition:

- **No checkout materialization.** Stock git follows clone with
  `git checkout`, an `O(files)` `write()` storm plus a *second*
  physical on-disk copy of every file (pack + expanded worktree). C
  fetches the same pack but never materializes a worktree; the VFS
  serves bytes from the odb/derived index. Effectively
  `clone --no-checkout` + a virtual worktree: one copy of the data,
  mount ~instant (criterion #1) instead of `O(files)`.
- **Multiplexing.** N containers on the same commit = one hydrated
  store + N cheap VFS views, vs N physical checkouts. `--reference`
  dedupes the odb for stock git, but you still pay N worktree
  materializations.

**C's only justification is saved materialization + sharing, never
fetch efficiency** (its fetch cost equals a clone). Therefore it is a
*policy `cached` escalates to* when it predicts high concurrent-mount
demand on one commit — not a default, and catastrophic for
sparse-unique access on a large repo.

## 8. Orthogonal mechanics (apply to A/B)

**How the daemon gets miss bytes:**

- **Trigger-and-reread** (default): daemon reads the store directly
  after `cached` signals "ready". Zero-copy hot path, simplest.
- **Stream-through** (optimization): `cached` returns bytes over the
  socket, optionally streaming ranges so a huge blob's first range
  serves before the whole object lands (cf. EdenFS's `CoverageSet`).

**Process boundary:**

- **Separate `cached` process** (recommended): real consolidation —
  one upstream connection per host, survives daemon crashes, daemons
  keep serving local data even if `cached` dies. Matches criterion #6
  structurally.
- **In-process library**: simpler, but loses cross-daemon
  consolidation unless daemons share state — re-deriving the
  per-mount-connection problem this design kills. Only sane if one
  daemon serves all mounts.

## 9. The transform / prep tier — two organs, not one

"Derived index" conflates two artifacts with different sizes,
structures, and justifications. Separating them is most of the design.

The starting point is what the VFS needs that a git **tree** entry
`(mode, name, OID)` does not carry. Four impedance mismatches between
git's object model and the FUSE callbacks:

1. **Trees carry no size and no mtime.** `stat` / `ls -la` need a size
   *per child*, but git stores size in the **blob**, not the tree.
   mtime does not exist per-file at all.
2. **Size for an unfetched blob is non-trivial.** On GVFS, `POST
   /gvfs/sizes` returns it without bytes; on stock git there is no
   equivalent, so size is learned only by fetching — and must then be
   remembered.
3. **Content-addressed, not location-addressed.** Git has no inode
   numbers and no parent pointers; the VFS namespace is synthesised.
4. **Blobs are transfer-optimised, not read-optimised.** Deltified +
   zlib-stream-compressed + whole-object ⇒ a partial read of a large
   file is O(whole file).

Mapped onto what is actually hard: **mtime** is trivial (the commit's
committer timestamp, constant per projection — already shipped),
**inode numbers** are per-mount synthesised state (already shipped via
`InodeAllocator`), which leaves exactly two derived artifacts worth
building.

### Organ 1 — the metadata memo (small, git-shaped)

The fields git's tree omits but `lookup` / `readdir` need: **`OID →
size`** first (the one needed-and-missing field), content-hash later
(for diff / status). Tiny, immutable, OID-keyed.

Its on-disk realisation is **git's own idiom, not a KV.** GVFS already
proved this: a cache server ships metadata as **packs of non-blobs**
(commit→tree closure via `/gvfs/prefetch` and `POST /gvfs/objects`
commit-expansion) plus the tiny `/gvfs/sizes` RPC — not a B-tree, not
RocksDB-for-metadata. So `cached`'s metadata prep output is a
**per-commit tree-closure pack** (the skeleton: a real `index-pack`-able
packfile, trees stored once in the shared CAS) plus a remembered
`OID → size` table. A transactional COW B-tree (redb / LMDB) is the
wrong *category*: the data is immutable, random-keyed, insert-only, and
rebuildable — which favours immutable sorted runs (git `.idx` / MIDX
shape), not in-place-mutable MVCC. For the MVP this organ may need *no
new file at all*: the shared odb + the in-process header cache already
answer presence and headers; persist only when a profile names the
cost (expect deltified-blob size resolution or content-hash demand, not
plain headers).

### Organ 2 — the decoded-content store (large, optional)

Flat, inflated, seekable blobs so `read()` at an offset is O(bytes
requested), not O(whole file) — the fix for impedance mismatch 4. This
is a *content transform*, not an index, and it earns its keep only for
**large files with partial / random access** (assets, databases; the
EdenFS `CoverageSet` case). Source files (<64 KiB) are already
flattened by the existing blob cache, so this is a tail optimisation,
deferred until a workload proves it.

It has a *second* justification beyond seekability (see §14):
materialising an inflated blob **once** in the shared CAS lets every
container's daemon mmap the *same decoded pages*, moving the
cross-container dedup boundary from "compressed pack" to "decoded
content" and sharing the inflate work itself.

The canonical git odb remains the source of truth for both organs; each
is a rebuildable cache.

## 10. Why this is cheap for projgit: read-only + content-addressed

The structural gift projgit has that EdenFS does not:

EdenFS pays for FSCK-on-every-boot and an invalidation state machine
*because its working copy is mutable*. projgit is **read-only** and git
objects are **immutable + content-addressed**, so anything the
transform tier derives is **keyed by OID and valid forever** — no
invalidation, no FSCK, no staleness machinery. The prep tier's outputs
are pure functions of immutable inputs. A derived structure is either
present (use it) or absent (rebuild it from the odb); it can never be
*stale*.

## 11. Relationship to existing designs

- **[`projgitd.md`](projgitd.md)** already specifies a per-host daemon
  owning the upstream connection + fetcher + shared cache, with
  per-container sidecars owning the mount fd. That *is* idea (A). This
  doc extends it with the **transform tier** (idea C) and reframes the
  default data plane as **eager/prefetch-warm** (idea B) rather than
  on-demand. The sidecar ≈ the "VFS daemon" here; the daemon ≈
  `cached`.
- **[`prefetch.md`](prefetch.md)** moves from "opportunistic
  optimization layered on on-demand" to "`cached`'s primary eagerness
  policy", while keeping on-demand as the correctness floor (§4).
- **[`prefetch-coalescing.md`](prefetch-coalescing.md) /
  [`fetch-coalescing.md`](fetch-coalescing.md)** single-flight becomes
  *intrinsic* to `cached` ("fetch each OID once") rather than a bolt-on
  across racing sidecars. The existing `PrefetchClaims` / `Coalescer`
  work is the ingest machinery this tier needs — it relocates, it is
  not wasted.

## 12. Mapping to success criteria

([`../problem-statement.md`](../problem-statement.md) §7.)

| # | Criterion | Effect of this model |
|---|-----------|----------------------|
| 1 | Mount < 100 ms | Helped by eager *trees only* at mount; full hydrate (C) hurts it — keep blobs streaming |
| 2 | `os.walk` every file, dir-bounded latency | Structural: eager trees ⇒ enumeration never hits the network |
| 3 | `cat` 1 RTT cold / < 1 ms warm | Cold gains an optimization path (pre-warm) it did not have; warm is local mmap |
| 6 | ≥ 100 mounts, one upstream connection | Structural: daemons hold no upstream socket; `cached` is the sole talker |

## 13. Comparison

| | A: Lazy-warm | B: Prefetch-warm | C: Full hydrate (policy) |
|---|---|---|---|
| Network in syscall path | misses only | rare tail | never (post-hydrate) |
| Disk / bandwidth | ∝ touched | ∝ touched + prefetch slack | ∝ commit size |
| First-mount latency | low | low | high (hydrate up front) |
| Best workload | sparse-unique | mixed (the realistic §1) | high-overlap, same commit |
| Reuses today's work | coalescer → tier | coalescer + prefetch | bulk fetch |
| Risk | miss tail still blocks | wasted prefetch | bandwidth blowup |

**Decision:** build **A as the floor**, ship **B as the default**,
keep **C as a per-commit policy** `cached` escalates to under observed
concurrent-mount demand.

## 14. Sharing model: cross-container and cross-commit

The §1 workload is *many containers, often different commits, on one
host*. What makes that cheap is reuse along two axes — both governed by
content-addressing for the *logical* part and by physical packing for
the *physical* part.

### Cross-container: the shared page cache

Containers are namespaces on **one host kernel**, so there is **one
page cache**, keyed by host **inode**. Bind-mount a single host CAS
**read-only** into each container (or keep the serving daemon on the
host), and every daemon reading the same packfile inode hits the
**same physical pages** — one copy in RAM for N containers, warm after
the first fault. This extends the [`workload.md`](workload.md) §1.6
amortisation from disk to RAM.

Two boundaries, kept distinct:

- **`cached` → serving daemon:** shared via the CAS page cache. The
  "notifications, not payloads" path — bulk bytes never cross the
  socket.
- **serving daemon → agent:** always FUSE (one kernel-mediated copy).
  By design: the agent is unmodified and must see a normal filesystem.
  Not zero-copy, and inherently so.

Correctness constraints for shared read-only mmap:

1. **One host CAS, bind-mounted `ro`** — not per-container copies
   (copies get their own inodes and share nothing).
2. **`cached` follows git's immutable-write discipline** — write pack
   to temp, fsync, atomic rename, publish `.idx`; **never mutate a
   published pack**. Readers only ever map complete, immutable files.
3. **Readers rescan the pack dir** to discover new packs; the
   "OID ready" notification triggers the re-read.
4. Content-addressed immutability ⇒ no cache-coherence problem.

**Isolation note:** prefer **sidecar-on-host** (only the FUSE
mountpoint enters the container) so raw git objects never enter a
possibly-hostile eval agent's namespace. Sidecar-in-container requires
mounting the CAS in, exposing raw objects. For Harbor (single-operator,
Scenario A in [`container-deployment.md`](container-deployment.md))
either works, but on-host serving is the cleaner default.

### Cross-commit: logical reuse vs physical reuse

Git history is highly redundant: changing one file bubbles new tree
OIDs only along that file's root-to-leaf path; every other tree and
blob keeps its OID. So **different commits share the overwhelming
majority of their objects** — commit B = commit A + a few changed blobs
+ O(depth × changed-paths) trees.

| Layer | Keyed by | Cross-commit reuse |
|---|---|---|
| CAS on disk | OID | near-total; commit B adds only the delta objects |
| Page cache | (inode, offset) | shared **iff** the object is physically singular |
| Metadata memo / decoded content | OID | automatic — identical regardless of commit |
| Per-mount inode namespace | per projection | not shared (and need not be) |

The catch: **content-addressing gives *logical* dedup; *physical* dedup
needs each object to live in one place.** Many independent per-commit
fetches create overlapping packs (the same OID in two packs) →
correctness is fine (git finds it either way) but disk and page cache
silently duplicate.

### The maintenance loop (the Scalar playbook)

The fix is background maintenance on the shared CAS —
**multi-pack-index (MIDX) + incremental repack + commit-graph** —
exactly what Scalar / GVFS ship as a first-class feature. MIDX gives
fast object lookup across many packs without merging them; incremental
repack consolidates so each object becomes physically singular (one
disk copy, one page-cache entry, shared across all commits referencing
it); commit-graph accelerates history queries. Git negotiation already
limits duplication (fetching commit B sends mostly *new* objects given
A), and GVFS prefetch packs are pre-segmented disjoint; stock git
approximates this via negotiation + local repack.

**Implication:** `cached` needs a **maintenance loop as a first-class
component**, not an afterthought. Skip it and you keep correctness but
bleed the dedup — N overlapping packs, N page-cache copies of shared
objects. This is the biggest operational lesson carried over from the
Scalar / GVFS lineage.

### Where the warm caches live

Because reuse is OID-keyed, the in-memory warm caches (decoded trees,
headers) should live in **`cached`** (one per host, cross-commit)
rather than per-sidecar (per-commit). A tree decoded for commit A is
then reused when commit B references the same OID — extending the
[`workload.md`](workload.md) §1.6 claim from "same commit across time"
to "different commits sharing objects."

The honest boundary: reuse tracks overlap. Nearby commits (the eval
case) overlap enormously; two unrelated branches across a giant
refactor overlap less and cost more. Optimise for high overlap; treat
low overlap as the acceptable tail.

### Prior art: GVFS cache servers

`cached` is a host-local **GVFS cache server**, and the GVFS v1
protocol encodes this design's core thesis. Its operations:

- `GET /gvfs/objects/{id}` — lazy single object (loose).
- `POST /gvfs/objects {objectIds, commitDepth}` — many objects as a
  pack; **a commit expands to all its trees recursively, no blobs**.
- `GET /gvfs/prefetch?lastPackTimestamp=T` — **pre-baked, timestamped,
  disjoint packs of non-blobs**, resumable by cursor.
- `POST /gvfs/sizes` — **size without bytes**.

Three lessons carry directly:

1. **Eager-metadata / lazy-blob is protocol-blessed.** `/gvfs/prefetch`
   is non-blob-only; even Microsoft never bulk-fetches blobs. That is
   §6.1 tiered eagerness, validated — and evidence against C as a
   default.
2. **Metadata travels as packs, not a KV** (§9). Commit→tree closure is
   a pack; size is a tiny RPC. The team that built the canonical cache
   server chose packs + `/gvfs/sizes`, not a B-tree.
3. **Capability asymmetry stock-git vs GVFS.** On GVFS, eager-tree
   hydration is **one RPC** (`POST /gvfs/objects` commit-expansion); on
   stock git there is no equivalent, so `cached` must **walk trees
   level-by-level** (directory-count × batched RTT). This is the
   [`../problem-statement.md`](../problem-statement.md) §4.5 "we don't
   own the server" tradeoff at the protocol layer, and it belongs in
   the `Fetcher` trait as an explicit capability (`GvfsFetcher` offers
   bulk commit-tree hydration; `GitCliFetcher` approximates by
   walking).

### Prior art: the Scalar playbook (what maps, what doesn't)

Scalar is Microsoft's "make *stock* git scale" package (now partly
`git maintenance` + the `scalar` command). It has two halves, and only
one maps:

- **Object-store half — adopt wholesale.** Partial clone + **MIDX +
  incremental repack + commit-graph + background `git maintenance`**.
  This *is* the maintenance loop above. We don't invent it; `cached`
  runs it (shell to `git maintenance` for the MVP; gix-native later).
- **Worktree half — replaced, not adopted.** Scalar bounds cost by
  making part of the worktree *real* via **sparse-checkout /
  sparse-index**; projgit rejects that (it breaks total enumerability —
  [`../problem-statement.md`](../problem-statement.md) §4.3) and
  replaces it with the **virtual filesystem**. **FSMonitor** is
  likewise N/A for a read-only virtual mount.

The one-line positioning: **projgit = Scalar's object-store
maintenance + a virtual filesystem *instead of* sparse-checkout** —
the same fork as [`../problem-statement.md`](../problem-statement.md)
§4.4/§4.5 (Microsoft kept maintenance, dropped the VFS; Meta kept the
VFS but owns the server; projgit keeps both, against stock git).

## 15. Non-goals & open questions

### Non-goals

- **Replacing the canonical git odb.** Derived structures sit beside
  it; tooling and `.git` synthesis keep reading stock git.
- **A write path.** Read-only here by design; the writable layer is a
  *separate, opt-in* tier on top of this one — its design space is
  [`writable-worktrees.md`](writable-worktrees.md) (Phase 2), which this
  tier's immutable derived baseline is built to support without being
  aware of it.
- **Cross-commit speculation in the tier.** Prefetch stays within a
  mount's commit OID (inherited from [`prefetch.md`](prefetch.md)).

### Resolved in session (2026-06-18)

1. **Wire protocol** — *reuse the projgitd `Fetcher`-shaped wire*
   ([`protocol.rs`](../../crates/projgit-daemon/src/protocol.rs):
   `Request::Fetch` / `PrefetchHeaders`), not a new protocol. Add one
   batched-blob `Fetch{oids}` variant for bulk blob-byte prefetch; keep
   one-shot trigger-and-reread; defer the locality hint and
   stream-through. **No gRPC:** two wires exist — `cached`↔remote (not
   our choice) and `cached`↔daemon (tiny control plane) — and bulk data
   *and* bulk metadata both travel via the shared CAS (mmap), never the
   socket. The wire carries *notifications, not payloads*, so transport
   choice is a non-issue.
2. **Derived-index format** — *git-shaped, not a KV* (§9): a per-commit
   tree-closure pack + an `OID → size` memo, immutable and OID-keyed,
   shared beside the CAS, single writer (`cached`), MVCC-free because
   insert-only. Persist only when profiled.

### Open questions

3. **Eagerness policy knobs** — what signals make `cached` escalate
   B→C, and how to bound prefetch slack on the sparse-unique tail.
4. **Stream-through vs trigger-reread** — measure whether streaming
   large-blob ranges is worth the IPC over zero-copy re-read.
5. **Maintenance cadence** — when `cached` runs MIDX / incremental
   repack / commit-graph (§14), and how to do it without disrupting
   in-flight mmap readers (immutable-write discipline makes this safe;
   the open part is *scheduling*).
6. **First-mount latency** — confirm "root tree sync, rest streamed"
   keeps criterion #1 under 100 ms on the target ~140 GB scale.

## 16. Status & next steps

Proposed, not committed. The Stage-style implementation plan now exists
in [`../implementation/cache-transform-tier-plan.md`](../implementation/cache-transform-tier-plan.md):
**Phase 1** builds this read-only tier (A→B, C as policy), starting at
**Stage 0** — pin open questions 1–2 (miss-trigger wire + derived-index
format) since every downstream interface depends on them. **Phase 2**
(writable mounts) is left open in
[`writable-worktrees.md`](writable-worktrees.md), and Phase 1 §8 of the
plan enumerates the seams that keep it additive.
