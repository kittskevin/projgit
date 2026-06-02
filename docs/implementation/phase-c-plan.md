# Phase C — implementation plan

> Status: **living doc.** Tracks how the
> [Phase C design](../design/phase-c-bench.md) actually gets built.
> Updated as each stage lands or surfaces something that changes
> downstream stages.
>
> Last updated: 2026-06-02 (created; no stages started yet).
>
> Design lives in [`../design/phase-c-bench.md`](../design/phase-c-bench.md);
> this doc is one level down — concrete steps, file changes, commit
> boundaries, decision points. If something here contradicts the
> design doc, the design doc wins and this doc updates.

## 0. Why this doc exists

Mirrors the projgitd plan pattern: design = what + why; plan =
how + what to learn from each step before committing to the next.

Phase C is small (~half a session, ~bench code + result capture)
so this doc stays small. Detailed for the next step, rough for
later steps — refine as we go.

## 1. Pre-flight (~10 min)

Before writing code:

1. Re-read [`../design/phase-c-bench.md`](../design/phase-c-bench.md)
   §3 (methodology) and §6 (risks). The risks shape the bench's
   error handling.
2. Confirm the existing harness layout in
   [`../../crates/projgit-cli/examples/bench_mount.rs`](../../crates/projgit-cli/examples/bench_mount.rs):
   - `parse_args()` already takes `--scenario`; adding two new
     variants is a 4-line `match` extension.
   - `projgit_mount_once()` and `projgit_remount_cold_cat()` are
     the helpers to base the concurrent driver on.
   - Reporting is straight `eprintln!` ms tables — no Criterion,
     no fancy plotting.
3. Confirm baseline.md's section structure
   ([`../bench/baseline.md`](../bench/baseline.md)) so the Phase C
   results section matches.
4. Note one open chore: `/memories/repo/audit.md` (referenced in
   the handoff as the source-of-truth audit memory) does NOT
   currently exist in this devcontainer's repo memory. Phase C
   will note A3 closure in the handoff text directly and skip
   touching the audit memory.

## 2. Stage 1 — refactor: extract a fetcher-factory helper

### 2.1 Goal

Make `projgit_mount_once()` reusable from a concurrent driver
without duplicating its mount-and-cat body.

### 2.2 Concrete change

Today, `projgit_mount_once()` hard-codes:

```rust
let fetcher = GitCliFetcher::open(store.clone())?;
let hydrating = Arc::new(HydratingObjectStore::new(store, fetcher));
```

Refactor to take a closure that produces the hydrating store:

```rust
fn projgit_mount_once<F, MakeHydrating>(
    args: &Args,
    cache_dir: &Path,
    make_hydrating: MakeHydrating,
) -> anyhow::Result<MountTimings>
where
    F: Fetcher + 'static,
    MakeHydrating: FnOnce(Arc<ObjectStore>) -> anyhow::Result<Arc<HydratingObjectStore<F>>>,
```

The `single` / `sequential` scenarios pass a closure that builds
the `GitCliFetcher` (one-line wrap of the old behaviour).
`daemon-concurrent` will pass a closure that builds a
`HydratingObjectStore<DaemonFetcher>`. Same shape for
`naive-concurrent` (also `GitCliFetcher`, but constructed per
thread).

### 2.3 Verification

`cargo build -p projgit-cli --example bench_mount` clean.
`PROJGIT_NETWORK_TESTS=1 cargo run -p projgit-cli --example bench_mount --release`
produces the same numbers as before this commit (no semantic
change for `single` / `sequential`).

### 2.4 Commit boundary

One commit:

```
bench: extract a fetcher-factory in projgit_mount_once
```

Pure refactor; no new scenarios yet. Easy to revert if Stage 2
surfaces a better factoring.

## 3. Stage 2 — `daemon-concurrent` scenario

### 3.1 Goal

`--scenario daemon-concurrent --concurrency N` works end-to-end
against `rust-lang/log` on this devcontainer's network.

### 3.2 Concrete changes in `bench_mount.rs`

1. Extend the `Scenario` enum with `DaemonConcurrent` and add
   parser support.
2. Add `--concurrency N` (default 4) to `Args`. Defaults to 1
   for `single` / `sequential` (or just ignored there).
3. New `bench_projgit_daemon_concurrent(&args, &cache_dir)` that:
   - Partial-clones into `cache_dir`.
   - Spawns `projgitd` in-thread on a temp socket path (using
     `projgit_daemon::server::run` + `DaemonConfig`).
   - Waits for socket file to appear.
   - Creates N temp mountpoints up front.
   - Spawns N `std::thread::Builder::spawn`'d threads. Each
     thread:
     - Builds `DaemonFetcher::new(socket)` →
       `HydratingObjectStore<DaemonFetcher>` →
       `ProjectionFsProvider`.
     - `mount_background` at its own mountpoint, waits for
       mount.
     - Records its own `cold_cat` time (read each file in
       `args.files`, `read_to_string`).
     - Drops its `BackgroundSession`.
     - Returns its `cold_cat` duration to the main thread via
       a channel.
   - Main thread: times wall-clock from "all N threads spawned"
     to "all N joined". Collects per-thread durations.
   - Sends `Shutdown` over the daemon socket; joins the daemon
     thread.
4. New `ConcurrentSample { wall_clock, per_thread: Vec<Duration>,
   failures: usize }` (probably) and a `print_concurrent_report`
   that formats wall clock + per-thread p50 + failure count.

### 3.3 Decision points during implementation

- **In-thread daemon vs subprocess daemon?** In-thread is
  simpler, deterministic, and matches the pattern the existing
  `sidecar_mount_smoke.rs` tests already use. Subprocess is
  closer to production but adds CARGO_BIN_EXE_* discovery and
  startup-race handling. **Pick in-thread for V1.** Note the
  decision so a future iteration of the bench can revisit if
  the in-process daemon's CPU competition with sidecar threads
  shows up in results.
- **Thread count > number of CPU cores?** At N=10 on a 16-core
  bench machine we're fine. On a 4-core machine N=10 means
  thread thrash on top of network wait. Cold-fetch is network-
  bound so this should still mostly show network ratios, but
  call it out in the per-N caveats if results look off.
- **What if `mount_background` fails for one thread?** Likely
  EAGAIN under load. Count as a failure (separate from the naive
  scenario's git-lock failures); continue with N-1 successful
  threads. Don't panic. Report the failure rate.

### 3.4 Verification

- `cargo build --release -p projgit-cli --example bench_mount` clean.
- `PROJGIT_NETWORK_TESTS=1 cargo run -p projgit-cli --example bench_mount --release -- --scenario daemon-concurrent --concurrency 4`
  runs to completion and prints a wall-clock + per-thread table.
- Sanity: at N=1, daemon-concurrent wall clock should be within
  ~5% of `single` cold-cat (no concurrency, daemon IPC overhead
  is tiny).

### 3.5 Commit boundary

```
bench: add daemon-concurrent scenario (Phase C, daemon arm)
```

Has the in-thread daemon plumbing + the N-thread driver + the
new report shape. Can ship without the naive comparator —
intermediate state is meaningful.

## 4. Stage 3 — `naive-concurrent` comparator

### 4.1 Goal

`--scenario naive-concurrent --concurrency N` works end-to-end;
the result table shows both scenarios at the same N for direct
comparison.

### 4.2 Concrete changes

1. Add `Scenario::NaiveConcurrent` + parser support.
2. New `bench_projgit_naive_concurrent(&args, &cache_dir)`:
   - Partial-clones into `cache_dir` (same as daemon arm).
   - Skips the daemon entirely.
   - Spawns N threads, each:
     - Builds its OWN `GitCliFetcher::open(store.clone())`
       pointing at the **same** `cache_dir` as every other
       thread. This is the actual A3 scenario.
     - Mounts FUSE locally, cold-cats, unmounts.
     - Returns timing or error.
   - Failures: log per-thread, count, report.
3. Reuse the `print_concurrent_report` from Stage 2.
4. Optionally: when running `--scenario daemon-concurrent`, also
   run the naive arm automatically and print the ratio inline.
   **Decide during implementation** whether to bundle (cleaner
   table) or keep separate invocations (cleaner CLI surface).

### 4.3 Risk: concurrent `git fetch` corruption

Mitigation (per design doc §6.1):

- Each bench iteration uses a fresh cache dir; no cross-iteration
  state pollution.
- Per-thread errors are caught and counted; bench doesn't panic.
- If failure rate > 50% at N=10, that's a Phase C finding worth
  noting explicitly in the result table — it means the naive
  scenario isn't just slow, it's unreliable, which itself
  justifies the daemon architecture.

### 4.4 Decision points

- **Does git's pack-lock cause spurious test failures we should
  retry?** If yes, the bench's "naive failure rate" is meaningless
  without retries. If no, raw failures are real signal.
  **Settled by observation, not pre-decided.**
- **What if naive-concurrent is *faster* than daemon-concurrent
  at low N?** Possible if daemon IPC overhead exceeds the savings
  at N=1 or N=2. That's still a meaningful result; document and
  move on. Don't try to "fix" the bench to hide it.

### 4.5 Verification

- Same as Stage 2 plus: at N=4 and N=10, both scenarios complete
  (modulo expected naive failures); the ratio between their wall
  clocks is reportable.

### 4.6 Commit boundary

```
bench: add naive-concurrent comparator (Phase C, A3 baseline arm)
```

Closes the bench feature work. Result capture is the next stage.

## 5. Stage 4 — capture results

### 5.1 Goal

Phase C results land in [`../bench/baseline.md`](../bench/baseline.md)
in the same structure as the existing `single` / `sequential`
sections.

### 5.2 Concrete changes

1. Run both scenarios on `rust-lang/log` at N ∈ {1, 4, 10},
   median of 3 iterations each. Capture all numbers.
2. Append a new section to `docs/bench/baseline.md`:
   - `## Results — Phase C concurrent (rust-lang/log @ master)`
     header
   - Reproduce block (the actual command lines)
   - Per-N table: `daemon-concurrent` wall clock, per-thread p50,
     `naive-concurrent` wall clock, per-thread p50, failure
     count, ratio.
   - "What this shows" prose summarising the result (whatever
     it turns out to be — write *after* running, not before).
   - "Caveats" specific to Phase C (in-thread daemon, network
     variance, etc., per design doc §6).
3. Optional (only if `log`'s numbers are unclear): run on
   `rust-lang/cargo` too. Add as secondary table. If `log`
   tells a clear story, skip this — don't over-collect data.

### 5.3 Decision points

- **What if results contradict the expected shape from
  design-doc §4?** Capture the actual numbers; don't massage.
  Update the design doc's "expected shape" section to record
  the actual finding (with date) so the doc stays honest about
  what was expected vs what was real.
- **What if there's no consistent ratio at all (high variance)?**
  Run more iterations (5 or 7 instead of 3). If still noisy,
  report the variance; don't fabricate a clean number.

### 5.4 Commit boundary

```
bench: capture Phase C concurrent results in baseline.md
```

## 6. Stage 5 — update handoff

### 6.1 Goal

[`handoff.md`](handoff.md) Done section gains a Phase C bullet;
"What I'd do next" demotes Phase C off the active list (now
done) and re-checks the queue.

### 6.2 Concrete changes

1. Bump the `Last updated` line.
2. Add a Done bullet for Phase C — what landed (the two
   scenarios + the baseline.md capture), what it measured, what
   it found (the actual numbers).
3. Drop Phase C from "What I'd do next" #1; promote Stage 5
   (production polish) to #1.
4. Note "A3 measured" anywhere the audit-closure list lives in
   this handoff (today the handoff doesn't have a dedicated
   audit section; the closure note goes in the Phase C Done
   bullet itself).
5. **Skipped on purpose:** updating `/memories/repo/audit.md` —
   that file doesn't currently exist in this devcontainer's repo
   memory. If a future session re-creates it, the closure note
   propagates from the handoff.

### 6.3 Commit boundary

```
docs(handoff): Phase C done; Stage 5 promoted to next-up
```

## 7. Stop conditions

If any of these fire during a stage, **pause and update the
design doc before pressing on:**

- **Stage 2 (daemon arm) wall clock at N=1 differs from `single`
  cold-cat by > 20%.** Either the daemon adds unexpected
  per-call overhead, or the bench is measuring something other
  than what it claims. Don't paper over; understand it.
- **Stage 3 (naive arm) never finishes — all threads hang on a
  shared pack lock.** Means the naive scenario isn't just slow,
  it's deadlock-prone with this number of concurrent fetchers.
  That's a Phase C finding by itself; capture and move on, don't
  try to make it complete.
- **Stage 4 (result capture) — the headline ratio is < 1.5× at
  N=10.** Means the daemon architecture isn't winning empirically
  the way it should architecturally. Don't capture and call it
  done — investigate first. Possible causes: per-call
  `UnixStream::connect` overhead exceeding savings (suggests
  Stage 5 protocol pipelining as a real follow-up); CPU
  contention from in-thread daemon (suggests subprocess variant);
  git's pack-lock allowing naive fetches to be unexpectedly
  cheap.

## 8. What this doc is not

- A schedule. No dates, no commitments to a release.
- A spec. The bench source under `crates/projgit-cli/examples/`
  carries its own doc comments; this doc captures the *plan*,
  not the final API.
- A binding promise about results. Stage 4 captures whatever the
  bench produces; the doc updates if reality differs from
  expectation.
- A user-facing roadmap. That's the handoff's "What I'd do
  next" derived from this plan.
