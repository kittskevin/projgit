# Cat-file pool — implementation plan

> Status: **living doc.** Tracks how the cat-file pool fix gets
> built. Updated as each stage lands or surfaces something that
> changes downstream stages.
>
> Last updated: 2026-06-18 (Stages 1-3 implemented; Stage 4 docs
> wrap-up in progress).
>
> Motivated by the 2026-06-04 data-plane diagnostic: the
> `rust-lang/rust` `sparse-shared` N=2 cell takes ~16 s, of
> which ~15 s is on-demand `Fetch` RPCs head-of-line blocked
> behind in-flight `PrefetchHeaders` batches all serialising
> through one `Mutex<BatchChild>` in `GitCliFetcher`. Full
> trace in [`../bench/baseline.md`](../bench/baseline.md)
> §Diagnostic. This is the highest-leverage perf change on the
> queue.

## 0. Why this doc exists

Mirrors the prior plan-doc pattern (Phase C / sparse-access /
worktree-comparator / data-plane investigation): design = what
+ why; plan = how + what to learn from each step before the
next.

The cat-file pool was speculative through 2026-06-02. The
2026-06-04 trace promoted it from "solution looking for a
problem" to "diagnosed fix with measured signal". This plan
captures the implementation.

## 1. Pre-flight

Before writing code:

1. Re-read the 2026-06-04 Diagnostic section in
   [`../bench/baseline.md`](../bench/baseline.md). The
   8-line trace + "What this shows" prose are the spec for
   what the pool must address.
2. Note the current single-child plumbing in
   [`../../crates/projgit-core/src/fetcher/git_cli.rs`](../../crates/projgit-core/src/fetcher/git_cli.rs):
   - `pub struct GitCliFetcher { batch: Mutex<Option<BatchChild>>, ... }` (line 184ish).
   - `raw_fetch(&self, oid)` locks `batch`, lazy-spawns the
     child, calls `query(oid)`, releases.
   - `prefetch_headers(&self, oids)` locks `batch`, lazy-spawns,
     calls `query_batch(&to_query)`, releases.
3. Note the existing `Coalescer<ObjectId, ()>` on `GitCliFetcher`:
   already covers `raw_fetch` (per-OID single-flight); does
   **not** cover `prefetch_headers` (the trace shows two
   PrefetchHeaders(31) calls with overlapping OID sets each
   got a full mutex turn). Per-batch coalescing is the
   *separate* follow-up item on the handoff queue; keep this
   plan focused on the pool.
4. Note that `BatchChild` is per-`git-dir` (spawned via
   `BatchChild::spawn(&git_dir)`). The pool's K children all
   talk to the same git-dir, so they share the same on-disk
   pack files. No coordination needed at the pack level.

## 2. Stage 1 — `BatchChildPool` data structure

### 2.1 Goal

A pool primitive that hands out a `BatchChild` to a caller for
a single use, then puts it back. Round-robin try-lock with
blocking fallback. Lazy spawn per slot. Self-contained inside
`git_cli.rs`; the pool's API mirrors `Mutex<Option<BatchChild>>`'s
relevant operations enough that `raw_fetch` and `prefetch_headers`
can swap their lock site for `pool.acquire()`.

### 2.2 Concrete change

In `crates/projgit-core/src/fetcher/git_cli.rs`:

```rust
struct BatchChildPool {
    /// One Mutex<Option<BatchChild>> per pool slot. Always
    /// `Some(_)` between spawn and tear-down; `None` while a
    /// child is being respawned after I/O failure.
    slots: Vec<Mutex<Option<BatchChild>>>,
    /// Round-robin starting index for `acquire`. AtomicUsize
    /// because we don't want callers contending on a counter
    /// mutex.
    next: AtomicUsize,
}

impl BatchChildPool {
    fn new(k: usize) -> Self { /* k slots, all None initially */ }

    /// Try each slot once round-robin starting from `next`;
    /// return the first one we can lock. If all slots are
    /// busy, fall back to a blocking lock on the original
    /// starting slot.
    fn acquire(&self) -> PoolGuard<'_> { ... }
}

struct PoolGuard<'a> {
    inner: MutexGuard<'a, Option<BatchChild>>,
    // Same methods as today's `slot.as_mut().expect("...")`
    // returns: query(oid) / query_batch(&[oid]) / take-and-reset
    // for respawn after I/O failure.
}
```

Replace `batch: Mutex<Option<BatchChild>>` on `GitCliFetcher`
with `batch: BatchChildPool`. Update `raw_fetch` and
`prefetch_headers` to call `self.batch.acquire()` instead of
`self.batch.lock()`. Most of the body inside the lock stays
identical.

### 2.3 Decision points

- **Pool size K.** Constructor takes K explicitly. `GitCliFetcher::open`
  defaults to some sensible value (probably `min(num_cpus, 8)`
  initially; tune empirically). Add `GitCliFetcher::open_with_pool_size(store, k)`
  for callers that want explicit control (the daemon will use
  this; tests can pin K=1 to verify behaviour is unchanged
  for the single-child case).
- **`PROJGIT_CATFILE_POOL_SIZE` env var.** Convenience for
  benching different K values without rebuilding. Read in
  `GitCliFetcher::open` as an override of the default.
  Optional; skip if it complicates the V1.
- **Respawn semantics on I/O failure.** Today
  `raw_fetch`/`prefetch_headers` clear `slot = None` on failure
  so the next call respawns. Same pattern per slot in the pool.
- **`Drop` ordering for the pool.** Today `BatchChild::Drop`
  closes stdin (so git sees EOF) then waits. Pool drops each
  slot's child the same way. Sequential drops are fine.
- **Lazy vs eager spawn.** Lazy (per slot, on first use):
  zero cost when the daemon is idle. Eager (spawn K on
  pool construction): predictable startup cost. **Pick lazy
  for V1** to match current behaviour; spawn rate isn't a
  bottleneck.
- **Fairness under starvation.** Pure round-robin try-lock
  starves slot K-1 if K-2 always finishes first. Acceptable
  for V1 — the actual cost is per-fetch (~hundreds of ms),
  not lock-acquisition latency. If profiling shows starvation,
  later swap for a fairer primitive.

### 2.4 Verification

- `cargo build --workspace` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace --all-targets` green. Pay particular
  attention to existing tests in
  `crates/projgit-core/tests/fetcher.rs` and
  `crates/projgit-daemon/tests/` — they all use the default K
  (whatever we pick) and should keep passing without changes.
- New unit test: pool with K=1 behaves identically to today's
  Mutex<Option<BatchChild>> (regression guard for "we didn't
  break the single-child case while building the pool").
- New unit test: pool with K=4, fire 4 concurrent
  fetch_object calls for distinct OIDs; assert they don't all
  serialise (timing-based, weak — but a sanity check).

### 2.5 Commit boundary

```
feat(core): BatchChildPool replaces single Mutex<BatchChild>
```

One commit. Pure plumbing change; behaviour-preserving when
K=1.

## 3. Stage 2 — wire pool size through to projgitd

### 3.1 Goal

`projgitd --pool-size N` flag lets operators pick K explicitly.
Default chosen for Harbor-shape deployments (probably K=8).

### 3.2 Concrete changes

- `DaemonConfig` gains `pool_size: usize` field, default
  picked here (e.g. `min(num_cpus::get(), 8)`).
- `DaemonState` carries it through to wherever `GitCliFetcher`
  is constructed inside the daemon. Currently in
  `attach_source`: `GitCliFetcher::open(store.clone())`. Change
  to `GitCliFetcher::open_with_pool_size(store.clone(), pool_size)`.
- `projgitd --pool-size N` CLI flag, plumbed to
  `config.pool_size`. Validate `N > 0`.
- Bench (`bench_mount.rs`) gains `--daemon-pool-size N` flag
  matching the `--daemon-depth` and `--daemon-trace` pattern,
  so we can run the diagnostic with explicit K.

### 3.3 Decision points

- **Default K.** Need a guess for what works for Harbor's
  N_sidecars ≈ 10. Plan says K ≥ N_sidecars + 1; that's 11. But
  more children = more git processes alive in the daemon = more
  RAM. `min(num_cpus::get(), 8)` is a hedge: bench machines
  often have 16+ cores, and 8 children should be enough for the
  10-sidecar workload. Pin defaults at Stage 3 (re-bench);
  adjust if the trace shows new contention shapes.
- **Skip a per-sidecar-mode default override.** The CLI's
  `projgit mount` (non-sidecar) uses `GitCliFetcher` directly
  too. Question: should `projgit mount`'s K default differ from
  the daemon's? Probably not — single-process mounts only have
  one consumer of GitCliFetcher (the FUSE thread + its prefetch
  worker = 2 concurrent callers), so a pool of K=2 would be
  sufficient. But keeping the default uniform is simpler. **Skip
  the per-call-site default for V1**; both use the same.

### 3.4 Verification

- Same as Stage 1, plus:
- `projgitd --help` shows `--pool-size`.
- Smoke: `projgitd --pool-size 4 --trace`; daemon starts; trace
  fires with K=4 children (visible indirectly via no
  contention).

### 3.5 Commit boundary

```
feat(daemon, cli, bench): --pool-size N for the cat-file pool
```

## 4. Stage 3 — re-bench rust-lang/rust with the pool

### 4.1 Goal

Run the same diagnostic that established the bottleneck, now
with the pool enabled. Capture the new trace output. Confirm
the head-of-line block is gone.

### 4.2 Concrete steps

1. Re-run the diagnostic recipe from
   [`../bench/baseline.md`](../bench/baseline.md) §Diagnostic
   with `--daemon-pool-size 4` (or whatever default we ship):
   ```sh
   PROJGIT_NETWORK_TESTS=1 \
     cargo run -p projgit-cli --example bench_mount --release -- \
     --scenario sparse-shared --concurrency 2 --iterations 1 \
     --daemon-depth 1 --daemon-trace \
     --url https://github.com/rust-lang/rust --ref main \
     --files README.md,Cargo.toml,LICENSE-APACHE
   ```
2. Compare the new trace to the pre-pool trace. Expected
   shape:
   - Two `PrefetchHeaders` calls run in parallel (each ~15s
     served — same per-batch cost, no improvement) **OR**
     each PrefetchHeaders is split across multiple cat-file
     children so each one finishes faster.
   - `Fetch` for `Cargo.toml` served in ~0.5 s (its real
     fetch cost), not ~15 s (mutex wait gone).
   - Per-thread wall drops from ~15 s to ~1–2 s.
3. If results match the prediction, run the rust-lang/rust
   matrix at N=4 and N=10 with the pool to validate at higher
   scale. (Each iter probably ~30 s now; 6 cells × 3 iters =
   ~10 min total.)
4. Cargo `sparse-shared` numbers should also re-improve.
   Re-run cargo at N=10 with the pool to update the headline.

### 4.3 Decision points

- **What if the wall doesn't drop?** Possible causes to
  investigate (ranked):
  - K too small (try K=10).
  - Per-blob promisor cost is actually 0.45s only in isolation;
    K parallel cat-files trigger K parallel promisor calls
    that throttle each other at GitHub. (Bandwidth or
    concurrent-connection limit.) If this is the cause, the
    fix has a different shape — protocol-level batching, or
    a small concurrency cap.
  - Per-batch coalescing matters more than expected; need to
    ship that follow-up too.
- **What if the wall drops but disk savings degrade?** Pool
  means more parallel cat-file children writing packs; could
  accidentally write more pack files than the single-child
  case. Re-check `disk_bytes` in the bench. Shouldn't matter
  structurally (each cat-file lazy fetch produces 1 pack
  regardless of K).

### 4.4 Verification

- New trace lines show `served_us` per `Fetch` ≈ per-blob
  fetch cost (~hundreds of ms), not mutex-wait time
  (~tens of seconds).
- Per-thread `wall_clock` from the bench report drops by the
  predicted ~10× (from ~15 s to ~1–2 s on rust).

### 4.5 Commit boundary

```
bench: re-capture rust-lang/rust + cargo with cat-file pool
```

### 4.6 Actual outcome (2026-06-18)

Stage 3 did not pass on the first post-pool run: the initial
K=4 diagnostic improved wall only ~1.65x (15.3 s -> 9.24 s),
which fired stop-condition #2 (<2x). Investigation added
`PROJGIT_CATFILE_TRACE=1` instrumentation in
`GitCliFetcher` (per-call `wait_us` + `work_us`) and revealed
that pool wait time was ~0 while work time remained long.

Root cause: a second, independent serialization point in
`projgit-daemon` -- `handle_fetch` and
`handle_prefetch_headers` held `state.active`'s mutex across
the full backend call, so only one RPC could enter
`repo.backend.*` at a time. This masked the cat-file pool.

Fix: clone `ActiveBackend` (cheap Arc clone) out of the mutex
critical section in both handlers, drop the lock, then call
`fetch_one` / `prefetch_headers`.

Post-fix measurements on rust diagnostic cell:

- K=1 pre-pool baseline: 15.3 s wall
- K=4 pool-only (mutex still held in handlers): 9.24 s wall
- K=1 with lock release: 9.71 s wall
- K=4 + lock release: 1.75 s wall (~8.8x vs baseline)

Interpretation: both fixes are required for the predicted
shape. The pool removes cat-file HoL blocking; the handler
lock-release lets concurrent RPCs actually reach the pool.

Cargo sparse-shared N=10 refresh with K=4:

- projgit-shared wall: 7.39 s (from 8.58 s pre-pool)
- partial-cat-independent wall: 8.50 s
- wall ratio: 1.15x, disk ratio: 9.98x

So rust-scale bottleneck is closed; cargo's wall win remains
modest and still behind worktree-depth1 on-demand total wall.

Updates the Diagnostic section in baseline.md with new trace
+ a "Post-pool measurements" table; refreshes the sparse-shared
+ worktree-comparator cargo numbers for projgit-shared.

## 5. Stage 4 — handoff bump + pitch language update

### 5.1 Goal

Handoff #1 demoted (cat-file pool done); pitch language
updated in README / project description to reflect post-pool
numbers. If projgit-shared now matches or beats
`worktree-depth1 on-demand` on wall clock, that goes in the
pitch.

### 5.2 Concrete changes

1. `docs/implementation/handoff.md`:
   - Bump Last updated.
   - Add Done bullet for cat-file pool with the
     before/after wall numbers + new trace snippet.
   - Demote #1 (pool); promote #2 (per-batch Coalescer for
     PrefetchHeaders) to top, and #3 (projgitd Stage 5).
2. `README.md` (or the top-level pitch in the project
   description):
   - If pool brought wall-clock parity or win: reframe to
     "projgit gives worktree-class wall-clock speed plus
     ~10× disk savings plus container-clean deployment".
   - If pool brought meaningful improvement but not parity:
     reframe to "projgit narrowed the wall-clock gap to ~X×
     and wins disk + containerization".
   - If pool fell short of prediction: document honestly and
     leave the pitch as-is from 2026-06-04.

### 5.4 Outcome guardrail update

Measured outcome after Stage 3: rust diagnostic meets the
prediction band (~1-2 s per-thread) only with the combined
pool + handler lock-release fix. Cargo wall improves but does
not reach worktree parity. Therefore Stage 4 pitch language
must stay workload-qualified:

- big-history multi-agent repos: strong wall + disk wins
- small-history repos like cargo: disk/containerization wins,
  wall still behind worktree-depth1 on-demand

### 5.3 Commit boundary

```
docs(handoff, README): cat-file pool shipped; pitch updated
```

## 6. Stop conditions

If any of these fire, **pause and update this plan before
pressing on:**

- **Stage 1 — `cargo test --workspace --all-targets` goes
  red.** The pool plumbing broke an existing contract.
  Fix before Stage 2.
- **Stage 3 — wall clock improves by < 2×.** Sub-prediction
  improvement means a different bottleneck dominates now.
  Add new trace fields if needed (per-acquire time, queue
  depth at acquire) and diagnose before declaring done.
- **Stage 3 — fetch failures appear at K>=4.** GitHub
  rate-limiting or local file-descriptor limits. Either
  reduce K or document the operational ceiling.

Status on 2026-06-18:

- Triggered once (pool-only run ~1.65x), diagnosed and fixed.
- Final post-fix run: stop condition cleared (~8.8x).
- No fetch failures observed at K=4 or K=10 in the diagnostic
  repro runs.

## 7. What this doc is not

- A spec for `BatchChildPool`'s exact API; the doc-comments on
  the type are the source of truth once it lands.
- A commitment to a specific default K. Stage 3 picks it
  based on observed behaviour.
- A pitch-language change. Stage 4 only updates the pitch if
  measurements justify it.
- A plan for the per-batch PrefetchHeaders coalescer. That's a
  separate item (#2 in the handoff) and is independent of this
  plan.
