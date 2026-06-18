# Session handoff — 2026-06-18: cat-file pool + the second bottleneck

> Scope: the cat-file pool implementation work (Stages 1–4 of
> [`../implementation/cat-file-pool-plan.md`](../implementation/cat-file-pool-plan.md))
> and the second data-plane bottleneck it uncovered. Project-wide
> state lives in
> [`../implementation/handoff.md`](../implementation/handoff.md);
> this file captures what happened in this session specifically so
> a future resume doesn't have to re-derive context from commit
> messages.
>
> Previous session-handoff:
> [`2026-06-04-bench-rounds-and-data-plane-diagnosis.md`](2026-06-04-bench-rounds-and-data-plane-diagnosis.md)
> (which diagnosed the rust-scale hang and queued the cat-file pool
> as the #1 fix; this doc executes that fix).

## Session arc

Picked up with the cat-file pool queued as the diagnosed fix for
the rust-lang/rust data-plane hang. The 2026-06-04 trace had
pinned the bottleneck on one `Mutex<BatchChild>` in the daemon's
`GitCliFetcher` serializing every sidecar's prefetch + on-demand
fetch. The plan predicted ~15 s → ~1–2 s once K parallel cat-file
children removed the head-of-line block.

The arc:

1. **Stage 1 — `BatchChildPool`.** Replaced the single
   `Mutex<Option<BatchChild>>` with a K-slot pool (round-robin
   try-lock acquire, lazy spawn, per-slot respawn on I/O
   failure). Behaviour-preserving at K=1. Regression tests for
   K=1 and K=4.
2. **Stage 2 — `--pool-size N` plumbing.** Wired `pool_size`
   through `DaemonConfig`, `projgitd --pool-size N`, and bench
   `--daemon-pool-size N`. Default `min(available_parallelism, 8)`.
   N=0 rejected at parse.
3. **Stage 3 — re-bench, and the surprise.** The first post-pool
   run **failed the plan's stop condition**: K=4 only moved the
   rust diagnostic from ~15.3 s to ~9.2 s (~1.65×, below the 2×
   bar). Rather than ship a partial win, added env-gated
   `PROJGIT_CATFILE_TRACE` instrumentation to `GitCliFetcher`
   (per-call `wait_us` + `work_us`) and found pool **wait** time
   was ~0 — the slots weren't contended. The serialization was
   somewhere else.
4. **Root cause #2.** `handle_fetch` and `handle_prefetch_headers`
   in the daemon held the `state.active` `MutexGuard` across the
   **entire** backend call. Only one data-plane RPC could be
   inside `repo.backend.*` at a time, so the cat-file pool's K
   slots were architecturally unreachable. Fixed by deriving
   `Clone` on `ActiveBackend` (both variants are `Arc`-wrapped, so
   cloning is a refcount bump), cloning the backend out of the
   critical section, and dropping the lock before the slow call.
5. **Stage 4 — close the loop.** Updated the plan doc, the
   project-wide handoff, the README pitch, and the baseline
   measurements with the honest post-fix numbers.

Net result: **the rust-scale hang is fixed** (~8.8× on the
diagnostic cell), **the fix required two changes not one** (the
pool plus the handler lock-release — neither alone clears the
stop condition), and the pitch is updated to match measured
reality rather than the pre-pool prediction.

## Commits landed (chronological, all on `main`, pushed)

| commit | what |
|---|---|
| `1b38499` | feat(core): BatchChildPool replaces single Mutex<BatchChild> |
| `bd19488` | feat(daemon, cli, bench): --pool-size N for the cat-file pool |
| `d3613e0` | fix(daemon,core,bench): clear post-pool serialization and recapture Stage 3 |
| `012a50c` | docs(handoff,plan,readme): close cat-file pool loop and reframe pitch |

HEAD = `012a50c` at end-of-session, pushed to `origin/main`.
Working tree clean.

## The headline finding (don't re-derive)

**There were two serialization points, not one.** The 2026-06-04
diagnostic correctly identified the cat-file `Mutex<BatchChild>`
as *a* bottleneck, but fixing it alone only bought ~1.65×. The
dominant remaining cost was the daemon's `state.active` mutex
held across the whole backend call in the two data-plane RPC
handlers. Both had to be fixed for the predicted shape.

Isolation table (rust-lang/rust `sparse-shared`, N=2, `--depth 1`,
3 files, 1 iteration — network-variance numbers, the *shape* is
the finding):

| configuration | wall | vs baseline |
|---|---:|---:|
| K=1, pre-pool (baseline) | 15,298 ms | 1.00× |
| K=4, pool only (handler mutex still held) | 9,242 ms | 1.65× |
| K=1, handler lock released (pool not the lever) | 9,709 ms | 1.58× |
| **K=4 + handler lock released** | **1,746 ms** | **8.77×** |

Read it as: the pool and the lock-release are **complementary**.
Either one alone tops out around ~1.6×; together they compound to
~8.8×. The pool removes cat-file head-of-line blocking; the
lock-release lets concurrent RPCs actually reach the pool.

K=10 was also measured (~9.0 s pool-only, before the lock fix) —
confirming more slots don't help while the handler mutex is the
real gate. After the lock fix, K=4 is sufficient; K wasn't the
constraint past ~4 at N=2.

## Post-fix trace shape (K=4 + lock released)

```
trace: rpc=Attach           served_us=1,200,081 inflight_at_recv=1
trace: rpc=Fetch            served_us=  421,569 inflight_at_recv=1 oid=ed35016e
trace: rpc=Fetch            served_us=  413,411 inflight_at_recv=3 oid=ed35016e
trace: rpc=Fetch            served_us=  451,699 inflight_at_recv=3 oid=67c7a9d6
trace: rpc=Fetch            served_us=  451,615 inflight_at_recv=4 oid=67c7a9d6
trace: rpc=Fetch            served_us=  862,380 inflight_at_recv=3 oid=1b5ec8b7
trace: rpc=Fetch            served_us=  862,353 inflight_at_recv=4 oid=1b5ec8b7
trace: rpc=Shutdown         served_us=       23 inflight_at_recv=3
trace: rpc=PrefetchHeaders  served_us=22,023,904 inflight_at_recv=2 n_oids=31
```

Versus the pre-pool trace (in baseline.md §Diagnostic), the
load-bearing differences:

- **On-demand `Fetch` blobs now serve in ~0.4–0.9 s each** (≈ the
  isolated per-blob promisor cost), not ~14.8 s of ~99 % mutex
  wait. The head-of-line block is gone.
- **The script's three reads finish in ~1.7 s total** because
  they no longer queue behind the `PrefetchHeaders` batch.
- **`PrefetchHeaders` is genuinely backgrounded.** It still takes
  ~22 s of wall (git's promisor does 31 lazy fetches sequentially
  inside one cat-file child — that's *within-batch* serialization,
  a separate follow-up), but nothing user-facing waits on it.
- **`Shutdown` is served in 23 µs while prefetch is still
  in-flight** — the daemon drains in-flight handlers without
  queueing new RPCs behind the slow batch.

## What also got measured

**Cargo `sparse-shared` N=10 refresh** (the headline-scale repo
from the sparse-access bench), with `--daemon-pool-size 4`:

| metric | pre-pool (2026-06-02) | post-pool (2026-06-18) |
|---|---:|---:|
| projgit-shared wall | 8,576 ms | 7,385 ms |
| partial-cat-independent wall | 13,611 ms | 8,500 ms |
| wall ratio (pci / pjs) | 1.59× | 1.15× |
| disk ratio | 9.98× | 9.98× |

Cargo improved only modestly (~1.16× on projgit-shared). The
pool helps less on small-history repos: cargo's 31-OID root tree
fans out fewer lazy fetches per batch, so the pre-pool
serialization was cheaper in absolute terms to begin with. The
comparator also got faster across the day (network variance),
which is why the *ratio* shrank even though projgit got faster.
The disk win is unchanged at ~10×.

## What this means for the pitch

The honest, workload-qualified framing (now in the README and
project-wide handoff):

- **Big-history repos at multi-agent scale** (rust-lang/rust): the
  fix closes the prior "doesn't complete / ~15 s" failure — the
  N=2 diagnostic cell now completes in ~1.7 s wall + ~3 s total.
  Strong wall **and** disk wins here.
- **Small-history repos** (cargo): projgit-shared still **loses**
  wall clock to `worktree-depth1 on-demand` (the steelman from
  the 2026-06-04 worktree comparator), while keeping the
  structural disk (~8–10×) and containerization-cleanness wins.
  The pool did **not** flip the cargo wall result.

So the pitch is *not* "matches worktree on wall everywhere" — it's
"wins on big-history multi-agent wall + disk, wins disk +
container-cleanliness everywhere, still behind worktree on
small-history wall". Captured that way deliberately; don't
over-claim it back.

## Tests / gating added this session

| file | tests | gating |
|---|---|---|
| [`../../crates/projgit-core/src/fetcher/git_cli.rs`](../../crates/projgit-core/src/fetcher/git_cli.rs) | `pool_k1_reuses_single_child_across_sequential_calls`, `pool_k4_dispatches_across_multiple_slots` | always-on |

`projgit-core` unit tests 53 → 55. `cargo test --workspace
--all-targets` green; `cargo clippy --workspace --all-targets --
-D warnings` clean. The existing
`batch_child_stays_alive_across_missing_queries` test was pinned
to K=1 so its single-slot assertion stays meaningful.

The 6 daemon-side `DaemonConfig` literal test sites
(`attach_smoke`, `server_smoke`, `fetch_smoke`, `mount_smoke`,
`sidecar_mount_smoke`, `daemon_fetcher_smoke`) gained
`pool_size: 1` so they keep exercising the single-child contract.

## Instrumentation left in the tree

`PROJGIT_CATFILE_TRACE=1` enables per-call cat-file timing on
stderr (`cattrace: op=… wait_us=… work_us=… [oid=…] [n_oids=…]`).
Env-gated via `OnceLock`, zero cost when off. Kept deliberately —
it's what disambiguated "pool contention" from "handler-mutex
serialization" this session, and the same question will recur for
the per-batch-coalescer follow-up. Lives in
[`../../crates/projgit-core/src/fetcher/git_cli.rs`](../../crates/projgit-core/src/fetcher/git_cli.rs)
alongside the daemon's existing `--trace` (per-RPC) instrumentation.

## Gotchas hit + worked around

1. **VS Code stale-buffer bug recurred on
   `crates/projgit-daemon/src/server.rs`** (as warned in
   `/memories/repo/session-state-2026-06-04.md`). Worked around by
   doing the server.rs edits via Python scripts that
   `assert old in src` for **all** patterns before writing
   anything (atomic all-or-nothing). The scripts live under
   `target/tmp/` (gitignored). Stages 2 and the Stage-3 lock-fix
   both used this path; no partial writes resulted.
2. **`available_parallelism().clamp(1, 8)`** — initial
   `min(8).max(1)` tripped clippy's `manual_clamp`. Use `.clamp()`.
3. **`comparison_to_empty`** — `v != ""` tripped clippy; use
   `!v.is_empty()`. (Caught only on the final all-targets clippy
   pass, not the per-crate build; always run the full clippy gate
   before committing.)
4. **Long network benches** ran 30–60 s each and repeatedly got
   moved to background terminals. Not a failure — just redirect to
   files and poll. The rust diagnostic with `--depth 1` is the
   cheap repro (~16 s pre-fix, ~3 s post-fix); use it, not the
   full matrix, for quick iteration.

## State at end-of-session

- **[`../bench/baseline.md`](../bench/baseline.md)** §Diagnostic
  gained a "Post-pool measurements (2026-06-18)" subsection: the
  isolation table, the post-fix trace, the cargo N=10 refresh, and
  a "where projgit-shared lands vs the worktree steelman" table.
- **[`../implementation/cat-file-pool-plan.md`](../implementation/cat-file-pool-plan.md)**
  Stages 1–3 marked implemented; §4.6 records the stop-condition
  miss + diagnosis + final numbers; §5.4 the pitch guardrail; §6
  the stop-condition status.
- **[`../implementation/handoff.md`](../implementation/handoff.md)**
  "Last updated" bumped; Done section gained the cat-file-pool +
  lock-release bullet; "What I'd do next" reprioritized (cat-file
  pool removed — done; per-batch PrefetchHeaders coalescer now #1).
- **[`../../README.md`](../../README.md)** Measured Behavior bullets
  updated with the post-pool rust result and the workload-qualified
  worktree-comparison caveat.

## Next up — for the next session

Per [`../implementation/handoff.md`](../implementation/handoff.md)
"What I'd do next":

1. **Per-batch Coalescer for `PrefetchHeaders`** (now #1). The
   pool + lock-release removed on-demand-fetch head-of-line
   blocking, but two sidecars firing near-identical
   `PrefetchHeaders(31 OIDs)` batches still each do the full 31
   lazy fetches. The trace this session still shows a ~22 s
   prefetch batch. Route `prefetch_headers` through the existing
   per-OID `Coalescer.do_or_join`, or add a set-keyed batch
   coalescer. `PROJGIT_CATFILE_TRACE` is the tool to verify it.
2. **`projgitd` Stage 5 — production polish.** systemd unit,
   restart policy, health checks, persistent daemon state,
   `tracing-subscriber` wiring.
3. **CI bench job (B3).** Guard the baseline tables now that the
   key bottleneck is fixed.
4. **Container deployment recipe doc.**
5. **Phase 3d production WinFsp** — only if Windows is back in
   scope.
6. **Optional bench follow-ups:** rust N=4 / N=10 post-pool
   matrix (only the N=2 diagnostic cell was run this session),
   higher-N worktree comparator, target-scale (~140 GB) workload.

**Still deferred / off the list** (recorded so they don't sneak
back): projgitd Stage 4 (T4 fd-passing) — see
`/memories/repo/projgitd-stage4-deferred.md`; `cargo build`-shaped
bench — off-target.

## Verifying it still works (sanity-check commands)

```sh
# Default suite + lint
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings

# --pool-size surfaces + N=0 rejected
./target/release/projgitd --help | grep -A2 -- "--pool-size"
./target/release/projgitd --pool-size 0 --socket /tmp/x.sock   # exits non-zero

# The cheap post-fix repro (~3 s; was ~16 s pre-fix)
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
    --scenario sparse-shared --concurrency 2 --iterations 1 \
    --daemon-depth 1 --daemon-trace --daemon-pool-size 4 \
    --url https://github.com/rust-lang/rust --ref main \
    --files README.md,Cargo.toml,LICENSE-APACHE
# Expect ~1.7 s projgit-shared wall; on-demand Fetch RPCs ~0.4-0.9 s each.

# Add PROJGIT_CATFILE_TRACE=1 to the above to see per-call
# wait_us/work_us if you're chasing the next bottleneck.
```
