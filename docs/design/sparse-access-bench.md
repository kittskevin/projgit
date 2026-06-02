# Design: sparse-access bench

> Status: **planned, not yet run.** The implementation plan with
> sub-stages and commit boundaries is in
> [`../implementation/sparse-access-plan.md`](../implementation/sparse-access-plan.md).
> Existing bench results live in
> [`../bench/baseline.md`](../bench/baseline.md) — `single`,
> `sequential`, and Phase C concurrent. This bench's results will
> land there too once run.
>
> Read alongside [`workload.md`](workload.md) §1 (sparse-access
> workload shape — the one projgit is actually for),
> [`phase-c-bench.md`](phase-c-bench.md) (the concurrent bench
> whose framework this reuses; sparse-access answers the question
> Phase C *didn't*), and
> [`projgitd.md`](projgitd.md) §1 (shared CAS as the daemon's
> empirical pitch).

## 0. Why this document exists

Phase C measured concurrent cold-fetch coalescing — found the
daemon's coalescer is architecturally real but not a wall-clock
win at the scale tested ([`../bench/baseline.md`](../bench/baseline.md)
§Phase C). The follow-up question, sharper than Phase C's: **on
the workload projgit was actually built for (sparse access to a
big repo, possibly by multiple agents), does projgit win and
against what alternative?**

The existing `single` / `sequential` benches use `rust-lang/log`
(small repo, full-tree workloads). They don't show projgit's
structural pitch because at that scale `git clone` finishes in
under 1 s. The pitch only matters when full-clone cost is
non-trivial *and* the agent only touches a small fraction of the
tree.

## 1. The question

**For a sparse-access agent on a moderately big repo, how does
projgit compare to the partial-clone-plus-`cat-file` alternative
that uses the same underlying mechanism, and to the
`clone --depth=1` alternative that materialises the working tree
up front?**

For N concurrent agents with overlapping blob sets: **does
projgit's shared CAS deliver a wall-clock and disk-bytes win vs
N independent partial clones?**

## 2. The architectural property under test

projgit is structurally a wrapper around `git clone
--filter=blob:none --no-checkout` plus on-demand lazy fetch via
`git cat-file --batch-check`. Its per-blob cost is therefore
structurally the same as that alternative — modulo one FUSE
syscall round-trip per read and (in sidecar mode) one daemon RPC.

Its three claimed wins over that alternative are:

1. **Shared CAS across N mounts.** N agents partial-cloning into
   the same on-disk store deduplicate blob storage and dedupe
   upstream fetches; N independent partial clones do not.
   Measured for the sequential case (mount 2 cold cat at ~1 ms
   in [`../bench/baseline.md`](../bench/baseline.md) §sequential);
   never measured for the concurrent multi-agent case.
2. **Zero-configuration sparse access.** `git clone
   --filter=blob:none --sparse` requires cone-spec setup per
   consumer; projgit just mounts and tools see paths naturally.
   This is a UX win, not a perf win, but it interacts with the
   measurement because the partial-clone-plus-`cat-file`
   comparator skips the sparse-checkout cone (otherwise the
   bench would be timing cone-spec config).
3. **Lower upfront cost than `--depth=1`.** A `--depth=1` clone
   materialises the working tree; projgit and `--filter=blob:none`
   only pull metadata. For big repos this is a multi-order-of-
   magnitude byte saving on the cold path.

This bench measures (1) and (3) directly. (2) is acknowledged but
not benched.

## 3. Methodology

### 3.1 Scenarios

Two new scenarios on `crates/projgit-cli/examples/bench_mount.rs`,
alongside `single` / `sequential` / `daemon-concurrent` /
`naive-concurrent`:

| Scenario | What it runs |
|---|---|
| `sparse-single` | Single-agent script (`ls` + read N files) against three configurations: projgit mount, partial-clone-+-`cat-file`, `--depth=1` clone-+-direct-read. |
| `sparse-shared` | N agents (default 4 and 10) each running the script with overlapping file sets, two configurations: N projgit sidecars sharing one daemon / one CAS, and N independent `--filter=blob:none` clones with their own scripts. |

### 3.2 The "script"

A small fixed access pattern that approximates a sparse-access
agent: list a directory, read a handful of named files. The exact
file list is `--files` (existing flag) extended to allow more
entries; the `ls`-of-a-dir part is fixed to the mountpoint root
(reuses the existing `read_dir_names` helper).

Deliberately *not* doing: recursive walks (`find -type f`), full
greps, or `cargo build`. Those are dense-access workloads where
projgit's per-syscall FUSE tax is known to lose; the bench's job
is to measure the workload projgit is *for*, not to re-demonstrate
the workload it's *not for*.

### 3.3 What's measured

For each (scenario, configuration, N) cell, per iteration:

1. Fresh temp dir(s).
2. Setup window: whatever clone(s) the configuration needs.
   Report as `setup`.
3. Measurement window: run the script(s) and time wall clock.
   For multi-agent: barrier-release all N, time from release to
   last join.
4. Post-measurement: record total bytes on disk for all cache /
   clone dirs combined.
5. Teardown.

Three numbers per cell:

- **`setup` time** (ms) — setup window only (clones, daemon
  startup, attaches).
- **`script` wall clock** (ms) — measurement window only. For
  multi-agent: barrier-release-to-last-join.
- **`disk_bytes`** — total bytes on disk across all cache dirs
  after the measurement window. For projgit-shared this is one
  CAS; for N-independent-clones this is the sum of N clone dirs.

Median of 3 iterations per cell.

### 3.4 Configurations

`sparse-single`:

| Config | Setup | Script |
|---|---|---|
| `projgit` | `partial_clone`, mount FUSE | `ls` + read N files via the mount |
| `partial-cat` | `git clone --filter=blob:none --no-checkout` | `ls-tree` + `cat-file blob` per file |
| `depth1` | `git clone --depth=1` (working tree materialised) | direct filesystem `ls` + read |

`sparse-shared`:

| Config | Setup | Script |
|---|---|---|
| `projgit-shared` | One in-thread daemon + one partial-clone (via daemon Attach) + N sidecar mounts | Each thread `ls` + read its file list |
| `partial-cat-independent` | N independent `--filter=blob:none` clones | Each thread `ls-tree` + `cat-file blob` per file in its own clone |

`depth1` deliberately omitted from `sparse-shared` — it's the
"every agent gets their own full clone" baseline, which is
operationally what Harbor doesn't want; the partial-cat-independent
configuration is the stricter shared-mechanism comparator.

### 3.5 N

Default matrix for `sparse-shared`: N ∈ {4, 10}. Same responsible
default as Phase C; the `--concurrency` flag accepts arbitrary N
for exploration.

### 3.6 Target repo

Bigger than `rust-lang/log` (~1 MB clone, used by the existing
benches) so the partial-clone-vs-full-clone byte gap is visible.
Default target picked at implementation time; candidates include
`rust-lang/cargo` (already in baseline.md, modest size),
`microsoft/typescript` (~50–100 MB partial), or a comparable
mid-size repo. The bench accepts `--url` so re-running against
different targets is one flag.

## 4. Expected shape

`sparse-single`:

- Setup time: `partial-cat` ≈ `projgit`'s setup (both partial-
  clone). `depth1` substantially slower for big repos (downloads
  the whole working tree).
- Script wall clock: `projgit` ≈ `partial-cat` (both do lazy fetch
  via cat-file). `depth1` is fastest because no fetch happens.
- Disk bytes: `depth1` >> `projgit` ≈ `partial-cat`. The win for
  projgit / partial-cat is `1 / (working-tree-size-over-touched-
  blobs-size)`.

If `projgit` and `partial-cat` differ by more than ~10–20 % on
either setup or script wall clock, that's a finding — same
mechanism shouldn't have wildly different cost.

`sparse-shared`:

- Setup: `projgit-shared` = one partial-clone + N mounts;
  `partial-cat-independent` = N partial-clones. Expect
  `projgit-shared` setup to be ≈ `1× partial-clone`, comparator
  setup to scale ≈ `N×`.
- Script wall clock: with overlapping blob sets, `projgit-shared`
  fetches each unique blob once across all sidecars (daemon
  Coalescer); comparator fetches each blob once per clone. Expect
  `projgit-shared` wall clock to be much smaller than comparator
  at higher N.
- Disk bytes: `projgit-shared` = one CAS; comparator = sum of N
  CASes. Expect `projgit-shared` disk bytes ≈ `1 / N` of
  comparator for fully-overlapping blob sets, scaling with overlap
  ratio for partial overlap.

If `projgit-shared` doesn't win on disk bytes by ~N×, the CAS
sharing isn't doing what it claims. If it doesn't win on wall
clock with overlapping blob sets, the daemon's Coalescer isn't
load-bearing for this workload either (consistent with Phase C's
finding) and the pitch's "shared cache" claim is purely a
disk-savings claim.

> Per Phase C's lesson, this is an *expectation* (mental model
> going in), not a *prediction*. Update with actual findings
> post-run.

## 5. Success criteria

Sparse-access bench is **shipped** when:

1. The two new scenarios (`sparse-single`, `sparse-shared`) are
   implemented in `bench_mount.rs` and work end-to-end against
   the chosen target.
2. Results captured in
   [`../bench/baseline.md`](../bench/baseline.md) in a new
   "Sparse-access" section with the same structure as the Phase C
   section.
3. [`../implementation/handoff.md`](../implementation/handoff.md)
   updated: sparse-access done; next-up re-checked.
4. The bench is reproducible — running it again should produce
   numbers of the same shape (not the same digits; bench variance
   documented).

What this bench is **not** trying to deliver:

- A `cargo build` comparison — dense-access workload, off-target
  per the workload doc.
- A monorepo-scale stress (10 k+ files, multi-GB clones) — useful
  follow-up, separate session.
- A Windows / WinFsp comparison — separate phase.
- Sparse-checkout cone-spec measurements — UX win is acknowledged
  in §2, not benched here.

## 6. Risks

### 6.1 Target choice

If the target repo is too small (like `log`), partial-clone
savings won't be visible and the bench reduces to a tie. If too
big (like full `linux-kernel`), iteration time blows up and the
median-of-3 protocol becomes painful. Pick something mid-size at
implementation time; document the choice in baseline.md.

### 6.2 The `partial-cat` comparator's `cat-file` invocations

Each per-file `git cat-file blob <oid>` spawns a `git` process.
That's not a fair comparison vs projgit's long-lived cat-file
child. Fix: use `git cat-file --batch` (long-lived) in the
comparator script too, matching projgit's strategy.

### 6.3 `disk_bytes` measurement

`du -s` of the cache dir(s) catches everything (pack + loose +
config + refs). For projgit-shared we want one number; for
N-independent we want a sum. Implementation should walk both with
the same accounting tool.

### 6.4 Network variance

Same as Phase C and all other network-gated benches. Median of 3
is the smoothing strategy; report the per-iteration range
alongside the median.

### 6.5 The sparse-shared scenario reuses Phase C plumbing

The daemon-startup + N-sidecar barrier + per-thread channel
pattern from `bench_projgit_daemon_concurrent` is the obvious
template. The comparator (`partial-cat-independent`) is new code.
Risk: the two implementations drift on shared concerns (timing
windows, failure counting). Mitigation: factor the common bits
out if duplication crosses ~50 LOC; otherwise live with copy-paste
as Phase C did.

## 7. Open questions to settle by running

- **Does `projgit-shared`'s shared-CAS win actually appear on
  wall clock, or only on disk bytes?** If only disk bytes, the
  daemon's Coalescer is again architecturally real and
  empirically a tie (consistent with Phase C). That's fine —
  disk savings are a legitimate win on their own — but it
  reframes the pitch as "shared storage" rather than "shared
  fetching".
- **Does `projgit` match or beat `partial-cat` in `sparse-single`?**
  Same mechanism; FUSE adds per-read overhead. If projgit is more
  than ~20 % slower at single-agent, that's structural and worth
  understanding (or accepting as the FUSE-overhead cost of admission).
- **Does `--depth=1` win `sparse-single` on wall clock by enough
  that someone might prefer it?** `--depth=1` pays full
  working-tree-bytes upfront and then has zero per-read cost.
  For very-sparse access (touch 3 files), projgit / partial-cat
  win. For moderately-sparse (touch 100 files), the crossover
  point matters and the bench should make it visible.

## 8. What this doc is not

- A prediction. §4 is mental-model framing; bench's job is to
  confirm or falsify.
- A spec of the bench code. That's [`../implementation/sparse-access-plan.md`](../implementation/sparse-access-plan.md).
- A binding commitment to follow-up work. If the bench surfaces
  a structural problem, that's a finding; what to do about it is
  a separate decision informed by this data.
- A monorepo-scale stress test. Mid-size target is enough to
  surface the structural shape; multi-GB targets are a separate
  bench-machine question.
