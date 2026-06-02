# Design: Phase C — concurrent cold-fetch amortisation bench

> Status: **run 2026-06-02.** Both scenarios implemented and
> executed at N ∈ {1, 4, 10} on `rust-lang/log`; results captured
> in [`../bench/baseline.md`](../bench/baseline.md) §Phase C. The
> implementation plan with sub-stages and commit boundaries is in
> [`../implementation/phase-c-plan.md`](../implementation/phase-c-plan.md).
> Headline finding: the daemon's in-flight coalescer doesn't
> deliver a wall-clock win at this workload scale (1.04× at
> N=10, ~12% loss at 20-file/N=10). The §4 "expected shape"
> below is left as-shipped and annotated post-run with what
> actually happened so the doc stays honest about expected vs.
> measured.
>
> Read alongside [`workload.md`](workload.md) §1.6 (the headline
> amortisation claim Phase C tests in its hardest case),
> [`projgitd.md`](projgitd.md) §1/§7 (the daemon architecture whose
> in-flight coalescer is the mechanism Phase C measures), and
> [`fetch-coalescing.md`](fetch-coalescing.md) (the related but
> distinct in-process coalescing investigation that this bench does
> **not** revisit).

## 0. Why this document exists

projgit's pitch — "100 short-lived containers per host sharing one
cache" — has two amortisation claims:

1. **Sequential.** Mount 1 dies; mount 2 starts; mount 2 finds its
   bytes on the on-disk CAS that mount 1 warmed. Measured in
   [`bench/baseline.md`](../bench/baseline.md) §sequential: mount 2
   cold-cat ~1 ms vs mount 1 cold-cat ~3.4 s. **Confirmed
   2026-05-18.**
2. **Concurrent.** N mounts simultaneously cold-fault the same OID.
   Without coordination they each spawn `git fetch <oid>` against
   the same `.git/objects/pack/` and pay N× upstream cost. **Not
   yet measured.** This is what audit item A3 (cross-process
   single-flight gap) is about; the projgitd Stage 2 / Stage 3
   daemon was the architectural answer; this bench is the
   empirical confirmation.

This document specifies how Phase C measures (2), what counts as
shipped, and what wouldn't.

## 1. The question

**At concurrency level N, how much does the daemon's in-flight
fetch coalescer save vs N independent consumers racing to fetch
the same blobs?**

Phrased as a number: pick a representative URL (`rust-lang/log`)
and a small set of cold blobs; cold-cat them in parallel from N
sidecars; compare two configurations:

- **`daemon-concurrent`** — one `projgitd`, N sidecars holding
  `DaemonFetcher`, each cold-cat-ing the same files.
- **`naive-concurrent`** — no daemon. N independent
  `GitCliFetcher`s pointing at the same on-disk cache dir, racing.

The ratio `naive_wall_clock / daemon_wall_clock` at each N is the
headline number. The shape of that ratio as N grows is the
secondary number.

## 2. The architectural property under test

[`projgitd.md`](projgitd.md) §1 and §7 (audit closure) commit to
this property:

> N sidecars asking for the same OID concurrently see **one**
> upstream fetch (the daemon's `HydratingObjectStore::header()`
> goes through the existing `Coalescer`), not N.

That's a **design property** that has been true since Stage 2
landed. Phase C is the **measurement** of how much that property
matters in wall-clock terms for projgit's specific workload shape.

The bench is deliberately **not** an investigation of whether the
coalescer is correct — that's covered by the unit / integration
tests in `crates/projgit-core/src/fetcher/coalesce.rs` and
`crates/projgit-daemon/tests/`. Phase C just runs both configurations
and reports the gap.

## 3. Methodology

### 3.1 Scenarios

Two new scenarios on `crates/projgit-cli/examples/bench_mount.rs`,
alongside the existing `single` and `sequential`:

| Scenario | Topology | Cache dir | Coordination |
|---|---|---|---|
| `daemon-concurrent` | 1 daemon + N sidecars (in threads) | shared (daemon owns it) | daemon's coalescer |
| `naive-concurrent` | N independent local mounts (in threads) | shared (the actual A3 scenario) | none |

Both are gated by `--scenario <name> --concurrency <N>`. Default
N = 4; the bench accepts arbitrary N for exploration.

### 3.2 What's measured

For each scenario at each N, per iteration:

1. Fresh cache dir + fresh `partial_clone` of the target URL.
2. Spin up the scenario's structure (start daemon if needed; pre-
   create N mountpoints).
3. **The measurement window:** spawn N threads that each build
   their fetcher / store / provider stack, mount FUSE locally,
   and cold-cat the file list. Wait for all N to finish. Time
   from first thread spawn to last thread complete.
4. Per-thread: also record each thread's own start/finish so we
   can report median per-sidecar latency in addition to wall
   clock.
5. Tear down all mounts; drop the daemon (if any); drop the
   cache dir.

Two numbers reported per (scenario, N) pair:

- **`total_wall_clock`** — the load-bearing headline. "How long
  did it take to satisfy N consumers cold-reading the same
  files?"
- **`per_thread_p50_ms`** — what an individual consumer
  experienced. Sanity check: in the daemon scenario this should
  *also* be roughly baseline-single-mount-cold-cat (not N× that)
  because the per-sidecar work is bounded.

A failure-mode column too:

- **`naive failures`** — number of threads in the naive scenario
  that errored with a git lock / pack contention failure. If
  consistently > 0, that's itself a Phase C finding worth
  reporting.

### 3.3 Concurrency levels

Default matrix: N ∈ {1, 4, 10}.

- **N=1** — sanity baseline. Both scenarios should converge here
  (no coordination needed for 1 consumer). If they differ
  meaningfully at N=1, something's wrong before the test even
  exercises the property under test.
- **N=4** — the realistic-eval-rig number. If the daemon win is
  visible here, the deployment shape projgit pitches for actually
  benefits.
- **N=10** — stress-test the coalescer / pack-lock contention.
  100 (the README's headline) would be ideal but is bench-
  machine-dependent (FUSE mount × N × file descriptors); 10 is
  the responsible default.

The bench accepts `--concurrency N` for exploration above the
default matrix.

### 3.4 Target

Initial: `rust-lang/log` (the existing baseline target). Reuses
the existing harness's URL handling. `rust-lang/cargo` is a
secondary follow-up if `log`'s numbers don't tell a clear story.

## 4. Expected shape (this is an expectation, not a promise)

> **Update 2026-06-02 (post-run): expectations did not hold.** Both
> the absolute per-consumer baseline and the qualitative shape of
> the ratio at N=10 were wrong. Actual results captured in
> [`../bench/baseline.md`](../bench/baseline.md) §Phase C — at
> N ∈ {1, 4, 10} on `rust-lang/log` with 3 small blobs, the two
> arms converge to within ~5–8% (ratio 1.04–1.08× at N=4/10, with
> N=1 single-iteration variance making daemon-N=1 nominally
> *slower* by 25% but inside the per-thread range). At 20 files /
> N=10 the daemon actually *loses* by ~12% (naive 8.1 s vs daemon
> 9.1 s) because the daemon serialises N×files unique fetches
> through one shared `git cat-file --batch-check` child, while
> the naive arm pipelines them across N parallel `cat-file`
> children with N parallel HTTPS connections to GitHub. The
> mechanism written below ("daemon coalesces N×duplicate fetches
> down to 1") is real — the coalescer does dedupe — but at this
> bandwidth / RTT regime the savings are bounded by per-fetch
> RTT, while the naive arm's cost is bounded by per-thread
> parallel work. They converge instead of diverging. The
> §3 methodology section's "secondary numbers" (per-thread p50,
> failure count) are still valid framing; only the §4 numerical
> expectations were wrong.
>
> **Original expectation block kept below** so the doc records
> what was expected before running vs. what was real after.
> See baseline.md for the headline.

Using existing `single` cold-cat as the per-consumer baseline
(~3.4 s, of which ~95% is upstream HTTPS round-trip):

| N | `daemon-concurrent` wall clock | `naive-concurrent` wall clock | Expected ratio |
|---|---|---|---|
| 1 | ≈ baseline (~3.4 s) | ≈ baseline (~3.4 s) | ~1.0× |
| 4 | ≈ baseline + small fan-out cost (~3.5 s) | depends on git lock serialisation: 3.4–13.6 s | 1.0–4× |
| 10 | ≈ baseline + larger fan-out cost (~4 s) | depends: 3.4–34 s | 1.0–10× |

The bracketed naive numbers reflect that **we don't know how git's
pack-lock behaves under concurrent fetch of distinct OIDs in the
same `.git/`**. Three plausible regimes:

- **Git locks aggressively**: every concurrent `git fetch`
  serialises → naive cost = N × baseline.
- **Git allows pipelined fetches**: kernel-level disk write
  contention only → naive cost ≈ baseline + small overhead.
- **Git fetches partly succeed, partly fail**: some threads error
  with lock contention → naive's "wall clock" is the lucky
  threads' completion plus the failure log.

The bench finding out which regime is real is itself a result
even if the ratio is unsurprising.

> **Actual answer (2026-06-02):** regime 2 ("Git allows pipelined
> fetches"). At N=10 the naive arm not only doesn't fail, it
> matches (3 files) or beats (20 files) the daemon. Per-thread
> wall clock dominates; pack-lock contention is invisible at this
> N. Higher N would be needed to surface the other regimes; the
> bench supports `--concurrency` for that exploration.

## 5. Success criteria

Phase C is **shipped** when:

1. The two new scenarios (`daemon-concurrent` and
   `naive-concurrent`) are implemented in `bench_mount.rs` and
   work end-to-end at N ∈ {1, 4, 10} on `rust-lang/log`.
2. Results captured in [`../bench/baseline.md`](../bench/baseline.md)
   under a new "Results — Phase C concurrent" section with the
   same shape as the existing scenario sections (Reproduce /
   Environment / What was measured / Results table / What this
   shows / Caveats).
3. [`../implementation/handoff.md`](../implementation/handoff.md)
   updated: Phase C done; next-up promoted accordingly.
4. The bench is reproducible — running it again should produce
   numbers of the same shape (not the same exact values; bench
   variance is documented).

Phase C is **not** trying to deliver:

- A pretty graph. Numbers in a table are enough.
- Multi-host / multi-network coverage. Single machine.
- An automated CI bench. That's the separate "CI bench job"
  item on the handoff's next-up list.
- A perf-tuning pass. If the numbers reveal a bottleneck, that's
  a separate piece of work informed by this data.

## 6. Risks

### 6.1 Concurrent `git fetch` in the naive comparator

The naive scenario is exactly what
[`container-deployment.md`](container-deployment.md) §5.3 calls
"highest-risk to run on the host (concurrent `git fetch` children
writing the same `.git/objects/pack/`)." In a bench context the
worst case is a bench iteration fails with a lock error.

Mitigation: each iteration uses a fresh cache dir (existing harness
behaviour). Failures are counted, not panicked-on. Multiple-failure
runs are reported in the result table — that's data, not a bug.

### 6.2 Bench-machine concurrency limits

N FUSE mounts + N threads + N file descriptors per thread.
At N=10 this is well within Linux defaults; at N=100 it could
push `ulimit -n`. The bench's default matrix tops at N=10
specifically to stay inside out-of-the-box limits.

### 6.3 Network variability

Cold-fetch time dominates the measurement. A single residential
broadband connection introduces ±20% variance per run. Median of
3 iterations is the existing harness's smoothing strategy; Phase
C inherits it.

### 6.4 In-process daemon ≠ separate-process daemon

The bench runs the daemon in-thread (via the library API) for
measurement determinism. A production deployment runs `projgitd`
as a separate process. The wall-clock numbers should match
closely because cross-process IPC adds < 1 ms RTT vs in-process
mutex; verifying that's actually true is a worthwhile sanity
check (e.g. one separate-process iteration alongside the in-
process matrix) but not load-bearing for the headline claim.

### 6.5 Daemon CPU contention with bench threads

The daemon, the N sidecar threads, and the bench's measurement
threads all live in one process. CPU competition could distort
per-thread latency. Mitigation: report total wall clock as the
primary number (CPU-budget-fair regardless), report per-thread
p50 as a secondary number with explicit caveat.

## 7. Open questions to resolve while running

These are deliberately not answered here. Each is settled by
running the bench, not by speculation.

- **Does the naive scenario actually fail at N ≥ 4?** If yes,
  Phase C produces an additional finding ("naive doesn't just
  scale badly, it fails") that's more valuable than the ratio
  itself.
- **At what N does the daemon-scenario wall clock stop scaling
  flat?** Stage 2's daemon serialises mount/umount through a
  mutex but the fetch path through the coalescer is per-OID
  concurrent. We expect flat scaling up to the point where the
  daemon's CPU is saturated or its single batch-check git child
  is the bottleneck.
- **Does mount2_cold_cat in `sequential` (the existing ~1 ms)
  reproduce as per-thread p50 in `daemon-concurrent`?** It
  should — same architectural path (warm CAS + in-process LRU
  warm-up). If it doesn't, there's an unexpected cost in the
  sidecar path worth investigating.

## 8. What this document is not

- A prediction. The "expected shape" in §4 is the mental model
  going in; the bench's job is to falsify or confirm it.
- A specification of how the bench is implemented. That's
  [`../implementation/phase-c-plan.md`](../implementation/phase-c-plan.md).
- A pre-commitment to follow-up work. If the daemon's win is
  small, that's a finding; it doesn't auto-trigger any
  remediation.
- A microbenchmark. Phase C measures cold-path wall clock with
  network round-trips dominant; it's a coarse system bench, not
  a CPU-bounded perf experiment.
