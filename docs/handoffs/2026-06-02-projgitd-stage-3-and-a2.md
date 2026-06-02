# Session handoff — 2026-06-02: projgitd Stage 3, dotgit A2, Stage 4 deferred

> Scope: just the work done in one focused session on 2026-06-02.
> Project-wide state lives in
> [`../implementation/handoff.md`](../implementation/handoff.md);
> this file captures what happened in this session specifically,
> so a future resume (or a new reader skimming history) doesn't
> have to re-derive context from commit messages.

## Session arc

Picked up after the May 20 work (projgitd Stages 0-1-2 done, daemon
+ in-thread sidecar smokes green, no fd-passing yet). The big arc
today: **projgitd Stage 3 shipped end-to-end** (sidecar holds the
FUSE fd; daemon is pure data plane; failure-mode contract
verified), **dotgit A2 ref visibility landed** as a parallel
small win, and **Stage 4 was indefinitely deferred** once we
walked through Harbor's threat model and concluded the stop
condition was met. Closed with **Phase C design + plan docs**
so the next-up item (concurrent cold-fetch bench) is ready to
pick up cold.

Net result: the daemon-architecture backbone is structurally
complete for the announced Harbor workload, with measured tests
proving each layer and the next-step queue trimmed of speculative
multi-tenant work.

## Commits landed (chronological, all on `main`)

| commit | what |
|---|---|
| `a39111e` | feat(daemon): Attach/Fetch/PrefetchHeaders RPCs (Stage 3a) |
| `1a00f8b` | feat(daemon): DaemonFetcher -- sidecar-side Fetcher (Stage 3b) |
| `cf93cb8` | feat(cli): projgit mount --daemon-socket sidecar mode (Stage 3c) |
| `3bc1172` | test(daemon): cross-process + cross-namespace sidecar smokes (Stage 3d) |
| `686dd10` | test(core): de-flake dotgit_index temp paths via AtomicU64 counter |
| `dcfbf5b` | feat(core): A2 ref visibility for branch projections (dotgit) |
| `736f6d0` | feat(cli, daemon): wire A2 ref visibility + e2e test + docs |
| `7c673ae` | docs(projgitd): defer Stage 4 indefinitely; T1.5 is sufficient for Harbor |
| `b07e60f` | docs(handoffs): session handoff for 2026-06-02 (Stage 3, A2, Stage 4 deferral) |
| `89a4607` | docs(phase-c): design + implementation plan for concurrent cold-fetch bench |

HEAD = `89a4607` at end-of-session; 10 commits ahead of
the prior session's `origin/main` baseline (`6bcbeaf`), all
pushed.

## What each chunk proved / shipped

### projgitd Stage 3 — sidecar holds the FUSE fd (3a/3b/3c/3d)

Closes the design doc §3 failure-mode upgrade. Daemon is now a
pure data plane; sidecar holds `/dev/fuse` and runs the FUSE
protocol loop locally. Daemon crash degrades to brief cold-path
unavailability instead of killing every mount on the host.

- **3a — protocol + handlers** (`a39111e`). Three new RPCs:
  - `Attach { source } -> Attached { git_dir }` — idempotent;
    sidecar discovers the on-disk CAS path so it can open its
    own `ObjectStore` against the shared store.
  - `Fetch { oid } -> Ok | Err` — daemon hydrates the OID through
    its existing `HydratingObjectStore::header()` (works for any
    object kind, not just blobs) so the in-flight `Coalescer`
    dedupes concurrent fetches across N sidecars. This is the
    single-flight that closes audit A3.
  - `PrefetchHeaders { oids } -> HeaderProbes { probes }` — batch
    variant for the sidecar's T1 readdir-prefetch worker.
  - New error codes: `not_attached`, `bad_oid`, `fetch_failed`.
  - `HeaderProbeWire` as a Serialize-safe wire form of the
    existing `HeaderProbe` (which carries a non-Clone
    `FetcherError` payload).
  - 5 new protocol roundtrip unit tests + 6 always-on integration
    tests in `tests/fetch_smoke.rs`.

- **3b — `DaemonFetcher`** (`1a00f8b`). Implements
  `projgit_core::Fetcher` by per-call `UnixStream::connect`.
  Lives in `projgit-daemon::fetcher` (not `projgit-core`) to
  avoid a dep cycle. Translates daemon error codes back to
  `FetcherError::Backend` / `FetcherError::Transport`.
  - Per-call connect = reconnect-friendly by construction;
    daemon restart between calls is just the next connect
    finding a fresh listener.
  - 5 integration tests in `tests/daemon_fetcher_smoke.rs`
    including the daemon-crash -> `FetcherError::Transport`
    case that backs the failure-mode contract.

- **3c — CLI sidecar mode** (`cf93cb8`). New
  `projgit mount --daemon-socket <PATH>` flag. With it set,
  `cmd_mount_via_daemon` calls `Attach`, opens the local
  `ObjectStore`, builds `HydratingObjectStore<DaemonFetcher>`,
  and serves FUSE locally. No new binary -- one flag.
  Rejects `--offline` (would defeat the purpose);
  `--cache-dir` / `--remote` / `--fetcher` are ignored.
  - 3 FUSE-gated integration tests in
    `tests/sidecar_mount_smoke.rs`: serve-files,
    warm-reads-survive-daemon-shutdown (the load-bearing
    headline property), two-sidecars-share-one-daemon.

- **3d — cross-process + cross-namespace tests + docker recipe**
  (`3bc1172`). Test coverage beyond what 3a/3b/3c could exercise
  on their own:
  - `tests/xprocess_mount_smoke.rs` (2 tests, FUSE-gated) spawns
    the real `projgitd` and `projgit` binaries as separate OS
    processes. Includes `kill -9 projgitd` mid-mount + warm-read
    survival check at the OS-process level.
  - `tests/xns_mount_smoke.rs` (1 test, FUSE + userns-gated)
    runs the sidecar inside
    `unshare --user --map-root-user --mount --propagation=private`
    -- closest in-CI proxy for "daemon on host, sidecar in
    container" without docker. Probes for userns support and
    skips cleanly if disabled.
  - `scripts/docker-smoke/{run.sh, Dockerfile}` -- one-command
    recipe for testing the topology with real docker on a host
    (NOT this devcontainer). Builds release binaries, builds a
    `debian:bookworm-slim + fuse3 + git` image, starts daemon +
    sidecar containers, verifies via `docker exec sidecar`.

End-to-end manual smoke against `/workspaces/projgit` confirmed:
`projgitd --socket /tmp/projgitd.sock` + `projgit mount
--daemon-socket /tmp/projgitd.sock /workspaces/projgit /tmp/sc-mp
--ref main` serves the workspace; `.git/HEAD` shows the projected
commit OID; `projgit attach status` shows `mounts: 0` (confirming
sidecar owns the FUSE fd, not the daemon).

### test(core): de-flake dotgit_index (`686dd10`)

Pre-existing flaky test surfaced during Stage 3 verification: the
two temp-path constructors in `tests/dotgit_index.rs` used
`Instant::now().elapsed().as_nanos()` as the supposedly-unique
suffix. That returns ~0-100 ns (the instant is polled immediately
after construction), so parallel test threads collided on
`(pid, nanos)` and one test removed another's fixture mid-run.
~1-in-5 failure rate.

Fix: process-local `AtomicU64` counter (`next_unique_id()`).
Verified by running the binary 10 times in a row: 10/10 pass.

### dotgit A2 ref visibility (`dcfbf5b`, `736f6d0`)

A2 promoted from deferred to shipped-as-default. Independent of
A1+'s index axis per the §9.6 axis-split insight; ~150 LOC plus
tests.

- **Core mechanism** (`dcfbf5b`):
  - `apply_a2_ref_visibility(overlay, branch_full, oid)` mutates
    an A1 / A1+ overlay in place: symbolic `.git/HEAD` →
    `ref: refs/heads/<branch>\n` + loose ref file at
    `.git/refs/heads/<branch>` containing `<oid>\n`. Pure data
    mutation; no store dependency. Supports nested branch names
    (`feature/foo` creates the intermediate directory on the fly).
  - `ObjectStore::try_resolve_branch_full_name(refname)` does
    short-name → full-name resolution and gates by ref kind
    (returns `None` for tags, remote-tracking refs, non-existent
    refs, and `HEAD` itself).
  - 6 unit tests + 1 integration test for the resolver.

- **Wiring + e2e + docs** (`736f6d0`):
  - All three overlay-building call sites apply A2 when
    applicable: `projgit mount`, `projgit mount-multi`, daemon
    `Mount` handler. No new CLI flag — it's just "what
    `--no-dotgit` opts out of", same as A1+.
  - Live e2e test in `mount_real_remote.rs` against
    `rust-lang/log`: `git symbolic-ref HEAD` →
    `refs/heads/master`, `git branch --show-current` → `master`,
    `git rev-parse refs/heads/master` matches
    `git rev-parse HEAD`, A1+ clean-status preserved.
  - `docs/design/dotgit-synthesis.md` §9.7 promotion entry;
    handoff Done bullet; README updates.

Manual smoke against `/workspaces/projgit`: `cat .git/HEAD` shows
`ref: refs/heads/main`; `git branch --show-current` prints
`main`; `git rev-parse refs/heads/main` matches
`git rev-parse HEAD` (`686dd10`, the flake-fix commit from
earlier in the session).

### Stage 4 deferred indefinitely (`7c673ae`)

After walking through Harbor's threat model end-to-end:
T4's headline win (per-namespace isolation) protects against
multi-operator-on-one-host adversaries; Harbor is explicitly a
single-operator, shared-host, parallel-agents framework
(Scenario A in container-deployment.md §6). Cross-agent
isolation already comes from container mount-namespaces in T1.5;
host-shell isolation is a non-event because the operator IS the
host shell.

Decision recorded in:
- `docs/design/projgitd.md` §8 Stage 4 row (DEFERRED INDEFINITELY)
- `docs/implementation/projgitd-plan.md` §4 mirrors the decision
- `docs/implementation/handoff.md` "Explicitly off the actionable list"
- `/memories/repo/projgitd-stage4-deferred.md` (session-cross
  persistence so a future session doesn't re-propose it)

Spike + Stage 3 sidecar substrate stay in place — Stage 4 is
~1 session away if Harbor's deployment model ever shifts.

### Phase C design + plan docs (`89a4607`)

Landed after the initial handoff doc (this file's original
version) was committed -- captured here for completeness so the
session record stays whole.

Two planning docs, mirroring the projgitd design/plan split:

- **`docs/design/phase-c-bench.md`** (282 lines) -- the why:
  the question Phase C answers ("at concurrency N, how much
  does the daemon's in-flight fetch coalescer save vs N
  independent consumers racing to fetch the same blobs?"),
  the architectural property under test (audit A3 closure),
  methodology (two new scenarios on the existing bench harness),
  expected shape (clearly labeled as expectation not prediction;
  expect 5-10× daemon win at N=10), success criteria, risks
  (concurrent `git fetch` in the naive arm, in-thread vs
  subprocess daemon), open questions to settle by running.

- **`docs/implementation/phase-c-plan.md`** (353 lines) -- the
  how: 5 stages with per-stage commit boundaries and decision
  points. Stages 1-3 are bench code (refactor harness, add
  daemon-concurrent, add naive-concurrent comparator); Stage 4
  captures results in baseline.md; Stage 5 updates the handoff.
  Explicit **stop conditions** -- if daemon arm at N=1 is > 20%
  off baseline-single, or if naive arm deadlocks, or if N=10
  ratio is < 1.5×, pause and investigate before declaring Phase
  C done. Don't massage results.

Key design choices baked in:
- Two scenarios on existing `bench_mount.rs` (not new bench
  infra). Phase C is small (~half a session).
- `naive-concurrent` uses a SHARED on-disk cache dir -- the
  actual A3 scenario, not the safer "separate cache dirs"
  variant that measures a different (already-answered) property.
- In-thread daemon (matches `sidecar_mount_smoke.rs` pattern).
  Subprocess variant is a follow-up if numbers raise questions.
- Default matrix N ∈ {1, 4, 10}. N=100 (README headline) is
  bench-machine-dependent.

Nothing is committed to numerical predictions -- if results
contradict the expected shape, the design doc updates to record
actual findings rather than the bench getting tuned to match.

## Tests added this session

| file | tests | gating |
|---|---|---|
| [`crates/projgit-daemon/src/protocol.rs`](../../crates/projgit-daemon/src/protocol.rs) | 5 new roundtrip unit tests | always-on |
| [`crates/projgit-daemon/tests/fetch_smoke.rs`](../../crates/projgit-daemon/tests/fetch_smoke.rs) | 6 integration tests for the new RPCs | always-on |
| [`crates/projgit-daemon/tests/daemon_fetcher_smoke.rs`](../../crates/projgit-daemon/tests/daemon_fetcher_smoke.rs) | 5 integration tests for DaemonFetcher, including daemon-crash → Transport | always-on |
| [`crates/projgit-daemon/tests/sidecar_mount_smoke.rs`](../../crates/projgit-daemon/tests/sidecar_mount_smoke.rs) | 3 FUSE integration tests for the in-thread sidecar | `#[ignore]` (FUSE+git) |
| [`crates/projgit-daemon/tests/xprocess_mount_smoke.rs`](../../crates/projgit-daemon/tests/xprocess_mount_smoke.rs) | 2 cross-process tests using real binaries + SIGKILL | `#[ignore]` (FUSE+git) |
| [`crates/projgit-daemon/tests/xns_mount_smoke.rs`](../../crates/projgit-daemon/tests/xns_mount_smoke.rs) | 1 cross-mount-namespace test via `unshare` | `#[ignore]` (FUSE+git+userns) |
| [`crates/projgit-core/src/dotgit.rs`](../../crates/projgit-core/src/dotgit.rs) | 6 unit tests for `apply_a2_ref_visibility` | always-on |
| [`crates/projgit-core/tests/integration.rs`](../../crates/projgit-core/tests/integration.rs) | 1 integration test for `try_resolve_branch_full_name` | always-on (git fixture) |
| [`crates/projgit-fuse/tests/mount_real_remote.rs`](../../crates/projgit-fuse/tests/mount_real_remote.rs) | 1 network-gated A2 e2e test | `#[ignore]` (FUSE+network) |

Plus the dotgit_index de-flake (no new tests, just the
`next_unique_id()` helper).

**Total: 30 new tests** (17 always-on + 13 gated). `cargo test
--workspace --all-targets` stays green; `cargo clippy --workspace
--all-targets -- -D warnings` clean.

## Gotchas hit + worked around

1. **VS Code stale-buffer overwrite (recurring).** Same gotcha
   noted in the 2026-05-20 handoff: `replace_string_in_file` on
   `crates/projgit-daemon/src/server.rs` silently failed during
   the A2 wiring pass — the in-IDE buffer was stale and the
   edit appeared to apply (subsequent `grep_search` saw the
   change) but never reached disk (`cat | grep` showed nothing,
   `git status` showed the file as unmodified). Workaround:
   dropped to Python with `assert old in src` to force-apply
   the edit. Same fix as 2026-05-20.
2. **clippy `zombie_processes` lint.** The xprocess test's
   `spawn_*_process` helpers return `Child` ownership to the
   caller for the success path but kill-and-don't-wait on the
   panic path. clippy 1.95+ flags this. Fixed by adding
   `child.wait()` after `child.kill()` on the panic paths and
   `#[allow(clippy::zombie_processes)]` on the helper functions
   to document the explicit handling.
3. **clippy `items_after_test_module` lint.** Pre-existing from
   Stage 2c when `cmd_attach` landed after the `mod tests` block
   in `projgit-cli/src/main.rs`. Suppressed via `#[allow]` on
   the `mod tests` declaration rather than reordering the file.
4. **`CARGO_BIN_EXE_<name>` is per-crate.** Cross-process tests
   need both `projgit` (in projgit-cli) and `projgitd` (in
   projgit-daemon). `env!(CARGO_BIN_EXE_*)` only works for the
   same crate's binaries. Resolved via `current_exe()`'s profile
   dir grandparent + an `ensure_binaries_built()` helper that
   invokes `cargo build` on demand.
5. **Local-fixture daemon tests don't exercise the daemon's
   value-add.** Initial `two_sidecars_share_one_daemon` test
   asserted `daemon header_hits + header_misses >= 1`, expecting
   cross-process amortisation traffic. But for a local-source
   daemon with all objects on disk, the sidecar's `ObjectStore`
   reads via mmap'd packs without round-tripping the daemon (the
   §4.2 "bytes don't cross the socket" property in action). Test
   relaxed to just assert both sidecars serve content; cross-
   process amortisation measurement needs a partial-clone source
   and is Phase C's job.
6. **Flaky dotgit_index temp paths** (separate fix shipped this
   session). Documented above; `Instant::now().elapsed().as_nanos()`
   in temp-path naming is unsafe under parallel `cargo test`.

## Plan / design / memory state at end-of-session

- **[`../design/projgitd.md`](../design/projgitd.md)** — Stage 3
  row marked DONE 2026-06-02 with shipped surface + findings;
  Stage 4 row marked DEFERRED INDEFINITELY with decision
  rationale.
- **[`../design/dotgit-synthesis.md`](../design/dotgit-synthesis.md)**
  — new §9.7 entry for A2 promotion (parallel to §9.5 / §9.6
  for A1 / A1+).
- **[`../design/phase-c-bench.md`](../design/phase-c-bench.md)**
  — new design doc for the concurrent cold-fetch bench. Status:
  planned, not yet run; mental model going in is documented.
- **[`../implementation/projgitd-plan.md`](../implementation/projgitd-plan.md)**
  — Stage 3 marked DONE with full sub-stage breakdown; Stage 4
  marked DEFERRED INDEFINITELY.
- **[`../implementation/phase-c-plan.md`](../implementation/phase-c-plan.md)**
  — new 5-stage execution plan for Phase C; reads top-to-bottom
  as a checklist; explicit stop conditions and decision points
  per stage.
- **[`../implementation/handoff.md`](../implementation/handoff.md)**
  — Done section has fresh bullets for Stage 3 and A2; "What
  I'd do next" demotes Stage 5 to #2 (was #3), drops Stage 4
  from active list and adds it to "Explicitly off the actionable
  list".
- **`README.md`** — "What works inside a projgit mount" table
  gains the branch-aware row (`git branch --show-current`,
  `git symbolic-ref HEAD`); "Deliberately doesn't work" loses
  the A2 caveat, gains a note about commit/tag projections
  staying detached.
- **`/memories/repo/projgitd-stage4-deferred.md`** — persistent
  session-cross note: don't re-propose Stage 4.
- **`/memories/repo/test-flakes.md`** — deleted (the only entry
  was the dotgit_index flake; fixed this session).
- **`/memories/repo/audit.md`** — noted as referenced in the
  handoff but NOT currently present in this devcontainer's repo
  memory. Phase C plan calls this out (§6.2): A3 closure goes
  in the handoff text rather than the audit memory unless a
  future session re-creates the audit file.

## Next up

Per the updated handoff "What I'd do next" at end of session,
and with Phase C's design + plan now in place:

1. **Execute Phase C** — the design and implementation plan are
   already committed. The plan reads top-to-bottom as a 5-stage
   checklist:
     1. Refactor: extract a fetcher-factory in
        `projgit_mount_once` (~1 commit, pure refactor).
     2. Add `daemon-concurrent` scenario (~1 commit, new code).
     3. Add `naive-concurrent` comparator (~1 commit, new code).
     4. Capture results in `docs/bench/baseline.md` (~1 commit,
        prose + tables).
     5. Update handoff (~1 commit).
   Total expected: ~half a session, 4-5 commits. Start point:
   [`docs/implementation/phase-c-plan.md`](../implementation/phase-c-plan.md)
   §2.2 (the Stage 1 "concrete change" block).
2. **projgitd Stage 5 — production polish.** systemd unit + PID
   file, `tracing-subscriber` wiring for the existing `-v` flag,
   persistent daemon state for fast restart, health endpoints.
3. **CI bench job** (`.github/workflows/ci.yml`).
4. **Phase 3d Windows / `projgit-winfsp`.** Still the riskiest
   remaining piece; lowest priority given the Linux focus.
5. **Container deployment recipe doc.** Make
   `scripts/docker-smoke/` discoverable; build out the cookbook
   side of `docs/design/container-deployment.md`.

**Explicitly OFF the next-up list:**
- `projgitd` Stage 4 (T4 last mile) — deferred indefinitely per
  `/memories/repo/projgitd-stage4-deferred.md`.

## Verifying it still works (sanity-check commands)

```sh
# default test suite stays green
cargo test --workspace --all-targets

# clippy clean
cargo clippy --workspace --all-targets -- -D warnings

# Stage 3 FUSE-gated tests (need FUSE + git CLI):
cargo test -p projgit-daemon --test sidecar_mount_smoke -- --ignored
cargo test -p projgit-daemon --test xprocess_mount_smoke -- --ignored
cargo test -p projgit-daemon --test xns_mount_smoke -- --ignored

# A2 end-to-end against rust-lang/log (needs network + FUSE):
PROJGIT_NETWORK_TESTS=1 cargo test -p projgit-fuse --test mount_real_remote \
    mount_real_remote_with_dotgit_a2_shows_branch_name -- --ignored

# Manual sidecar topology smoke against this repo:
SOCK=/tmp/projgitd.sock; MP=/tmp/sc-mp
rm -f "$SOCK"; rm -rf "$MP"; mkdir -p "$MP"
cargo run --release --bin projgitd -- --socket "$SOCK" &
sleep 1
cargo run --release --bin projgit -- mount --daemon-socket "$SOCK" \
    /workspaces/projgit "$MP" --ref main &
sleep 2
cat "$MP/.git/HEAD"                       # ref: refs/heads/main  (A2)
git -C "$MP" branch --show-current        # main                  (A2)
git -C "$MP" status                       # clean working tree    (A1+)
cargo run --release --bin projgit -- attach --socket "$SOCK" status
# mounts: 0  (sidecar owns the fd, not the daemon)
kill %2; wait %2 2>/dev/null
cargo run --release --bin projgit -- attach --socket "$SOCK" shutdown
wait %1
```

## Docker-on-host recipe (for when you're outside this devcontainer)

```sh
# From the host (NOT inside the devcontainer; needs docker):
scripts/docker-smoke/run.sh                   # mounts this repo
scripts/docker-smoke/run.sh /path/to/repo     # mounts a different repo
```
