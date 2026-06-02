# Sparse-access bench — implementation plan

> Status: **living doc.** Tracks how the
> [sparse-access bench design](../design/sparse-access-bench.md)
> actually gets built. Updated as each stage lands or surfaces
> something that changes downstream stages.
>
> Last updated: 2026-06-02 (created; no stages started).
>
> Design in [`../design/sparse-access-bench.md`](../design/sparse-access-bench.md);
> this is one level down — concrete steps, file changes, commit
> boundaries, decision points.

## 0. Why this doc exists

Mirrors the Phase C plan pattern
([`phase-c-plan.md`](phase-c-plan.md)): design = what + why; plan
= how + what to learn from each step before the next.

Sparse-access is small (~half a session) because the bench
harness already exists post-Phase C: fetcher factory, in-thread
daemon plumbing, barrier-N-thread driver, concurrent report.
What's new: a script-style access pattern, a `cat-file --batch`
comparator, and a disk-bytes accounting helper.

## 1. Pre-flight (~5 min)

Before writing code:

1. Re-read [`../design/sparse-access-bench.md`](../design/sparse-access-bench.md)
   §3 (methodology) and §6 (risks).
2. Skim Phase C's
   `bench_projgit_daemon_concurrent` and
   `bench_projgit_naive_concurrent` in
   [`../../crates/projgit-cli/examples/bench_mount.rs`](../../crates/projgit-cli/examples/bench_mount.rs)
   — the new scenarios reuse the barrier-N-thread + mpsc pattern.
3. Decide the default target repo (§3.6 of the design). Mid-size
   public repo. `microsoft/typescript` is a candidate; pick at
   Stage 1.

## 2. Stage 1 — `sparse-single` + `cat-file --batch` baseline

### 2.1 Goal

`--scenario sparse-single` works end-to-end on the chosen target,
printing setup + script wall clock + disk bytes for three
configurations (`projgit`, `partial-cat`, `depth1`).

### 2.2 Concrete changes in `bench_mount.rs`

1. Extend `Scenario` enum with `SparseSingle`. Parser support.
2. New `SparseSample { configs: [PerConfig; 3] }` and
   `PerConfig { setup, script, disk_bytes }`.
3. New `bench_sparse_single(&args)` that runs all three
   configurations against fresh dirs and returns a `SparseSample`.
4. Per-configuration helpers:
   - `sparse_single_projgit(&args) -> PerConfig` — partial-clone
     into a fresh dir, mount FUSE, `ls` mount root, read every
     file in `args.files`, unmount.
   - `sparse_single_partial_cat(&args) -> PerConfig` — `git clone
     --filter=blob:none --no-checkout`, `git ls-tree <ref>` of the
     root, batch read every file in `args.files` via a long-lived
     `git cat-file --batch` child fed `<ref>:<path>` per line.
   - `sparse_single_depth1(&args) -> PerConfig` — `git clone
     --depth=1`, `ls`, plain `read_to_string` per file.
5. New `disk_bytes_of(path: &Path) -> u64` helper that recursively
   sums file sizes (Phase C used `du -s`; doing it in-process
   keeps the bench self-contained and works the same way for
   both configurations).
6. New `print_sparse_single_report(&args, &samples)` that prints
   a 3-row × {setup, script, disk_bytes} table.

### 2.3 Decision points

- **`git cat-file --batch` script shape.** Per file: write
  `<ref>:<path>` line, read header line (`<sha> blob <size>`),
  read `<size>` bytes of payload, read trailing newline. This is
  the protocol; a small helper isolates the parsing. Reuse
  `BatchChild`-style spawn from `git_cli.rs`? — no, that one
  speaks `--batch-check`. New helper for the bench.
- **What if the target ref has nested paths not in `args.files`?**
  Don't care; `args.files` is the script and the script doesn't
  enumerate.
- **Mountpoint dir cleanup.** Phase C's `DirGuard` already
  handles this. Reuse.

### 2.4 Verification

- `cargo build -p projgit-cli --example bench_mount` clean.
- `cargo clippy -p projgit-cli --example bench_mount -- -D warnings`
  clean.
- `PROJGIT_NETWORK_TESTS=1 cargo run … -- --scenario sparse-single`
  runs to completion. Numbers print. `projgit` and `partial-cat`
  setup numbers are roughly the same (both partial-clone).
- Sanity: with the default 3-file `args.files` against
  `rust-lang/log`, `projgit` and `partial-cat` script wall clocks
  should be within ~20 % of each other.

### 2.5 Commit boundary

```
bench: add sparse-single scenario (projgit vs partial-cat vs depth1)
```

One commit, all single-agent code. If the sanity check fails,
investigate before moving to Stage 2 (the multi-agent scenario
inherits this code).

## 3. Stage 2 — `sparse-shared` (N agents, overlapping blobs)

### 3.1 Goal

`--scenario sparse-shared --concurrency N` works end-to-end at
N ∈ {4, 10}, printing setup + script wall clock + disk bytes for
two configurations (`projgit-shared`, `partial-cat-independent`).

### 3.2 Concrete changes

1. Extend `Scenario` enum with `SparseShared`. Parser support.
2. New `SparseSharedSample` (similar shape to Phase C's
   `ConcurrentSample` but per-configuration).
3. New `bench_sparse_shared(&args)`:
   - `projgit-shared` arm: spawn daemon in-thread, Attach (the
     daemon does the one partial-clone), spawn N sidecar threads,
     each thread `ls`-and-reads its file list. Reuses the Phase C
     barrier-N-thread + mpsc pattern.
   - `partial-cat-independent` arm: spawn N threads. Each thread
     does `git clone --filter=blob:none --no-checkout` into its
     own dir, then reads via long-lived `git cat-file --batch`.
     No daemon, no sharing, no coalescing.
4. Per-thread file lists: by default each thread gets the full
   `args.files` (100 % overlap — the case the daemon's
   Coalescer is supposed to win). Optional follow-up: a
   `--overlap-ratio` knob to vary; default 100 % for now since
   that's where the pitch lives.
5. Report extends `print_sparse_single_report` shape; both arms
   reported with setup / script / disk_bytes per arm + ratios.

### 3.3 Decision points

- **Overlap = 100 % vs lower.** 100 % is the strongest case for
  the daemon's coalescer and the simplest to implement. Lower
  overlap is interesting but is Phase C2 territory; defer unless
  100 % shows no daemon win.
- **What if a thread's clone fails (rate limit, network blip)?**
  Count as failure (mirrors Phase C); don't panic.
- **In-thread daemon vs subprocess.** In-thread per Phase C's
  decision; subprocess deferred per [the assessment in this
  session](#).

### 3.4 Verification

- `cargo build` / `cargo clippy` clean.
- `--scenario sparse-shared --concurrency 4` runs to completion;
  printed table has both configurations with sensible numbers.
- Sanity: `projgit-shared` setup ≈ 1 × partial-clone;
  `partial-cat-independent` setup ≈ N × partial-clone.
- Sanity: `projgit-shared` disk_bytes ≈ 1 × CAS;
  `partial-cat-independent` disk_bytes ≈ N × CAS.

### 3.5 Commit boundary

```
bench: add sparse-shared scenario (N projgit sidecars vs N independent partial clones)
```

One commit. Closes the bench feature work.

## 4. Stage 3 — capture results

### 4.1 Goal

Sparse-access results land in
[`../bench/baseline.md`](../bench/baseline.md) in a new
"Sparse-access" section with the same structure as the Phase C
section.

### 4.2 Concrete changes

1. Run both scenarios median-of-3 against the chosen target.
   Default N ∈ {4, 10} for `sparse-shared`.
2. Append a new section to `docs/bench/baseline.md`:
   - `## Results — sparse-access (`<target>` @ <ref>)` header.
   - Reproduce block.
   - `sparse-single` table: setup / script / disk_bytes for the
     three configurations.
   - `sparse-shared` table per N: setup / script / disk_bytes
     per configuration + ratios.
   - "What this shows" prose summarising the result (written
     *after* running).
   - "Caveats" specific to sparse-access (target choice, no
     sparse-checkout comparator, etc.).
3. Update the top-of-file Scenarios list to point at the new
   section.

### 4.3 Decision points

- **What if `projgit-shared` doesn't win on wall clock?** Per
  the design's §7 open question, that's fine — disk-bytes savings
  alone justify the pitch — but capture the actual numbers and
  reframe the prose to lead with disk-bytes if needed.
- **What if `partial-cat` and `projgit` differ wildly on
  `sparse-single`?** Per design §4, that's a structural finding
  worth understanding. Investigate (instrument cold-cat path,
  count cat-file invocations) before declaring done.

### 4.4 Commit boundary

```
bench: capture sparse-access results in baseline.md
```

## 5. Stage 4 — update handoff

### 5.1 Goal

[`handoff.md`](handoff.md) Done section gains a sparse-access
bullet; "What I'd do next" re-checks the queue.

### 5.2 Concrete changes

1. Bump `Last updated`.
2. Add Done bullet for sparse-access — what landed, what it
   measured, what it found.
3. Re-check next-up. Likely candidates:
   - projgitd Stage 5 polish (still next-up).
   - CI bench job (now genuinely ready since the bench shape is
     more complete).
   - Higher-N or bigger-repo sparse-access follow-up (if results
     suggest it'd matter).
4. Update session-state memory.

### 5.3 Commit boundary

```
docs(handoff): sparse-access bench done; next-up re-checked
```

## 6. Stop conditions

If any of these fire during a stage, **pause and update the
design doc before pressing on:**

- **Stage 1 (`sparse-single`) — `projgit` and `partial-cat` differ
  by more than ~30 % on script wall clock.** Same mechanism;
  shouldn't differ. Likely either the comparator is mis-implemented
  (e.g., per-call `git cat-file` instead of long-lived `--batch`)
  or projgit has a per-blob path overhead we don't know about.
  Investigate before Stage 2.
- **Stage 2 (`sparse-shared`) — `projgit-shared` disk bytes don't
  collapse vs comparator.** The shared-CAS pitch is wrong or
  there's a bookkeeping bug. Investigate; don't move to Stage 3.
- **Stage 3 (capture) — all numbers tie within noise on every
  axis.** Either the target is too small or the script too thin
  to surface the structural shape. Re-pick the target (bigger
  repo, fewer or more files) before declaring done.

## 7. What this doc is not

- A schedule. No dates, no release commitments.
- A spec. The bench source carries its own doc comments.
- A binding promise about results. Stage 3 captures what runs.
- A user-facing roadmap. That's the handoff's "next-up".
