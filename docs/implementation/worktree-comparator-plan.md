# Worktree-comparator bench — implementation plan

> Status: **living doc.** Tracks how the
> [worktree-comparator design](../design/worktree-comparator-bench.md)
> actually gets built. Updated as each stage lands or surfaces
> something that changes downstream stages.
>
> Last updated: 2026-06-04 (created; no stages started).
>
> Design in [`../design/worktree-comparator-bench.md`](../design/worktree-comparator-bench.md);
> this doc is one level down — concrete steps, file changes,
> commit boundaries, decision points.

## 0. Why this doc exists

Same pattern as
[`phase-c-plan.md`](phase-c-plan.md) and
[`sparse-access-plan.md`](sparse-access-plan.md): design = what
+ why; plan = how + what to learn from each step.

This bench is small — the framework already exists post-Phase-C
+ sparse-access. What's new: `git worktree add` orchestration,
two new orthogonal flags, a report format that shows the full
matrix.

## 1. Pre-flight (~5 min)

Before writing code:

1. Re-read
   [`../design/worktree-comparator-bench.md`](../design/worktree-comparator-bench.md)
   §3 (methodology) and §6 (risks). §6.1
   (worktree parallel contention) and §6.4 (containerization
   not benched) shape the writeup tone.
2. Skim the existing `bench_sparse_shared` (and its two
   per-config helpers) in
   [`../../crates/projgit-cli/examples/bench_mount.rs`](../../crates/projgit-cli/examples/bench_mount.rs)
   — the new code reuses the same barrier-N + mpsc + per-config
   `SparseSharedConfig` shape.

## 2. Stage 1 — `worktree-shared` scenario (both strategies, both modes)

### 2.1 Goal

`--scenario worktree-shared --worktree-strategy {full|depth1}
--worktree-mode {pre-stage|on-demand} --concurrency N` works
end-to-end against `rust-lang/cargo`.

### 2.2 Concrete changes in `bench_mount.rs`

1. Extend `Scenario` with `WorktreeShared`. Parser support.
   `is_concurrent()` + `is_sparse()` updates as needed.
2. Add two new flags to `Args`:
   - `--worktree-strategy {full|depth1}` (default `depth1`)
   - `--worktree-mode {pre-stage|on-demand}` (default
     `pre-stage`)
3. New `bench_worktree_shared(&args)` driver, shape parallels
   `bench_sparse_shared`:
   - Setup window: `git clone <strategy>` into a shared dir.
     If mode = `pre-stage`: also run N × `git worktree add`
     into per-thread mountpoint dirs (in setup, not
     measurement window).
   - Measurement window: barrier-N threads. Each thread:
     - If mode = `on-demand`: `git worktree add` for its
       assigned worktree.
     - Run the script (ls + read every file in `--files`)
       against the worktree dir.
   - Collect per-thread durations, wall clock, failures.
   - `disk_bytes_of(shared_clone_dir)` — this captures
     `.git` + all worktree dirs (they're all under the same
     parent).
4. Returns a `SparseSharedConfig` (the existing per-config
   sample type from sparse-shared) — keeps the report shape
   compatible.
5. Report function: extend `print_sparse_shared_report` to also
   print the worktree row when run via the new scenario, OR
   add a dedicated `print_worktree_shared_report` if the
   formatting differs enough. Decide during implementation —
   probably a new function since the worktree report should
   also show strategy + mode columns.

### 2.3 Decision points

- **`worktree-full` clone might be slow.** Cargo's full clone
  is bigger than `--depth=1`. If iteration time blows up,
  document and proceed — that's reality, not a bug.
- **`worktree add` failure handling.** Treat as per-thread
  failure (existing `failures` counter); don't panic. If rate
  > 50 % at N=10, capture and flag in §6.1's stop-condition
  handling.
- **Cross-iteration cleanup.** Drop the whole shared clone
  dir via `DirGuard`. No `git worktree remove` calls.
- **Worktree mountpoint paths.** Worktrees live under the
  shared clone dir's parent (or as a sibling), not inside
  `.git`. Use the existing `make_temp` pattern for the
  shared-clone dir and the worktree dirs separately, with
  separate `DirGuard`s.

### 2.4 Verification

- `cargo build -p projgit-cli --example bench_mount --release`
  clean.
- `cargo clippy -p projgit-cli --example bench_mount --
  -D warnings` clean.
- Smoke at `--concurrency 4 --worktree-strategy depth1
  --worktree-mode pre-stage --iterations 1` on cargo; prints a
  sane report.
- Sanity: at N=4 `depth1 pre-stage`, per-agent script wall
  should be near zero (everything local, just FS reads).
  Setup wall should be roughly clone-time + 4 × small
  worktree-add cost.

### 2.5 Commit boundary

```
bench: add worktree-shared scenario (full + depth1, pre-stage + on-demand)
```

One commit — both strategies and both modes in one go, since
the orthogonal-flag shape means a single driver handles all
four cells.

If `worktree-full` turns out to need substantially different
plumbing from `--depth=1`, split into two commits (full first,
then depth1 + the abstraction lift). Decide during
implementation.

## 3. Stage 2 — capture matrix

### 3.1 Goal

Worktree-comparator results land in
[`../bench/baseline.md`](../bench/baseline.md) in a new
"Worktree comparator" section. The sparse-access section's
"What this shows" prose is updated to point at the new
section as the more honest comparison.

### 3.2 Concrete changes

1. Run the full matrix at N ∈ {4, 10}: 4 worktree cells per N
   (full × depth1 × pre-stage × on-demand) + projgit-shared
   for direct comparison + partial-cat-independent for
   continuity. Median of 3 iterations each.
2. Append a new section to `docs/bench/baseline.md`:
   - Header: `## Results — worktree comparator (`<target>` @ <ref>)`.
   - Methodology summary + cleaning-strategy note.
   - Per-N table: rows for each config; columns for setup,
     wall clock, per-thread p50, disk total, total time
     (setup + wall_clock), failures.
   - "What this shows" — written *after* running.
   - "Caveats specific to worktree comparator" — the
     containerization-not-benched note from design §6.4 lands
     here.
3. Update the existing "Results — sparse-access" section's
   "What this shows" with a forward pointer: the headline
   1.59× win there was vs the strawman; the steelman
   comparison is in the new section.
4. Update the top-of-file Scenarios list with a pointer to
   the new section.

### 3.3 Decision points

- **What if `worktree-depth1` pre-stage wins wall clock by a
  lot?** Capture honestly. Update the pitch language in the
  bench writeup to lead with disk + containerization. Don't
  paper over.
- **What if `worktree-full` is unusably slow (say, full clone
  takes > 30 s)?** Document the cargo full-clone size in the
  caveats; the result is still valid (it's what an operator
  who picked the wrong strategy would experience). Don't
  re-pick the strategy.
- **What if the disk numbers are within 2× across all
  configs?** Re-derive the calculation by hand; the
  prediction is ~10× and a 2× actual would mean something
  unexpected about cargo's working-tree size.

### 3.4 Commit boundary

```
bench: capture worktree-comparator results in baseline.md
```

## 4. Stage 3 — handoff bump + pitch reframing

### 4.1 Goal

`handoff.md` Done section gains a worktree-comparator bullet.
"What I'd do next" re-checks the queue. **Most importantly**:
update top-level pitch language (in handoff, and possibly
README) to reflect the steelman comparison's findings.

### 4.2 Concrete changes

1. Bump `Last updated`.
2. Add Done bullet for worktree comparator — what landed,
   what it measured, what it found.
3. Update the existing sparse-access Done bullet's framing:
   the 1.59× wall claim was vs the strawman; the steelman is
   X.YZ× (whatever it turns out to be).
4. Re-check next-up. Likely candidates after this:
   - projgitd Stage 5 polish (still next-up).
   - CI bench job (now has even more scenarios to protect).
   - README pitch-language update (a separate commit
     downstream — flag in the "what I'd do next" rather than
     do it here, since README updates can be sensitive).
5. Update session-state memory.

### 4.3 Commit boundary

```
docs(handoff): worktree comparator done; pitch reframed
```

## 5. Stop conditions

If any of these fire during a stage, **pause and update the
design doc before pressing on:**

- **Stage 1 — `git worktree add` fails under N=10 parallel
  contention with rate > 50 %.** That's itself a finding:
  worktrees aren't multi-agent-safe at this scale. Capture in
  Stage 2's results and update the design doc's §7 open
  question with the actual answer. Move on (the bench produces
  a valid finding even if a config doesn't complete).
- **Stage 2 — projgit-shared wins wall clock decisively
  (> 1.5×) against `worktree-depth1` pre-stage.** Surprising;
  contradicts the predicted shape. Investigate: is the bench
  measuring worktree-add cost wrong? Did we account for the
  parallel contention overhead?
- **Stage 2 — disk gap is narrower than 5× across all
  configs.** Means working-tree materialisation isn't
  dominating; possible if cargo's `.git` is much bigger than
  the prediction assumes. Re-derive by hand; either the bench
  is wrong or the disk pitch is more nuanced than claimed.

## 6. What this doc is not

- A schedule. No dates, no release commitments.
- A spec. The bench source carries its own doc comments; this
  doc captures the plan, not the final API.
- A binding promise about results. Stage 2 captures what runs.
- A commitment to README updates. Pitch-language updates are
  flagged in handoff next-up; they're a separate change with
  separate review.
