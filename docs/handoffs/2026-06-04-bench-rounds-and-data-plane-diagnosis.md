# Session handoff — 2026-06-02 → 2026-06-04: bench rounds + data-plane diagnosis

> Scope: just the perf/measurement work done across the
> 2026-06-02 → 2026-06-04 window. Project-wide state lives in
> [`../implementation/handoff.md`](../implementation/handoff.md);
> this file captures what happened in this session specifically,
> so a future resume (or a new reader skimming history) doesn't
> have to re-derive context from commit messages.
>
> Previous session-handoff:
> [`2026-06-02-projgitd-stage-3-and-a2.md`](2026-06-02-projgitd-stage-3-and-a2.md)
> (which itself extended an earlier handoff at the bottom with
> Phase C planning; this doc picks up from there).

## Session arc

Picked up right after Phase C had been planned but not run. The
job was "improve performance or demonstrate it". Three rounds of
benches each falsified or sharpened the previous round's claim,
ending with a diagnosed root cause for the rust-scale failure and
a concrete fix queued.

The arc:

1. **Phase C** ran the concurrent cold-fetch bench Phase C had
   planned. Found the daemon's in-flight coalescer doesn't
   deliver a wall-clock win at the scale we tested.
2. **Sparse-access** reframed the bench around projgit's actual
   target workload (sparse access, multi-agent shared CAS).
   Found projgit-shared wins 1.59× wall + 10× disk vs N
   independent partial clones at N=10 on cargo.
3. **Worktree comparator** replaced that strawman comparator
   with the steelman (`git worktree`). Found projgit *loses*
   wall clock 3.77× but still wins disk decisively — and
   crucially, **didn't complete on rust-lang/rust** in 36
   minutes.
4. **Data-plane investigation + shallow** shipped two parallel
   tracks: `--depth=N` shallow partial clone at every layer, and
   `--trace` per-RPC instrumentation in the daemon. Ran the
   diagnostic on rust with both enabled. Confirmed the
   bottleneck: per-mount prefetch worker × N sidecars × batched
   cat-file all serializing through one `Mutex<BatchChild>`.

Net result: **the pitch is now empirically honest at every
measured scale**, the rust-scale hang has a documented trace + a
diagnosed bottleneck + a specific fix queued, and two
independently useful capabilities shipped along the way (shallow
+ trace).

## Commits landed (chronological, all on `main`, pushed)

| commit | what |
|---|---|
| `273b3a7` | bench: extract a fetcher-factory in projgit_mount_once (Phase C stage 1) |
| `a7f140d` | bench: add daemon-concurrent scenario (Phase C stage 2) |
| `3d53d6c` | bench: add naive-concurrent comparator (Phase C stage 3) |
| `2bf9cb7` | bench: capture Phase C concurrent results in baseline.md (Phase C stage 4) |
| `4130e4c` | docs(handoff): Phase C done; Stage 5 promoted to next-up |
| `6bbf3ec` | docs(sparse-access): design + implementation plan |
| `bf92b8f` | bench: add sparse-single scenario (projgit vs partial-cat vs depth1) |
| `329957c` | bench: add sparse-shared scenario (N projgit sidecars vs N independent partial clones) |
| `0500558` | bench: capture sparse-access results in baseline.md |
| `b3b4e2d` | docs(handoff): sparse-access bench done; next-up re-checked |
| `77b1a97` | docs(worktree-comparator): design + implementation plan |
| `19a96ec` | bench: add worktree-shared scenario (full + depth1, pre-stage + on-demand) |
| `dee95df` | bench: capture worktree-comparator results in baseline.md |
| `8a8e47b` | docs(handoff): worktree comparator done; pitch reframed, data-plane investigation now top of queue |
| `dd5a88d` | docs(data-plane): plan for shallow partial-clone + daemon trace instrumentation |
| `4869594` | feat(core, cli, daemon): --depth=N option for partial clones |
| `ed7ad90` | feat(daemon): per-RPC trace instrumentation behind --trace |
| `028fb03` | docs(baseline, handoff): data-plane diagnosis + cat-file pool promoted to top of queue |

HEAD = `028fb03` at end-of-session, pushed to origin. 19 commits
total spanning the 2026-06-02 → 2026-06-04 window.

## What each chunk proved / shipped

### Phase C — concurrent cold-fetch bench (5 commits, 273b3a7 → 4130e4c)

Closes the design doc `docs/design/phase-c-bench.md` audit-A3
empirical question. Daemon's in-flight `Coalescer` does dedupe
concurrent same-OID requests; the question was whether that
shows up as a wall-clock win.

- **Stage 1** refactor: extract a fetcher-factory in
  `projgit_mount_once`. Bench harness change to support multiple
  fetcher backends from one driver. Pure refactor.
- **Stage 2** `daemon-concurrent` scenario: 1 in-thread projgitd
  + N sidecar threads holding DaemonFetcher. Coalescer is the
  property under test.
- **Stage 3** `naive-concurrent` comparator: N independent
  fetchers racing the same on-disk cache (the actual A3 scenario
  the daemon was built to fix).
- **Stage 4** results matrix N ∈ {1, 4, 10} on `rust-lang/log`:
  daemon-concurrent and naive-concurrent ratio at N=10 is
  **1.04×** (within noise). At 20-file/N=10 the daemon actually
  **loses by ~12%** because the single shared cat-file child
  serializes all sidecars' fetches. Stop-condition §7.3 fired;
  investigated rather than massaged.

Finding: **audit A3 is architecturally closed but empirically
neutral at this workload scale**. The daemon's Coalescer dedupes
N×fetches → 1 upstream fetch (mechanism correct), but in
overlapping-access workloads with one shared cat-file child the
serialisation cost cancels the dedup win. Recorded in
`docs/design/phase-c-bench.md` §4 (post-run annotation explains
the strawman model that informed the original §4 expectation).

### Sparse-access bench (5 commits, 6bbf3ec → b3b4e2d)

Reframed the bench around projgit's actual target workload
after recognising Phase C measured an artificial workload
(concurrent agents reading the same files; not realistic).

- New design + plan docs
  (`docs/design/sparse-access-bench.md`,
  `docs/implementation/sparse-access-plan.md`).
- New scenarios: `sparse-single` (one agent, three configs:
  projgit / partial-cat / depth1) and `sparse-shared` (N agents
  with 100% blob overlap, two configs: projgit-shared vs N
  independent partial clones).
- Results on `rust-lang/cargo` at N=10: projgit-shared wins
  **1.59× wall + ~10× disk** vs N independent partial clones.
- Single-agent surprise: `--depth=1` wins every axis on
  source-heavy repos like cargo because partial-clone metadata
  (~24 MB) exceeds a single-snapshot working tree (~22 MB);
  partial-clone disk savings only materialise when working tree
  bytes >> history bytes.

Finding: the daemon's empirical value is **clone amortisation**
across N agents, not fetch coalescing (Phase C tested the
latter; this tests the former). Reframed the pitch from
"100 agents fetching through one coalescer" to "100 agents
sharing one clone".

### Worktree comparator bench (4 commits, 77b1a97 → 8a8e47b)

Replaced sparse-access's strawman comparator (N independent
partial clones) with the steelman a competent operator would
actually reach for (`git worktree add`).

- New design + plan docs
  (`docs/design/worktree-comparator-bench.md`,
  `docs/implementation/worktree-comparator-plan.md`).
- New scenario `worktree-shared` with two orthogonal flags:
  `--worktree-strategy {full|depth1}` × `--worktree-mode
  {pre-stage|on-demand}`. Full matrix on cargo plus a 4-cell
  follow-up on rust-lang/rust.
- Cargo at N=10: `worktree-depth1 on-demand` total 3.0 s vs
  projgit-shared total 11.3 s — projgit **loses wall clock by
  3.77×**. Disk: projgit-shared 24.6 MB vs worktree 199 MB —
  projgit **wins by 8.09×**. The sparse-access "1.59× wall win"
  doesn't survive against the steelman.
- Rust-lang/rust: worktree-depth1 on-demand at N=10 completes
  in ~20 s with 2.47 GB disk. **`projgit-shared` killed at 36
  minutes** without completing the N=4 cell. Most important
  finding for the project's roadmap.

Finding: **wall-clock pitch is broken at every measured scale**
(strawman flip); **disk pitch is the only robust structural win**
(~6-11× across configs); **containerization-cleanness is the
architectural argument** (worktrees need two bind-mounts per
container + cross-tenant `.git` writeability hole, not benched
but documented in container-deployment.md §6). projgit's data
plane doesn't scale to rust-lang/rust in its current form —
that's the engineering work for the next session.

### Data-plane investigation + shallow partial clone (5 commits, dd5a88d → 028fb03)

Two parallel tracks in one session responding to the
worktree-comparator finding (c) (rust-scale hang).

- **Plan doc** (`dd5a88d`) covers both tracks:
  - Track A (shallow): `--depth=N` plumbing at every layer.
  - Track B (instrumentation): per-RPC trace in the daemon.
- **Shallow** (`4869594`): CloneOptions.depth field +
  with_depth builder; partial_clone passes --depth=N when set;
  DaemonConfig.cache_depth + projgitd --depth N + projgit mount
  --depth N + bench --daemon-depth N. 4 new unit tests covering
  default / depth=1 / arbitrary depth / panic on depth=0. 8
  existing DaemonConfig literal sites updated.
- **Trace** (`ed7ad90`): DaemonConfig.trace + DaemonState.inflight
  AtomicUsize + handle_connection instrumented. Output:
  ```
  trace: rpc=<name> served_us=<n> inflight_at_recv=<n>
         [oid=<8-hex>] [n_oids=<n>] [mp=<path>] [code=<err>]
  ```
  Off by default; `projgitd --trace` and `bench --daemon-trace`
  flag it on.
- **Diagnostic run + capture** (`028fb03`): minimal
  `sparse-shared` N=2 on rust-lang/rust with --depth=1 + --trace.
  Trace caught the hang cleanly:
  ```
  trace: rpc=PrefetchHeaders served_us=15,285,592 inflight_at_recv=3 n_oids=31
  trace: rpc=Fetch           served_us=14,833,042 inflight_at_recv=3 oid=67c7a9d6
  trace: rpc=PrefetchHeaders served_us=15,285,629 inflight_at_recv=4 n_oids=31
  ```
  Two sidecars' `PrefetchHeaders(31 OIDs)` calls each consume
  ~15 s of cat-file time; on-demand `Fetch` for `Cargo.toml`
  arrives in the middle and is head-of-line blocked behind the
  prefetch batch (14.8 s served, ~99% mutex wait).

Finding: **hypothesis (1) from the investigation plan confirmed**.
Per-mount prefetch worker × N sidecars × batched cat-file all
serialize through one `Mutex<BatchChild>` in `GitCliFetcher`.
**The cat-file pool — speculative through 2026-06-02 — is now
the diagnosed fix.** Cat-file pool plan committed:
[`../implementation/cat-file-pool-plan.md`](../implementation/cat-file-pool-plan.md).

Also found: the rust-lang/rust `sparse-shared` N=2 cell now
completes in ~16 s (vs >36 min before, killed) and
projgit-shared wins partial-cat-independent by 3.65× wall +
335× disk on this iteration. Shallow alone was a huge win;
the pool will compound it.

## Tests added this session

| file | tests | gating |
|---|---|---|
| [`crates/projgit-core/src/clone.rs`](../../crates/projgit-core/src/clone.rs) | 4 unit tests for CloneOptions::with_depth + build_clone_command | always-on |
| [`crates/projgit-cli/examples/bench_mount.rs`](../../crates/projgit-cli/examples/bench_mount.rs) | (5 new bench scenarios; not test-suite tests) | gated by `PROJGIT_NETWORK_TESTS=1` + `--release` |

**Total: 4 always-on unit tests** (49 → 53 in projgit-core).
The bigger surface addition is the 5 new bench scenarios
(`daemon-concurrent`, `naive-concurrent`, `sparse-single`,
`sparse-shared`, `worktree-shared`) and 4 new flags
(`--daemon-depth`, `--daemon-trace`, `--worktree-strategy`,
`--worktree-mode`).

`cargo test --workspace --all-targets` stays green;
`cargo clippy --workspace --all-targets -- -D warnings` clean.

## Gotchas hit + worked around

1. **VS Code stale-buffer bug recurred HARD** on
   `crates/projgit-daemon/src/server.rs` (and once on
   `docs/implementation/handoff.md`). Multiple
   `replace_string_in_file` / `multi_replace_string_in_file`
   calls reported success but didn't persist to disk. Lost ~20
   minutes of debugging time. Workaround documented in
   `/memories/repo/session-state-2026-06-04.md`: **drop to
   Python with explicit `assert old in src` + atomic
   all-or-nothing write** (`sys.exit` BEFORE writing if any edit
   fails; earlier "exit on failure" scripts wrote partial
   results and left the file inconsistent — the lesson is
   "validate all patterns first, then write once at the end").
2. **rust-lang/rust without `--depth=1` was way slower than
   predicted**. First attempt at the rust bench: full partial
   clone took >40s for setup alone; the bench then hung for
   >36 min before being killed. Initial diagnosis was "per-blob
   promisor cost is huge on deep-history repos" but a
   single-shot probe outside the bench showed ~0.45 s per blob.
   The actual cost was orchestration (mutex contention), not
   per-blob. The diagnostic Stage 3 captured this cleanly.
3. **Pre-existing `make_temp` was unsafe under parallel
   contention** in the bench. Fixed in the Phase C series by
   adding a process-local `AtomicU64` counter — same flake
   pattern as the dotgit_index fix from the prior session.
4. **Trying to do too many edits atomically with
   `multi_replace_string_in_file`** sometimes left half-applied
   states when one edit failed. Pattern: when editing
   `server.rs`, use Python scripts with explicit pre-checks for
   ALL patterns before any write.
5. **Pool of pre-existing rustfmt diffs in `bench_mount.rs`**
   carried forward from earlier sessions. Not introduced this
   session; leave alone unless deliberately doing a fmt pass.

## Plan / design / memory state at end-of-session

- **[`../bench/baseline.md`](../bench/baseline.md)** — gained 4
  new sections this session:
  - "Results — Phase C concurrent"
  - "Results — sparse-access (rust-lang/cargo @ master)"
  - "Results — worktree comparator (cargo + rust follow-up)"
  - "Diagnostic — data-plane investigation (rust-lang/rust @
    main, 2026-06-04)"
  - Plus updates to the top-of-file Scenarios list pointing at
    each new section, and an inline 2026-06-04 reframe note on
    the sparse-access section after the worktree comparator
    showed the 1.59× wall claim was vs a strawman.
- **[`../design/`](../design/)** — three new design docs (Phase
  C, sparse-access, worktree-comparator). Each is "the why"
  paired with an implementation plan doc. Phase C's design doc
  also has a 2026-06-02 post-run annotation correcting the
  expected-shape table.
- **[`../implementation/`](../implementation/)** — four new
  plan docs: `phase-c-plan.md`, `sparse-access-plan.md`,
  `worktree-comparator-plan.md`, `data-plane-investigation-plan.md`.
  Plus the new [`cat-file-pool-plan.md`](../implementation/cat-file-pool-plan.md)
  for next session.
- **[`../implementation/handoff.md`](../implementation/handoff.md)**
  — Done section gained 4 new bullets (one per bench round).
  "What I'd do next" rewritten end-to-end: cat-file pool now
  #1 (was speculative; diagnosed fix), per-batch Coalescer for
  PrefetchHeaders #2, projgitd Stage 5 demoted to #3.
- **`/memories/repo/session-state-2026-06-04.md`** — current
  session-state memory. Reflects all of the above.
  `/memories/repo/projgitd-stage4-deferred.md` — unchanged
  (still deferred).

## Next up — for the next session

[`../implementation/cat-file-pool-plan.md`](../implementation/cat-file-pool-plan.md)
is the load-bearing next-session plan. 4 stages with commit
boundaries:

1. **Stage 1** — `BatchChildPool` data structure inside
   `git_cli.rs`; `GitCliFetcher.batch` swaps from
   `Mutex<Option<BatchChild>>` to the pool. Behaviour-preserving
   at K=1. ~1 commit.
2. **Stage 2** — wire `pool_size` through `DaemonConfig` +
   `projgitd --pool-size N` + bench `--daemon-pool-size N`. ~1
   commit.
3. **Stage 3** — re-bench rust-lang/rust with the pool. Compare
   trace output before/after; confirm head-of-line block is
   gone; re-run cargo numbers too. ~1 commit (results capture).
4. **Stage 4** — handoff bump + pitch language update in
   README if post-pool numbers warrant. ~1 commit.

**Predicted outcome**: rust-lang/rust per-thread wall drops from
~15 s to ~1-2 s. If true, projgit-shared probably matches or
beats `worktree-depth1 on-demand` on cargo too. Pitch can
reframe to "matches worktree on wall clock plus wins 10× on
disk plus container-clean". If wall doesn't drop that much,
either K needs tuning or there's a second bottleneck the trace
will show.

**After the pool**:
- Per-batch Coalescer for `PrefetchHeaders` (smaller follow-up;
  trace showed two sidecars' `PrefetchHeaders(31 OIDs)` calls
  with fully overlapping OID sets each got a full mutex turn).
- projgitd Stage 5 (production polish).
- CI bench job.
- Phase 3d WinFsp.
- Container deployment recipe doc.
- 140 GB synthetic-bench / real-bench against the actual target
  workload scale.

**Off the actionable list** (recorded so they don't sneak back):
- projgitd Stage 4 (T4 last mile fd-passing). Still deferred per
  `/memories/repo/projgitd-stage4-deferred.md`.
- `cargo build`-shaped bench. Off-target — projgit is for sparse
  access, not dense.

## Verifying it still works (sanity-check commands)

```sh
# Default test suite stays green
cargo test --workspace --all-targets

# Clippy clean
cargo clippy --workspace --all-targets -- -D warnings

# Smoke the new shallow flag
./target/release/projgitd --help | grep -A2 -- "--depth"
./target/release/projgit mount --help | grep -A2 -- "--depth"

# Smoke the new trace flag
SOCK=/tmp/projgitd-smoke.sock
rm -f "$SOCK"
./target/release/projgitd --socket "$SOCK" --trace 2>&1 &
sleep 0.5
./target/release/projgit attach status --socket "$SOCK"
./target/release/projgit attach shutdown --socket "$SOCK"
# Expect stderr lines: "trace: rpc=Status ..." and "trace: rpc=Shutdown ..."

# Reproduce the data-plane diagnostic
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
    --scenario sparse-shared --concurrency 2 --iterations 1 \
    --daemon-depth 1 --daemon-trace \
    --url https://github.com/rust-lang/rust --ref main \
    --files README.md,Cargo.toml,LICENSE-APACHE
# Expect ~16s total wall; trace shows the two PrefetchHeaders(31)
# at ~15s each and the Fetch for Cargo.toml at ~15s wall but
# almost entirely mutex-wait. Confirms the bottleneck still
# present pre-cat-file-pool.

# Full bench matrix on cargo (existing; no behaviour change this session)
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
    --scenario worktree-shared --worktree-strategy depth1 \
    --worktree-mode on-demand --concurrency 10 --iterations 3 \
    --url https://github.com/rust-lang/cargo --ref master \
    --files "Cargo.toml,README.md,LICENSE-APACHE,LICENSE-MIT,\
CHANGELOG.md,CONTRIBUTING.md,src/cargo/lib.rs,src/cargo/macros.rs,\
Cargo.lock,build.rs"
# Expect ~3s total; projgit-shared cell still loses by 3.77x on cargo
# (pool fix is for the next session).
```
