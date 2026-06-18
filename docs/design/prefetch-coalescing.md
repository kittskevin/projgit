# Design: Prefetch Coalescing

> Status 2026-06-18: **Implemented.** Shipped in `GitCliFetcher`
> (`PrefetchClaims` + the claim/skip path in `prefetch_headers`).
> Verified on the rust-lang/rust N=2 `--depth 1` diagnostic: one
> sidecar leads the 31-OID root-tree batch, the other skips it
> entirely (`cattrace: op=prefetch_coalesce ... lead=0 skipped=31`),
> issuing zero duplicate upstream fetches. Numbers in
> [`../bench/baseline.md`](../bench/baseline.md) §Diagnostic
> "Prefetch coalescing".
>
> Companion to [`prefetch.md`](prefetch.md) (the T1–T5 tier ladder,
> which deliberately defers the multi-agent dimension to here),
> [`fetch-coalescing.md`](fetch-coalescing.md) (the per-OID
> single-flight `Coalescer` this builds beside), and
> [`projgitd.md`](projgitd.md) (the daemon that hosts the shared
> fetcher).

## 1. The problem

[`prefetch.md`](prefetch.md) §4 describes T1: on every `readdir(D)`,
the provider posts that directory's file/symlink blob OIDs to a
background worker, which batches them into one
`git cat-file --batch-check` round trip and warms the header cache.
That doc scopes itself to a **single mount** and explicitly defers
cross-mount concerns.

In the daemon (sidecar) deployment, T1 runs **per sidecar**, but the
fetcher is **shared**. Each agent container has its own
`ProjectionFsProvider` + prefetch worker; all of them route through
one daemon-side `GitCliFetcher` over one shared CAS. When N agents
mount the same commit and each does the same first `ls`, each
sidecar's worker independently posts the same root-tree OID batch:

```
sidecar A readdir(/) → prefetch worker → PrefetchHeaders(31) ─┐
sidecar B readdir(/) → prefetch worker → PrefetchHeaders(31) ─┼─▶ daemon
                                                              │   GitCliFetcher
                                          (same 31 root OIDs) ─┘   ::prefetch_headers
```

After the 2026-06-18 handler-lock-release fix
([`../bench/baseline.md`](../bench/baseline.md) §Diagnostic,
"Post-pool measurements"), those two RPCs run **in parallel** at the
daemon. Both pre-passes miss on a cold cache, so both issue a full
31-OID `cat-file --batch-check` batch — **62 upstream promisor
fetches where 31 would do.** The post-fix trace still shows two
~22 s `PrefetchHeaders` batches side by side.

`GitCliFetcher` already has a per-OID single-flight
`Coalescer<ObjectId, ()>`, but it covers only `fetch_object`
(on-demand reads). `prefetch_headers` bypasses it entirely. That is
the gap this design closes.

## 2. Why it matters (and why it isn't urgent)

At N=2 the duplicate batches run concurrently, so **wall time barely
moves** — the script's reads still finish in ~1.7 s. The cost is
**wasted upstream work**: 2× promisor fetches, 2× pack writes, 2×
GitHub negotiation load. This scales with N and with batch size:

- At N=10–100 sidecars (the README's target shape), N duplicate
  31-OID batches saturate bandwidth / connection limits / the
  promisor endpoint, and the wasted work *does* start to cost wall
  time and disk.
- At the ~140 GB target-repo scale, duplicate lazy-fetch traffic is
  the difference between "one agent warms the CAS for all" and
  "every agent re-pays a slice of the clone."

So this is a **scalability / efficiency** fix, not a critical-path
latency fix — correctly a "smaller follow-up" to the cat-file pool,
not a peer of it.

## 3. The access pattern this targets

Prefetch is most useful for a **cold, metadata-heavy directory walk**
(`ls -la`, `stat`, `find` without content reads, IDE/language-server
file-tree enumeration, `git status`). See [`prefetch.md`](prefetch.md)
§1. The multi-agent version of that same pattern — **N agents each
`os.walk()` the same commit on a cold shared CAS** — is exactly what
generates the duplicate batches above. The scenario prefetch helps
most is therefore also the scenario that, at scale, most needs
coalescing.

Prefetch order is reactive: directories in the order the agent lists
them, and within each directory, git tree order of the file/symlink
entries (directories and gitlinks skipped), headers only, one level
deep, batched ≤ 64 OIDs per round trip. Coalescing operates on those
posted batches as they converge at the shared fetcher.

## 4. Design: non-blocking per-OID claim set

Add a `Mutex<HashSet<ObjectId>>` of "prefetch in flight" to
`GitCliFetcher`, **separate** from the `fetch_object` coalescer.
In `prefetch_headers(to_query)`:

1. **Claim.** Lock the set; for each missing OID, `set.insert(oid)`.
   Newly inserted OIDs are the caller's `lead`; already-present OIDs
   are `skipped` (a peer prefetch owns them). Unlock.
2. **Batch the lead only.** Run the existing `BatchChildPool` +
   `query_batch` path on `lead` instead of `to_query`.
3. **Release.** Remove all `lead` OIDs from the set. Use an RAII
   guard so a panic in the batch can't leak claims.
4. **Resolve skipped.** For each `skipped` OID, do a daemon-local
   `store.header(oid)` (microseconds): emit `PresentWithHeader` if a
   peer already landed it, else a plain `Present`.
5. Reassemble probes in input order via the existing
   `reorder_probes` helper.

Worked example (the measured case): A claims all 31 and holds the
claims for its batch; B — arriving at any time — finds them claimed,
skips all 31, and issues **zero** duplicate fetches. Total upstream
fetches: 31, not 62. This holds even for staggered arrivals: if B's
cold-cache pre-pass already excluded the OIDs A finished, B's
`to_query` is the remaining set, all of which A still holds claims
on, so B skips them too.

## 5. Two invariants the design protects

1. **No head-of-line regression.** Prefetch coalescing must never
   make an on-demand `Fetch` *wait* on an in-flight prefetch batch —
   that would undo the 2026-06-18 lock-release win. Two guards:
   the claim set is **separate** from the `fetch_object` coalescer
   (an on-demand read never consults it), and prefetch peers
   **skip** rather than **block** (no thread parks on a 22 s
   batch). A held claim slows nothing; it only suppresses a
   duplicate.
2. **Prefetch stays best-effort.** A `HeaderProbe` only warms the
   cache; the on-demand `lookup` path is the source of truth. So
   returning a weaker `Present` for a skipped, not-yet-local OID is
   safe: the sidecar's `store.header()` read-through simply misses
   and is ignored, and the kernel's later `lookup` fetches on
   demand. The peer that owns the claim warms the shared cache for
   everyone.

## 6. Alternatives considered

- **OID-set-hash batch coalescer** (key a `Coalescer` on a hash of
  the sorted OID set; followers join and clone the leader's result).
  Rejected for V1: only *identical* sets coalesce (fragile to
  staggered arrivals and partial overlap), it parks a daemon thread
  for the full ~22 s batch per follower, and `HeaderProbe` is
  deliberately non-`Clone` (the `Error` variant holds a non-`Clone`
  `FetcherError`), so it needs a cloneable intermediate. The claim
  set is simpler, handles partial overlap, and parks nothing.
- **Route prefetch through the existing `fetch_object` coalescer.**
  Rejected: a batch would hold per-OID claims for its whole
  duration, so an on-demand `Fetch` for any OID in the batch would
  block behind it — the exact head-of-line shape we just removed.
- **Coalesce at the `HydratingObjectStore` layer.** Rejected: that
  layer is generic over `F` and also wraps the sidecar's
  `DaemonFetcher`; deduping there would pointlessly wrap the RPC
  client. The duplication converges at the daemon's shared
  `GitCliFetcher`, so that is where the fix belongs.

## 7. Verification

- Extend the env-gated `PROJGIT_CATFILE_TRACE` line (added 2026-06-18)
  with `lead=N skipped=M`. Pre-fix: two `prefetch_headers` each
  `lead=31 skipped=0`. Post-fix: one `lead≈31`, the other
  `lead≈0 skipped≈31`, and only one full ~22 s batch in the trace.
- Re-run the rust-lang/rust N=2 `--depth 1` diagnostic from
  [`../bench/baseline.md`](../bench/baseline.md) §Diagnostic; confirm
  the duplicate batch is gone and disk total drops vs the post-pool
  capture.
- Unit-test the claim primitive deterministically (insert → lead,
  re-insert → skip, release → clears) plus a barrier-gated
  concurrency smoke test with bogus OIDs (which return `missing`
  fast, no network — same style as
  `batch_child_stays_alive_across_missing_queries`).

## 8. Non-goals

- **Cross-batch header memoization.** This dedupes *concurrent*
  overlapping batches; it does not cache probe results across time.
  The header LRU already handles the warm-cache case (a later
  `readdir` of the same directory pre-passes to a hit and issues no
  batch at all).
- **Partial-overlap merging into a single optimal batch.** The
  claim set suppresses duplicates; it does not re-pack two partially
  overlapping batches into one minimal query. Not worth the
  complexity for V1.
- **Anticipatory / recursive prefetch.** Still T2/T4 in
  [`prefetch.md`](prefetch.md); unaffected by this change.
