# Session handoff — 2026-06-19: cache+transform tier (Phase 1 read-only)

> Scope: this session took the **cache+transform tier** from a design
> conversation all the way to an implemented, tested, and benched
> read-only data plane. Design lives in
> [`../design/cache-transform-tier.md`](../design/cache-transform-tier.md);
> the stage plan in
> [`../implementation/cache-transform-tier-plan.md`](../implementation/cache-transform-tier-plan.md);
> a code-grounded "as-built" reference in
> [`../implementation/how-it-works.md`](../implementation/how-it-works.md).
> This doc is the running narrative so a resume doesn't re-derive it.
>
> **All 17 commits are on `main` but NOT pushed (branch is `ahead 17`).**

## Session arc

1. **Research + design.** Studied EdenFS (Overview + 14 design docs) and
   the GVFS protocol / Scalar playbook; compared against projgit's
   stock-git posture. This produced a model shift — split object
   *acquire* from *serve* into a per-host `cached` tier that eagerly
   fetches + transforms git data, leaving VFS daemons as local readers —
   captured in `cache-transform-tier.md`.
2. **Built Phase 1 (read-only) mechanisms** across Stages 0/1/3/5
   (Stage 2 deferred; Stage 4 not built — see below).
3. **Filled test gaps** (the two new daemon wire paths) and fixed a bug
   the tests surfaced.
4. **Productionized** the maintenance gate as a `projgitd` flag.
5. **Documented** the as-built system and **benched** it against a real
   remote (`rust-lang/log`).

## Commits (chronological, all on `main`, unpushed)

| commit | what |
|---|---|
| `1ff31ec` | docs(design): cache+transform tier model, plan, writable seam |
| `71d7113` | feat(core): `Fetcher::fetch_objects` batched bulk-resident primitive |
| `9359607` | feat(daemon): `FetchMany` wire for bulk blob-byte prefetch |
| `713e24c` | feat(core): `warm_tree_closure` eager tree-skeleton warm |
| `c504216` | feat(daemon): eager-tree warm in the background after mount |
| `14a57f3` | feat(cli): eager-tree warm on standalone + sidecar mounts |
| `ebb324a` | docs(plan): Stage 0 decided + Stage 1 implemented |
| `79590de` | feat(core): blob-byte prefetch on readdir (Architecture B) |
| `9f0966e` | feat(cli): surface blob prefetch counters in `--stats` |
| `cb3a6ff` | docs(plan): Stage 3 mechanism shipped |
| `e9e5a86` | feat(core): `maintenance::run_maintenance` for CAS upkeep |
| `7d7d187` | feat(daemon): background CAS maintenance loop |
| `0eb81d5` | docs(plan): Stage 5 shipped; Phase 1 mechanisms complete |
| `500d6aa` | fix(core): skip already-resident objects in `fetch_objects` |
| `356dafd` | test(daemon): FetchMany handler + DaemonFetcher batch client |
| `69de737` | feat(daemon): `--maintenance-interval-secs` flag |
| `217a877` | docs(impl): how-it-works (as-built) doc |

## What got built (by stage)

- **Stage 0 (decisions).** Wire = reuse the projgitd `Fetcher` RPC + a
  new batched-blob `FetchMany`; reply via `HeaderProbes`, **bytes via the
  shared CAS, never the socket** ("notifications, not payloads"). Format
  = git-shaped (packs/`.idx`), persistence deferred.
- **Stage 1 (Architecture A — eager trees).** `Fetcher::fetch_objects`
  (batch, skips already-resident); `HydratingObjectStore::warm_tree_closure`
  (BFS tree closure, trees only); background warm at mount on **all three
  paths** (daemon `handle_mount`, standalone, sidecar via `DaemonFetcher`
  → `FetchMany`).
- **Stage 3 (Architecture B — blob prefetch).** A `Blobs` prefetch task
  bulk-warms a directory's small-file blob *bytes*; gated by
  `PROJGIT_PREFETCH_BLOBS` + a size cap (`PROJGIT_PREFETCH_BLOB_CAP_BYTES`,
  1 MiB); the FIFO worker processes a dir's `Headers` task first so the
  cap reads warm sizes.
- **Stage 5 (maintenance).** `run_maintenance` shells
  `git maintenance run --task=incremental-repack --task=commit-graph`;
  the daemon runs it from a background thread gated by
  `--maintenance-interval-secs` / `PROJGIT_MAINTENANCE_INTERVAL_SECS`,
  off the serving path, joined on shutdown.

## Bench results (vs stock git, `rust-lang/log`, real network)

| Win | Result |
|---|---|
| Blob-free `readdir` vs git | **~16×** (0.43 ms vs 7.1 ms) |
| Eager-tree warm (`os.walk` local) | instant (`ls -R` ~4 ms, trees all warm) |
| Blob prefetch, `readdir → gap → read` | **~200×** (0.004 s warm vs 0.808 s cold) |
| Daemon multiplexing (4 sidecars, 1 CAS) | **1.98× wall, 3.95× disk** vs 4 independent clones |
| Cross-commit maintenance | MIDX + commit-graph written; see honest finding below |

## Honest findings the bench earned (read before trusting the design docs)

1. **Blob prefetch is gap-dependent.** It delivers ~200× *only when the
   workload has a readdir→think→read gap* (the agent pattern). In a tight
   walk→read microbench it shows **no change** — the background prefetch
   can't get ahead, and on-demand doesn't block on it by design.
2. **On the GitCli backend, blob prefetch is partly redundant.** The
   always-on T1 *header* prefetch uses `cat-file --batch-check`, which in
   a partial clone faults the **full** object in — so a header batch
   doubles as a bytes batch. Stage 3's distinct value is the **GVFS**
   backend (`/gvfs/sizes` = sizes without bytes).
3. **Cross-commit "physical dedup" is mostly a misnomer.** git
   *negotiation* already fetches each object once, so a multi-commit CAS
   ≈ the union of objects (little raw duplication to recover). The real
   maintenance win is **MIDX (O(log total) lookup across accumulating
   packs) + commit-graph**, not disk dedup. A full `repack -a -d` shrinks
   ~18% via **delta recompression** (not dedup), and projgit deliberately
   uses *incremental* repack to avoid rewriting a big base pack.
   → **The design §14 wording ("physical dedup") should be softened to
   "MIDX-accelerated lookup across accumulating packs."** (not yet done)

## Gotchas / lessons

- **Stale release binary.** `target/release/projgit` and the
  `bench_mount` example build **separately**. A stale `projgit` binary
  silently ran old code (its `--stats` lacked `blobs_warmed`, the tell).
  Always `cargo build --release -p projgit-cli` before a manual mount
  bench.
- **FUSE mount in this container** works (`/dev/fuse` present); the
  foreground mount needs **SIGINT** (not `fusermount -u`) to print
  `--stats` and exit. `pkill -INT -f "projgit mount <url>"`.
- **Bench is network-gated** by `PROJGIT_NETWORK_TESTS=1`.

## Config surface added this session (all default OFF)

- `PROJGIT_PREFETCH_BLOBS`, `PROJGIT_PREFETCH_BLOB_CAP_BYTES` (env).
- `projgitd --maintenance-interval-secs N` (flag) /
  `PROJGIT_MAINTENANCE_INTERVAL_SECS` (env fallback).

## What's NOT done / next-up queue

1. **Push** — branch is `ahead 17`, unpushed.
2. **Fix design §14 wording** per honest finding #3 (dedup → MIDX lookup).
3. **Record the bench session** in
   [`../bench/baseline.md`](../bench/baseline.md) (numbers above).
4. **Stage 2 (persisted derived metadata)** — deferred; odb + in-proc
   header cache cover the MVP per design §9.
5. **Stage 4 (Architecture C, full hydrate)** — not built; niche
   (high-coverage access, opposite of §1's sparse pattern).
6. **Blob-prefetch CLI flag** — env-gated only; promotion deferred
   (4-file `PrefetchPolicy` refactor for marginal ergonomics over a
   working env gate).
7. **Phase 2 (writable worktrees)** — gated on
   [`../design/writable-worktrees.md`](../design/writable-worktrees.md)
   §10.1 no-fork spike. The Phase-1 seams to preserve are in plan §8.

## State at handoff

- `cargo clippy --workspace --all-targets -- -D warnings` clean; all
  tests green (core unit + `warm_tree` / `maintenance` / prefetch
  integration; daemon `FetchMany` + `DaemonFetcher` smokes).
- Phase 1 read-only **mechanisms are complete**; remaining work is
  validation polish (bench recording, §14 wording) and direction choices
  (Stage 4 / Phase 2 / flag promotion).
