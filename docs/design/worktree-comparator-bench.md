# Design: worktree-comparator bench

> Status: **planned, not yet run.** The implementation plan with
> sub-stages and commit boundaries is in
> [`../implementation/worktree-comparator-plan.md`](../implementation/worktree-comparator-plan.md).
> Existing bench results in
> [`../bench/baseline.md`](../bench/baseline.md); this bench's
> results will land there in a new section.
>
> Read alongside [`sparse-access-bench.md`](sparse-access-bench.md)
> §1 (the bench whose comparator this corrects),
> [`workload.md`](workload.md) §1 (sparse-access workload), and
> [`container-deployment.md`](container-deployment.md) §6
> (Scenario A / Harbor threat model — why worktrees aren't an
> alternative even when they're a fair perf comparator).

## 0. Why this document exists

The sparse-access bench (shipped 2026-06-02) compared
`projgit-shared` to `partial-cat-independent` — N agents each
running their own `git clone --filter=blob:none --no-checkout`.
That's a strawman: a competent operator deduplicating N agents
on one host would reach for `git worktree`, not N independent
clones.

This bench replaces the strawman with the steelman. The
question becomes: **against the most-efficient pure-git
multi-agent deployment, does projgit-shared still win, and on
what axes?**

The likely answer (per the architecture assessment) is "wall
clock ties or loses against optimised worktree configurations;
disk still wins decisively; the structural advantage is
containerization-cleanness and pre-staging-independence, neither
of which this bench measures directly". The point of the bench
is to make that framing honest by having numbers behind it.

## 1. The question

For sparse access to a moderately-sized repo by N agents on
one host, how does projgit-shared compare to **one clone + N
`git worktree add`** under two pre-staging modes:

- **operator-pre-stages**: clone + N worktrees provisioned in
  setup; agents only do the script.
- **agents-on-demand**: clone in setup; each agent does its own
  `worktree add` inside the measurement window.

Two clone strategies for the worktree arms:

- **`worktree-full`**: full `git clone` (everything: history +
  tree + blobs).
- **`worktree-depth1`**: `git clone --depth=1` (one commit's
  tree + blobs, no history).

Cross-product: 2 modes × 2 strategies = 4 worktree cells per N,
plus `projgit-shared` for direct comparison and
`partial-cat-independent` retained for continuity (lets readers
see the full bracket: strawman comparator → steelman comparator
→ projgit).

## 2. The architectural properties under test

The sparse-access bench validated two pitch claims:

1. **Multi-agent disk dedup.** ~10× win for projgit-shared vs N
   independent partial clones (at 100 % blob overlap, N=10).
2. **Multi-agent wall-clock amortisation.** 1.59× win for
   projgit-shared at N=10 — driven by amortising the partial
   clone once across N agents instead of N times.

The worktree comparator stress-tests both claims against a
deployment that *also* deduplicates:

1. **Disk dedup against a sharing competitor.** Worktrees share
   `.git/objects` but materialise N working trees on disk. So
   worktree-disk = `.git` + N × working-tree-size; projgit-disk
   = `.git`-only (no working tree materialisation). Predict
   projgit still wins ~10× because working-tree materialisation
   dominates the worktree-arm disk total.

2. **Wall-clock amortisation against a competitor that ALSO
   amortises setup.** A worktree-using operator pays the clone
   once and the per-worktree-add cost N times (~hundreds of ms
   each). projgit-shared pays the partial clone once and the
   per-FUSE-mount cost N times (~500 ms each). The numbers are
   in the same range; the prediction is "rough tie", not a
   projgit win.

If the prediction holds, the load-bearing pitch reframes from
"projgit wins wall clock" to **"projgit gives worktree-class
storage efficiency without worktree-class deployment overhead
(no operator pre-staging, no two-bind-mount-per-container
plumbing, no cross-tenant `.git` writeability)"**.

If the prediction *fails* — say projgit-shared loses wall
clock decisively against `worktree-depth1` operator-pre-stages
— that's also a finding, and it forces an honest update to the
pitch language.

## 3. Methodology

### 3.1 Scenarios

One new scenario on `crates/projgit-cli/examples/bench_mount.rs`:
`worktree-shared`, with two new orthogonal flags:

- `--worktree-strategy {full|depth1}`
- `--worktree-mode {pre-stage|on-demand}`

Run the cross-product at the existing N matrix to produce 4
worktree cells per N. The existing `sparse-shared` scenario's
two configurations (`projgit-shared` and
`partial-cat-independent`) are run alongside for direct
comparison.

### 3.2 What's measured

Same shape as `sparse-shared`:

- `setup` — wall clock of setup window.
- `wall_clock` — barrier-release to last-thread-join.
- `per_thread_p50` — median per-thread script time across all
  iterations.
- `disk_bytes` — total bytes resident after measurement window
  (`.git` + sum of N worktree dirs for worktree variants;
  `cache_root` for projgit-shared).
- `failures` — threads that errored.

Median of 3 iterations per cell.

### 3.3 What each pre-staging mode means

- **`pre-stage`**: setup = `git clone <strategy>` + N ×
  `git worktree add <wt-i> <ref>`. Measurement window = N
  threads each run the script against their assigned worktree.
- **`on-demand`**: setup = `git clone <strategy>` only.
  Measurement window = N threads each run
  `git worktree add <wt-i> <ref>` + the script, all inside the
  per-thread timing.

For projgit-shared: stays as today (Attach + partial clone in
setup; per-thread mount + script in measurement window).
There's no "pre-stage" mode for projgit because per-FUSE-mount
setup is per-agent and the operator can't do it ahead of time
on the agent's behalf — that asymmetry IS one of the
architectural findings.

### 3.4 Concurrency levels

Default matrix: N ∈ {4, 10}. Same as sparse-shared. The bench
accepts `--concurrency` for exploration.

### 3.5 Target

`rust-lang/cargo` @ `master`. Same as sparse-shared so the
projgit-shared cell composes directly with the existing
baseline.md numbers. Same 10-file script.

A `rust-lang/rust` or `torvalds/linux` follow-up is reasonable
("does the disk gap widen on bigger repos?"), but deferred —
single mid-size target is enough to establish the structural
shape.

### 3.6 Cleanup

`git worktree add` writes to the shared `.git/worktrees/`.
Easiest cleanup strategy: drop the whole shared clone dir
between iterations via `DirGuard`. No `git worktree remove`
needed; the bench's existing temp-dir pattern handles it.

## 4. Expected shape (mental model, to be falsified)

`rust-lang/cargo` @ master, N=10, 10-file script. All numbers
approximate, in ms / KiB:

| Config | setup | per-agent | wall | disk total | total time |
|---|---:|---:|---:|---:|---:|
| `projgit-shared` (existing) | ~3,000 | ~8,000 | ~8,500 | ~24,000 | ~11,500 |
| `worktree-full` pre-stage | ~10,000 | ~ms | ~10 | ~280,000 | ~10,010 |
| `worktree-full` on-demand | ~5,000 | ~1,500 | ~1,500 | ~280,000 | ~6,500 |
| `worktree-depth1` pre-stage | ~5,000 | ~ms | ~10 | ~230,000 | ~5,010 |
| `worktree-depth1` on-demand | ~3,000 | ~1,500 | ~1,500 | ~230,000 | ~4,500 |
| `partial-cat-independent` (existing strawman) | ~0 | ~13,000 | ~13,600 | ~245,000 | ~13,600 |

Predicted findings:

1. **Disk: projgit-shared wins ~10× against every worktree
   configuration.** Robust to mode and strategy. Structural; the
   pitch's load-bearing axis.
2. **Wall clock: worktree variants tie or beat projgit-shared.**
   `worktree-depth1` on-demand is the likely winner overall
   (~4.5 s total vs projgit's ~11.5 s). `worktree-full`
   pre-stage roughly ties (~10 s). The current "projgit wins
   1.59× wall clock" headline (vs the strawman) becomes "rough
   tie or modest loss" against the steelman.
3. **Operator-pre-stage gap**: ~3–5 s for worktree variants.
   Quantifies what pre-staging buys you — and importantly, what
   projgit can't access because per-mount setup is per-agent.

### What contradicts the prediction would be interesting

- **`projgit-shared` wins wall clock decisively against
  `worktree-depth1` pre-stage.** Surprising; would suggest the
  bench is measuring `worktree add` cost wrong or that there's
  unexpected parallel-contention overhead in worktree-add at
  N=10.
- **Disk gap is much narrower than 10×.** Would mean working-
  tree materialisation isn't the dominant disk cost on cargo —
  possible if cargo's `.git` is much bigger than I'm assuming.
- **`git worktree add` fails under N=10 parallel contention.**
  Worktrees aren't designed for high-concurrency creation; a
  real failure mode at N=10 would itself be a finding (and
  argue that worktrees aren't a fair comparator for parallel-
  agent provisioning).

## 5. Success criteria

Worktree-comparator bench is **shipped** when:

1. The new `worktree-shared` scenario is implemented with both
   strategy and mode flags.
2. Results captured in
   [`../bench/baseline.md`](../bench/baseline.md) in a new
   "Worktree comparator" section with the matrix above.
3. The existing sparse-access section's "What this shows" prose
   is updated to point at the new section as the more honest
   comparison (the sparse-access headline becomes
   contextualised, not erased).
4. [`../implementation/handoff.md`](../implementation/handoff.md)
   updated with the new Done bullet and any pitch-language
   reframing the numbers warrant.

What this bench is **not** trying to deliver:

- A containerization comparison. The "worktrees don't bind-mount
  cleanly" point is captured in §6 below as the architectural
  reason worktrees aren't actually usable for Harbor's scenario,
  but it's not benched here — it's a deployment-shape claim,
  not a wall-clock measurement.
- A multi-target bench. Cargo only; bigger-target follow-up if
  the cargo numbers don't tell a clear story.
- A `git clone --shared` (alternates) comparator. Worktrees are
  the more common pattern; alternates have similar issues plus
  worse lifecycle properties (source-clone pruning breaks
  derivatives). Skip.

## 6. Risks

### 6.1 `git worktree add` parallel contention

N=10 simultaneous `worktree add` calls into the same shared
clone all write to `.git/worktrees/` and trigger working-tree
materialisation in parallel. Git's locking on these paths is
robust but not designed for high concurrency. Possible
failure modes: lock-file collisions, inode-allocation
contention, slow checkouts due to FS journal pressure.

Mitigation: count failures per Phase C pattern; if rate > 50 %
at N=10, that's a finding on its own ("worktrees aren't
multi-agent-safe at this scale"), capture and move on.

### 6.2 Disk accounting honesty

`disk_bytes_of` walks file sizes recursively. For worktrees,
this correctly captures `.git` + all N worktree dirs because
they're all under the same parent. But the shared `.git`
includes a `worktrees/` subdir with per-worktree state — that
gets counted once (in `.git`), not N times. That's correct;
just be explicit in the writeup so disk numbers aren't
misread.

### 6.3 Setup-window asymmetry across modes

`pre-stage` mode runs `worktree add` in setup, so all N
worktree-add costs are paid in serial before the measurement
window opens. `on-demand` mode runs them in parallel inside the
measurement window. The two modes test different operator
postures (pre-provision vs spawn-on-demand), and the gap is the
finding; don't try to normalise the two modes against each
other except via "total time" (setup + wall_clock).

### 6.4 Containerization is not benched

The headline architectural argument for projgit over worktrees
is that worktrees don't bind-mount cleanly into containers
(two bind-mounts required per agent; per-worktree state lives
in the shared `.git`; cross-tenant `.git` writeability).
**None of this is testable in a wall-clock bench.** The writeup
must make this explicit: the bench measures one axis (per-host
parallel agents); the deployment-shape claim is separate and
relies on the threat model in
[`container-deployment.md`](container-deployment.md) §6.

This is a real risk for the bench's narrative: someone could
read "worktrees win wall clock" and conclude projgit isn't
needed, missing the containerization argument. The "What this
shows" prose has to load-bear here.

### 6.5 Cleanup between iterations

`git worktree add` modifies the shared `.git/worktrees/`
directory. If iterations don't clean up properly, stale
worktree entries accumulate and bias results. Mitigation: drop
the whole shared clone dir between iterations via the existing
`DirGuard` pattern.

## 7. Open questions to resolve while running

- **At what N does `worktree add` start failing under
  contention?** Open question; the bench captures it as
  failure-count.
- **Does `worktree-depth1` `on-demand` beat `worktree-full`
  `pre-stage` consistently?** They measure different things
  (smaller upfront vs already-staged); if `depth1 on-demand`
  wins, that's the recommendation for an operator who can't
  pre-stage. If `full pre-stage` wins, that's the recommendation
  for one who can.
- **Does the disk gap (projgit ~10× win) hold on the smaller
  `--depth=1` worktree config too?** Mathematically it should
  (working-tree materialisation × N still dominates). Worth
  confirming empirically.

## 8. What this doc is not

- A prediction. §4 is mental-model framing; bench falsifies or
  confirms.
- A spec of the bench code. That's the implementation plan.
- A claim about deployment ergonomics. The "worktrees don't
  containerize" argument is architectural; the bench measures
  wall clock and disk only.
- A "projgit is faster" reaffirmation. If the numbers say
  worktrees win wall clock, the writeup says worktrees win wall
  clock; the pitch reframes to the axes projgit still leads on
  (disk, containerization, no-pre-staging-needed).
