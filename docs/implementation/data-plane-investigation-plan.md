# Data-plane investigation + shallow partial clone — implementation plan

> Status: **living doc.** Tracks how the data-plane investigation
> and shallow partial-clone work get built. Updated as each stage
> lands or surfaces something that changes downstream stages.
>
> Last updated: 2026-06-04 (created; no stages started).
>
> Motivated by the worktree-comparator bench finding (see
> [`../bench/baseline.md`](../bench/baseline.md) §worktree-comparator):
> `projgit-shared` did not complete the N=4 cell on `rust-lang/rust`
> in 36 minutes. The user-context that makes this load-bearing:
> the actual target workload is a ~140 GB source repo, so the rust
> failure is at a small fraction of the real scale.

## 0. Why this doc exists

Two tracks of work, tightly coupled because they both target the
same root concern (projgit's data-plane scaling to big-history
repos):

- **Track A (shallow partial clone)** — add a `--depth=N` option
  to projgit's partial-clone helper and plumb it through the
  daemon and CLI as a flag. Mechanical; ~half session of code +
  tests. Independently valuable at 140 GB scale where partial-
  clone metadata itself is multi-GB on deep-history repos.
- **Track B (data-plane investigation)** — instrument the
  daemon's request handler with per-RPC timing and queue-depth
  tracking. Run a minimal repro on `rust-lang/rust` with the
  instrumentation, identify the bottleneck. Don't yet *fix* the
  bottleneck; the goal is to know what to build next.

These ship in parallel in one session because:

- They don't conflict in the code (different files; A in
  `projgit-core/src/clone.rs`, B in `projgit-daemon/src/server.rs`).
- A is uncertain to fix the rust bench (probably won't — per-blob
  promisor cost in isolation is already 0.45 s, the bench's
  problem is orchestration). But A ships regardless because of
  140 GB-scale benefits.
- B is the diagnostic that tells us what to do next, but the
  *next* work is out of scope for this session.

## 1. Pre-flight

Before writing code:

1. Re-read the worktree-comparator section in
   [`../bench/baseline.md`](../bench/baseline.md) — the "What
   this shows" finding (c) and its three likely causes are the
   investigation's hypothesis ranking.
2. Re-read the handoff §"What I'd do next" #1 — the data-plane
   investigation is now the top of queue.
3. Note the call sites of `CloneOptions::new(...)`:
   - `crates/projgit-daemon/src/server.rs:610` — `attach_source` (URL).
   - `crates/projgit-cli/src/main.rs:383` / `:638` — both `projgit mount` paths (URL source).
   - `crates/projgit-cli/examples/bench_mount.rs` (3 sites in bench code).
   - `crates/projgit-fuse/tests/mount_real_remote.rs` (4 sites in tests).
   - `crates/projgit-core/tests/fetcher.rs` (1 site).
   Only the first three need the depth flag wired in; the rest
   use defaults.

## 2. Stage 1 — shallow partial clone

### 2.1 Goal

`git clone --filter=blob:none --no-checkout --depth=1` (or
`--depth=N` for arbitrary N) is available as a flag on
`projgit mount` (URL sources) and `projgitd`. Stays opt-in;
default behaviour unchanged so existing tests don't shift and
`git log` / `blame` / history-walking workloads still work.

### 2.2 Concrete changes

1. **`CloneOptions`** gets a `depth: Option<u32>` field.
   - `None` = no `--depth` arg, full history.
   - `Some(N)` = pass `--depth=N` to git.
   - `CloneOptions::new` keeps defaulting to `None`.
   - Add a builder-style `with_depth(depth: u32) -> Self`
     method for ergonomic call-site construction.
2. **`partial_clone`** passes `--depth=N` when set.
3. **`DaemonConfig`** gets `cache_depth: Option<u32>`,
   defaulting to `None`. Propagated to `attach_source` via
   a new parameter or a `DaemonState` field.
4. **`attach_source`** uses the configured `cache_depth` when
   constructing `CloneOptions` for URL sources.
5. **`projgitd` CLI binary** gets `--depth N` flag (optional,
   `Option<u32>`), threaded into `DaemonConfig.cache_depth`.
6. **`projgit mount` CLI** gets `--depth N` flag (optional)
   applied to its `CloneOptions` for URL sources. Ignored for
   local sources (which don't clone). Also ignored when
   `--daemon-socket` is set (the daemon owns clone policy in
   sidecar mode).
7. **Documentation**: update `projgit mount --help` and
   `projgitd --help` to explain the tradeoff (no history
   operations on shallow clones).

### 2.3 Decision points

- **Default depth?** Stays `None` (full history). Defaulting
  to shallow would break existing tests' expectations and
  cargo-build-style workloads. Harbor flips the flag explicitly.
- **Validation?** Reject `--depth 0` (meaningless). Accept any
  positive integer. `--depth 1` is the expected case.
- **Combinability with `--filter`?** The default filter
  (`blob:none`) is compatible with `--depth=N`. Don't change
  filter behaviour.
- **Local-source paths**: leave alone. They open in place, no
  clone happens. Document that `--depth` is ignored.

### 2.4 Verification

- `cargo build` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace --all-targets` green.
- One new unit test that constructs a `CloneOptions` with
  `with_depth(1)` and verifies the produced command has
  `--depth=1`. Probably via a refactor that exposes the command
  arg list before spawn.
- Manual smoke: `projgit mount --depth 1 <url> <mp>` on a
  small public repo (rust-lang/log) succeeds and the
  resulting cache dir is smaller than without `--depth`.

### 2.5 Commit boundary

```
feat(core, cli, daemon): --depth=N option for partial clones
```

One commit covering the field addition + plumbing + tests +
help text. Small enough to land atomically.

## 3. Stage 2 — daemon data-plane instrumentation

### 3.1 Goal

Add per-RPC timing + queue-depth tracking to
`handle_connection` in the daemon so a future run on
`rust-lang/rust` produces a trace showing where time goes. The
goal is **diagnosis, not fix**.

### 3.2 Concrete changes

1. New module `projgit_daemon::trace` (or inline in
   `server.rs` if small) with:
   - `RpcTrace { rpc: String, queued: Instant, started: Instant,
     finished: Instant }` capturing recv → handler-start →
     handler-end times.
   - Per-RPC counter showing in-flight count when the request
     arrived (so we can spot "10 sidecars piled up on the
     daemon" patterns).
   - cat-file mutex wait time (the suspected #1 bottleneck).
2. Instrument `handle_connection` (or `handle_request`):
   - On request receive: record queue-time start.
   - When handler begins: record `started`.
   - When handler returns: record `finished`, emit a line.
3. Output format: structured single-line per-RPC entries, like:
   ```
   trace: rpc=Fetch oid=<short> queued_us=12 served_us=4523 inflight_at_recv=4 catfile_wait_us=4500
   ```
   Greppable / awk-able.
4. Gate behind a new `DaemonConfig` field (`trace: bool`) +
   `projgitd --trace` flag. Off by default — instrumentation
   shouldn't be in the hot path for normal runs.

### 3.3 Decision points

- **Output destination**: stderr (matches existing
  `projgitd: ...` messages). Keep simple; don't introduce a
  log framework just for this.
- **Cat-file mutex wait timing**: requires touching
  `GitCliFetcher::raw_fetch` to record the lock-acquisition
  time. Acceptable since it's behind the trace flag and the
  measurement IS the point. Alternatively, do this at the
  daemon's `hydrating.header(oid)` call site by wrapping the
  whole call — coarser but no `projgit-core` changes.
  **Prefer the coarser version for V1** (changes one crate, not
  two).
- **`PrefetchHeaders` granularity**: probably the most
  interesting RPC. Time it explicitly because it's the
  prefetch worker's path, suspected of cascading.
- **Atomic in-flight counter** in the daemon: `AtomicUsize`,
  incremented at handler entry, decremented at exit. Sample at
  request receive time. ~5 lines.

### 3.4 Verification

- Cargo build / clippy clean.
- Existing daemon tests stay green (trace is off by default).
- Smoke: `projgitd --trace --socket /tmp/projgitd.sock &` then
  `projgit attach status` produces a trace line for the
  `Status` RPC on the daemon's stderr.

### 3.5 Commit boundary

```
feat(daemon): per-RPC trace instrumentation behind --trace
```

One commit. Pure observability; no behavioural change when
`--trace` is off.

## 4. Stage 3 — minimal repro on `rust-lang/rust` + diagnose

### 4.1 Goal

Run a tiny `sparse-shared` cell on `rust-lang/rust` with the
trace flag on; capture trace output; identify which of the
three suspected causes is dominant (or surface a fourth).

### 4.2 Concrete steps

1. Make a fresh release build with shallow + trace landed.
2. Run a minimal repro:
   ```
   PROJGIT_NETWORK_TESTS=1 \
     cargo run -p projgit-cli --example bench_mount --release -- \
     --scenario sparse-shared --concurrency 2 --iterations 1 \
     --url https://github.com/rust-lang/rust --ref main \
     --files README.md,Cargo.toml,LICENSE-APACHE
   ```
   (Smaller N, fewer files, single iteration — minimum to
   produce the symptom.)
3. **Critical**: pass `--depth=1` via the bench (need to add
   a flag to the bench harness too, or hardcode it for this
   diagnostic step). Otherwise the partial clone alone is
   minutes of setup.
4. Bench harness needs to spin the daemon with `--trace` (or
   the in-thread daemon needs to take a `trace: true` config).
5. Capture stderr → analyse: which RPCs dominate? Is `inflight`
   growing? Is `catfile_wait_us` the bulk of `served_us`?

### 4.3 Decision points

- **If repro hangs with shallow + trace**: that's still a
  finding. Capture whatever trace data accumulated; the
  inflight counter alone tells us if requests are piling up.
- **If repro completes quickly with shallow but slow without**:
  shallow fixed it; cause was deep-history per-fetch
  negotiation. Document and recommend shallow as default for
  big repos.
- **If repro completes either way with this small N**: spin
  up to N=4 or N=10 to recreate the symptom. The original hang
  was at N=4.

### 4.4 Verification

This stage doesn't ship code; verification is "the diagnosis
is recorded in baseline.md with enough specificity that a
future fix has a target".

### 4.5 Commit boundary

If new bench flag for trace mode is added:
```
bench: add --trace flag to bench_mount for diagnostics
```
Otherwise no commit at this stage; the output is recorded in
Stage 4.

## 5. Stage 4 — capture findings + handoff bump

### 5.1 Goal

`baseline.md` gains a new section documenting the diagnostic
output and what it means for the data-plane roadmap. Handoff
updated with the new finding and re-prioritised next steps.

### 5.2 Concrete changes

1. Add `## Diagnostic — data-plane investigation
   (`rust-lang/rust` @ main)` to `baseline.md`.
   - The trace command + output snippet.
   - Identified bottleneck (whatever Stage 3 surfaced).
   - Implications for next steps (which fix is now justified).
2. Update the handoff:
   - Bump `Last updated`.
   - Add Done bullet for shallow + trace instrumentation.
   - Update next-up #1 from "investigation" to "implement
     fix X based on diagnosis" — concrete.

### 5.3 Commit boundary

```
docs(baseline, handoff): data-plane diagnosis + next-up update
```

## 6. Stop conditions

- **Stage 1 (shallow) — `cargo test --workspace --all-targets`
  goes red.** Means the depth plumbing broke an existing
  contract. Stop and fix before Stage 2.
- **Stage 2 (instrumentation) — adds measurable overhead even
  when `--trace` is off.** Defeats the "off by default" design.
  Fix the gating before Stage 3.
- **Stage 3 (repro) — even with shallow + trace, the rust
  bench still doesn't complete in 5 minutes.** The instrumented
  hang is itself a finding (we now have trace output of the
  hang). Capture and move to Stage 4.

## 7. What this doc is not

- A full fix for the rust-scale failure. Stages 1+2 ship
  diagnostic capability; Stage 3 produces a diagnosis; the
  *fix* (cat-file pool, prefetch throttling, whatever) is the
  *next* session's work informed by the diagnosis.
- A pitch-language update. That waits until the data-plane
  fix actually lands.
- A spec for shallow-as-default. Shallow stays a flag; only
  Harbor flips it on.
