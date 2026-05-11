# Design: Prefetch

> Status: **T1 implemented; T2+ design**. Companion to
> [../problem-statement.md](../problem-statement.md) (the agent-eval
> use case driving this) and to the existing tree+blob LRUs
> documented in `crates/projgit-core/src/{tree_cache,blob_cache}.rs`.

## 1. Problem

The Phase 5a measurements on `rust-lang/log` quantified the cold-fetch
problem precisely:

```
cold ls -la src/   (5 files, 5 sequential HTTPS RTTs)   ~1.5 s
warm ls -la src/                                         ~2 ms
```

Cold latency is dominated by upstream round trips, not by anything
projgit does. For the agent-eval use case
([../problem-statement.md](../problem-statement.md)), this is the
single biggest perceptual issue: an agent's first `os.walk()` of a
new mount waits on `O(directories) × upstream_RTT`.

We can mostly hide this by **fetching things before the kernel asks
for them**. Done well, the agent never blocks on the network for
anything it was going to access soon. Done badly, we DDoS the
upstream and waste bandwidth on bytes nobody touches.

This doc lays out *what* to prefetch, *when*, and *who decides*. Tier 1
is implemented in `projgit-core`; later tiers remain design notes.

## 2. Goals & non-goals

### Goals

- **Hide upstream RTT for the access patterns agents actually
  exhibit.** Bound the perceived latency by something other than
  network RTT × file count.
- **Bounded resource use.** Prefetch must respect a configurable
  in-flight budget so it can't DoS the upstream or pin unbounded
  memory.
- **Backwards compatibility.** Adding prefetch must not change the
  public `FsProvider` trait in ways that break the `projgit-fuse` /
  `projgit-winfsp` adapters. Internal hooks are fine.
- **Observable.** Every tier must surface enough counters via
  `--stats` (or equivalent) that we can tell whether the prefetch
  is doing anything useful.
- **No new crate dependencies.** Background work uses
  `std::thread` + a small bounded queue; we already have all the
  primitives we need. (We can revisit Tokio/async later if
  justification appears.)

### Non-goals

- **Predicting future commits.** Prefetch operates within a single
  mount's commit OID; no cross-commit speculation.
- **Replacing on-demand fetch.** Prefetch is opportunistic. The
  on-demand `Fetcher` path always remains the source of truth for
  correctness.
- **Network-level optimization.** HTTP/2 multiplexing, parallel
  TCP, etc. are upstream concerns. We assume the existing
  long-lived `git cat-file --batch-check` child gives us an
  amortised connection; we focus on *what to ask for*, not *how to
  send the bytes*.
- **Cross-mount learning** (this lives in a future `projgitd`
  design, not here).
- **Prefetching commit graphs / refs / tags.** All projgit
  projections are pinned to a single commit OID at construction;
  graph walks are out of scope until we add features that need them.

## 3. Tier ladder

Five distinct classes of prefetch, in increasing engineering cost.
Each is independently shippable.

| Tier | Trigger | What it fetches | Cost (rough) | Win |
|---|---|---|---|---|
| **T1** | `readdir(D)` returns | Headers (size+kind) for every entry of `D` | 1 day | Cold `ls -la` becomes ~1 RTT instead of N |
| **T2** | Subtree-traversal heuristic fires | Trees + headers under the active subtree, recursively, depth-bounded | 3–5 days | Agent perceives "directory I'm in is warm" |
| **T3** | Mount time, from CLI manifest | Trees + headers + optionally blobs, for given paths | 0.5 day | Deterministic eval startup |
| **T4** | Learned access patterns | Speculatively fetches "blob accessed after blob X" | 1–2 weeks | Cache feels prescient |
| **T5** | Mount time, full hydration | All blobs reachable from the commit | 0.25 day | "Effectively a full clone, faster" — appropriate for small repos only |

The recommended build order — **T1, T3, T2, T5, T4** — is *not* in
ascending cost order. It's in ascending complexity-vs-justification
order: T1 and T3 give us almost-everything-we-need with very little
risk; T2 and T4 require more code and more thought about pathological
behaviour.

## 4. Tier 1 — `readdir` header batching

### Design

Today's path:

```
kernel → readdir(D)        ← projgit returns names+kinds (cheap, no fetch)
   ↓
kernel → lookup(D, name)   ← projgit calls header(blob_oid) (one RTT)
kernel → lookup(D, name)   ← projgit calls header(blob_oid) (one RTT)
   ↓                          ... N times for N entries
```

T1 path:

```
kernel → readdir(D)        ← projgit returns names+kinds AND queues
                              header() prefetch for every blob OID
                              in D
   ↓ (background)
   prefetch worker → batches K OIDs, sends one cat-file --batch-check
                     request, populates the header cache for all K
   ↓
kernel → lookup(D, name)   ← projgit reads header from cache (no RTT)
```

### Implementation outline

- Introduce `HeaderCache` in `projgit-core` mirroring
  `TreeCache`: `OID → (kind, size)`, bounded LRU, stats. Today
  `ObjectStore::header()` calls gix; T1 introduces a cache layer in
  front. (Cheap; small.)
- Add `HydratingObjectStore::prefetch_headers(&[ObjectId])` that
  the prefetch worker calls. It:
  1. Skips OIDs already in the header cache.
  2. Skips OIDs known to be locally present (gix `try_find_header`
     succeeds without network).
  3. Hands the remaining list to the fetcher's batched query.
- Extend `Fetcher` with one optional method:
  ```rust
  fn prefetch_headers(&self, oids: &[ObjectId]) -> Vec<HeaderProbe>;
  ```
  Default impl: call `fetch_object` per OID. `GitCliFetcher`
  overrides: send all OIDs to the batch-check child in one go,
  read back N status lines.
- `HeaderProbe::HeaderOnly` lets transports such as GVFS publish
  trusted metadata from a server sizes endpoint without pretending
  the object bytes were hydrated locally.
- In `ProjectionFsProvider::readdir`, after returning entries to
  the caller, post the batch of regular-file, executable-file, and
  symlink blob OIDs (skip directories and gitlinks) to a
  per-provider prefetch worker.

### What changes

- New module `crates/projgit-core/src/header_cache.rs` (mirror of
  `tree_cache.rs`).
- New optional trait method on `Fetcher`. Default impl preserves
  back-compat for `NoopFetcher` and `GixFetcher`.
- `GitCliFetcher` adds a multi-OID query path (the protocol already
  supports it; we just stop limiting ourselves to one OID per
  query).
- `ProjectionFsProvider` owns a `PrefetchHandle` and posts to it after
  `readdir`. The worker thread is spawned at construction and joined on
  drop.

### Bounded resource use

- One worker thread per provider; not per directory. The worker
  drains its mpsc channel, batches up to `MAX_BATCH` OIDs (say 64),
  sends one query.
- In-flight budget: the channel itself is bounded (capacity ~256);
  if the producer (readdir) outruns the worker, posts are dropped
  silently (the next on-demand `lookup` will fetch correctly).
- No retry on the prefetch path — failures are logged, not retried;
  on-demand will retry naturally.

### Stats added

- `header_cache.{hits, misses, inserts, evictions, len}` (mirrors
  `tree_cache_stats`).
- `prefetch.{posted, dropped, batches_sent, oids_resolved,
  headers_published, oids_failed}`.

### Tests

- Unit: `header_cache` LRU semantics (mirror of `tree_cache`'s
  unit tests).
- Unit: `Fetcher::prefetch_headers` default impl works on
  `NoopFetcher` (returns `NotHydratable` per OID without panicking).
- Integration: against the local fixture, `readdir` followed by N
  `lookup`s should incur exactly **one** `header()` round trip in
  total (currently N). Assertable via the new prefetch counters.
- The existing `mount_smoke` test passes unchanged; T1 is purely
  internal.

### Deferred

- **Speculative blob hydration** during `readdir`. T1 fetches only
  *headers*, not blob bytes. Blob bytes are still fetched on first
  `read`. This is deliberate — many `os.walk()`-shaped tools never
  read most files, and blob bytes are 100×–10000× larger than
  headers.

## 5. Tier 3 — Mount-time manifest prefetch

(Tier 2 deliberately moved later; see ordering note in §3.)

### Design

For agent evals you typically know in advance which paths matter:
"this eval is about the auth subsystem; we'll touch `src/auth/`,
`tests/auth/`, `docs/auth/`." Adding a flag that warms those
subtrees before the agent starts is essentially free engineering
and removes the cold-fetch tax entirely for those paths.

```
projgit mount … /workspace --prefetch src/auth --prefetch tests/auth
projgit mount … /workspace --prefetch-manifest paths.txt
projgit mount … /workspace --prefetch-blobs src/auth   # also bytes
```

### Implementation outline

- New CLI flags on `projgit mount`:
  - `--prefetch <PATH>` (repeatable): walk this subtree at mount
    time, queue trees + headers for prefetch. Bytes not fetched by
    default.
  - `--prefetch-manifest <FILE>`: same but read the list from a
    file (one path per line, `#` comments OK).
  - `--prefetch-blobs <PATH>` (repeatable): also fetch blob bytes
    under this subtree.
- After `ProjectionFsProvider` is constructed but before
  `mount_background`, the CLI walks each prefetch path via
  `Projection::lookup` and posts the OIDs to the same prefetch
  worker T1 introduced.
- Mount returns immediately; prefetch happens in the background.
  Agent observation: by the time it gets to `os.walk('src/auth')`,
  the cache is warm.

### What changes

- CLI flags only; no `projgit-core` changes if T1 has landed (we're
  using the prefetch worker T1 created).
- A small `prefetch_subtree(provider, path, fetch_blobs)` helper
  in the CLI that does the recursive walk.

### Bounded resource use

- Same worker, same channel bounds as T1. The CLI is just another
  producer.
- For `--prefetch-blobs` (which can move serious bytes), the worker
  needs a separate concurrency limit on blob fetches (say 4 in
  flight at once) so we don't saturate the upstream.

### Stats added

- `prefetch.{manifest_paths, manifest_subtrees_walked,
  manifest_blobs_fetched}`.

### Tests

- Integration: against a fixture, `--prefetch src` makes subsequent
  `read_tree(src_oid)` resolve from the LRU.
- CLI: argument-parsing unit tests for the flag combinations.
- The smoke test verifies it doesn't crash with a manifest that
  references a path that doesn't exist (warn, skip, continue).

### Deferred

- **Wildcards / globs in manifest paths.** Start with literal paths.
  Globbing can land later if needed.
- **Manifest format with priorities** (warm A before B). Linear
  order in the file is implicitly the priority.

## 6. Tier 2 — Subtree-traversal heuristic

### Design

After T1 + T3 are live, the remaining cold-fetch cost comes from
subtrees the agent enters that nobody anticipated. T2 watches the
access pattern and speculatively warms a subtree once we see the
agent committing to it.

The heuristic, kept deliberately simple:

> When `lookup` for a subtree's tree OID happens **and** at least
> one descendant blob is also requested within `T_window`,
> mark the subtree as "active" and queue a depth-limited recursive
> prefetch.

Defaults: `T_window = 1 second`, `max_depth = 3`. Both tunable.

### Implementation outline

- New `AccessTracker` inside `ProjectionFsProvider`: per-subtree
  recency + child-access count.
- `lookup` and `read` notify the tracker.
- Tracker promotes a subtree to "active" when the threshold trips
  and posts a recursive prefetch task to the worker.
- Recursive prefetch reads each tree, then queues its blob OIDs
  for header batching (T1 path).

### What changes

- New `access_tracker.rs` module in `projgit-core`.
- `ProjectionFsProvider::lookup` / `read` gain tracker calls.
  Internal only; no trait change.
- The prefetch worker handles a new task variant:
  `RecursiveSubtree { tree_oid, depth_remaining }`.

### Bounded resource use

- The recursive prefetch must respect a *separate* outstanding-task
  budget from T1 (say 16 active subtree walks at once); otherwise a
  pathological `find /workspace` would queue thousands of subtree
  walks.
- Depth limit prevents one walk from blowing the budget.

### Stats

- `prefetch.{subtrees_promoted, subtrees_walked, depth_capped}`.

### Tests

- Unit: `AccessTracker` promotion logic with synthetic clocks.
- Integration: against a fixture, simulate the access pattern
  (lookup tree X, read child Y) and assert the rest of X is warmed.
- Integration: pathological case — many subtrees touched once,
  none promoted.

### Why this is later than T3

T3 covers the "you knew up front" case with deterministic behaviour
and zero risk of misbehaving. T2's heuristic could in principle
prefetch a lot of stuff the agent never touches; that's not great
on bandwidth-constrained networks. Build T2 only after T1+T3 are in
production and we've measured what real workloads need.

## 7. Tier 5 — Full hydration at mount

### Design

For small enough repos (say <1 GiB pack data), just fetch
everything reachable from the commit at mount time. The agent then
operates against an effectively-local checkout with zero on-demand
fetches.

```
projgit mount … /workspace --full-hydrate
```

Implementation: shell out to `git -C <store> fetch origin
<commit_oid> --no-deepen` with no filter, then proceed normally.
This is a one-liner in the CLI plus a sanity check that the repo
is small enough to make this sensible.

### When to use it

- Eval pipelines that prefer "predictable, no-network at runtime"
  over bandwidth efficiency.
- Repos under ~1 GiB.
- Networks where upstream RTT is highly variable.

### Why it's listed at all

It's nominally "the boring answer" but it's also genuinely a valid
operational mode for small repos. Better to have it as a `--flag`
than to have users reach for `git clone` separately.

### Tests

- CLI test that the flag parses.
- Optional integration test (network-gated) that exercises it
  against a small public repo.

## 8. Tier 4 — Learned access patterns

### Design

Over many runs against the same store, projgit could record a graph
of "blob A was accessed shortly after blob B" and use it to
speculatively fetch B-followers when A is touched.

Two natural approaches:

- **Markov chain on path components.** Cheap to build, captures
  "after `lookup(src/foo)`, agents often `lookup(src/foo/bar)`."
  Per-mount or per-store state.
- **Sequence frequent-pattern miner.** Captures longer dependency
  chains. More state, more code.

### Why this is the last tier

Three reasons:

1. T1+T2+T3 will probably cover 90% of the perceptual win for the
   first generation of agent workloads. Don't pre-optimise.
2. Persistence story is non-trivial. Pattern data has to live
   somewhere; if it's per-store on disk, we're growing
   `.git/projgit/access.db`-style state. If it's per-host, that's
   `projgitd`'s problem (and `projgitd` doesn't exist yet).
3. The win is most pronounced when the daemon exists and serves
   many consumers. Pre-`projgitd`, learned patterns help one mount
   at a time, which is the worst case.

Ship `projgitd` first; add T4 inside it if profiles say it's
worth the storage and complexity.

### Stats / safeguards (sketch only)

- Hard cap on the size of the learned-pattern store.
- Clear cache when the user rotates commits enough that the
  patterns are stale.
- Disable-by-default flag; opt in per mount or per daemon.

## 9. Cross-cutting: the prefetch worker

A single mechanism shared across T1–T5.

### Shape

```
mpsc::channel<PrefetchTask>     bounded, capacity 256
        │
        ▼
worker thread (one per provider, joined on Drop):
   loop {
     batch up to MAX_BATCH tasks of compatible kind
     execute via Fetcher trait methods (header batch / blob fetch)
     update caches; bump stats
   }
```

### Task variants

```rust
enum PrefetchTask {
    Headers(Vec<ObjectId>),              // T1, T3 (path walks)
    Blob(ObjectId),                      // T3 with --prefetch-blobs
    SubtreeRecursive { tree: ObjectId, depth_remaining: u8 },  // T2
}
```

### Backpressure

- Channel is bounded; producers `try_send` and drop on full. The
  on-demand path always works regardless.
- Per-kind concurrency limits inside the worker (e.g. blob fetches
  at most `MAX_INFLIGHT_BLOBS` at once).

### Cancellation

- Worker observes a single `Arc<AtomicBool> running` flag.
- Set to `false` on provider drop / mount unmount; worker exits
  ASAP after finishing the current batch.

### Why one worker, not a pool

Today's `GitCliFetcher` uses one `git cat-file --batch-check` child;
parallel queries against it would interleave incorrectly. A future
multi-child or daemon-backed fetcher could justify a worker pool;
single-worker is right for the current design.

## 10. Open questions

These need decisions before code lands but aren't blocking the
design.

- **Does the kernel re-`lookup` a recently-`readdir`'d entry?**
  If yes (most common case), T1 is a clear win. If the kernel
  caches dent attributes from `readdirplus`-style flows we don't
  use, the win is smaller. Worth measuring with a `strace -ttT` of
  the smoke test before/after T1.
- **What's the right `MAX_BATCH` for the cat-file child?** Need a
  micro-benchmark across batch sizes 1, 8, 32, 128. Likely 32–64
  is the sweet spot; the protocol is fine with arbitrary numbers
  but git may chunk responses internally.
- **Per-mount vs per-store budgets.** Today caches are per-mount
  (per `ObjectStore`). When `projgitd` lands, the budgets should
  move with it. The trait signatures should accommodate "this
  mount is sharing a worker" without breaking the standalone case.
- **Persistence of the header cache.** Tree LRU and blob LRU are
  in-memory only today. Header cache could plausibly be persisted
  on disk (it's small — `(20-byte OID, kind, size)` triples) so
  cold mounts skip the first round of fetches. Probably defer; not
  on the critical path.

## 11. What this isn't

- **A latency target.** The success criteria in
  [../problem-statement.md §7](../problem-statement.md) cover
  end-to-end goals; this doc is about *one mechanism* contributing
  to them.
- **A daemon design.** The prefetch worker is provider-local; it's
  designed to slot into a future `projgitd` cleanly but doesn't
  require it.
- **A FUSE / WinFsp thing.** Prefetch is entirely above the
  `FsProvider` trait. Both backends benefit equally with no
  per-backend work.
- **Cross-commit prefetch.** Mount = pinned commit. Future work
  on commit-graph or branch-tip refresh is a different design.

## 12. Recommended sequence

1. **T1** — header batching at `readdir`. Land first; everything
   else stacks on top of the worker it introduces. Highest-leverage
   per LOC of any item in this doc.
2. **T3** — manifest prefetch at mount. Same worker; tiny CLI
   delta; deterministic eval startup. Trivial after T1.
3. **Measure.** Profile real agent workloads against a non-trivial
   monorepo. Decide whether T2 is worth the heuristic risk before
   building it.
4. **T2** — subtree-traversal heuristic, *if* T1+T3 don't cover the
   workload. Build with a hard kill-switch; this is the tier most
   likely to misbehave in unexpected ways.
5. **T5** — `--full-hydrate` flag. One-line CLI add; ship whenever
   convenient.
6. **T4** — learned patterns, *only if* `projgitd` is built and
   profiles show this is the bottleneck. Most likely never.

The total of T1+T3 is roughly 1.5 days of focused work and gives
the agent-eval use case a perceptually-warm filesystem on first
walk. Everything beyond is incremental refinement.
