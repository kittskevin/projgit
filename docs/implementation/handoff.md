# projgit — Handoff

> **Internal status notes for contributors.** Not user documentation. If you
> landed here from outside the project, start with the [README](../../README.md)
> and [docs/problem-statement.md](../problem-statement.md) instead. This file is
> a running scratch pad for whoever is actively working on projgit; it's
> deliberately written in the second person and assumes you're about to edit
> code.
>
> Living document. Updated whenever we land a phase or change direction.
> Last updated: 2026-06-18, after cat-file-pool Stage 1-3 landed
> and prefetch coalescing shipped.
> Rust-scale data-plane bottleneck is now fixed in two parts:
> (1) `GitCliFetcher` moved from single-child cat-file to a
> K-slot `BatchChildPool`; (2) daemon RPC handlers release
> `state.active` before slow backend calls, so concurrent
> Fetch/PrefetchHeaders RPCs can actually reach the pool.
> On the rust diagnostic repro (`sparse-shared`, N=2,
> `--daemon-depth 1 --daemon-trace`): wall moved from ~15.3 s
> pre-pool to ~1.75 s post-fix (~8.8x). Cargo N=10 improved
> modestly (8.58 s -> 7.39 s wall). Full traces and tables are
> in [`../bench/baseline.md`](../bench/baseline.md) §Diagnostic
> "Post-pool measurements".
>
> Prior in the multi-day session: worktree-comparator bench
> (2026-06-04), sparse-access bench (2026-06-02), Phase C
> concurrent bench (2026-06-02), `projgitd` Stage 3, dotgit A2,
> Stage 4 indefinitely deferred. Long context; the
> [bench/baseline.md](../bench/baseline.md) sections are now
> the load-bearing artifact, not these commit lists.

If you're resuming work on this project after a break (or you're a fresh
AI session), read this file first. It's the shortest path back to context.
The deep references are linked; trust them, not memory.

## What is projgit?

An experimental user-mode filesystem in Rust. It exposes git objects from a
normal git repository as one or more **read-only projections** backed by a
single shared content-addressable store. The working mount backend is FUSE on
Linux/macOS; the production URL hydration path shells out to system `git` for
partial-clone promisor fetches; the WinFsp backend is planned but deferred.

Three projection kinds:
- **Ref** — `refs/heads/main` mounted as a directory tree.
- **Commit** — a specific commit OID mounted as a directory tree.
- **Subtree** — a sub-path inside either of the above.

The architectural killer feature: many simultaneous projections share
**one** object store, so blob deduplication is free.

Core decisions, design docs, and the phased plan all live in
[`docs/implementation/initial-plan.md`](initial-plan.md). Focused design docs go deeper on
specific subsystems: [`docs/design/fetchers.md`](../design/fetchers.md),
[`docs/design/windows-symlinks.md`](../design/windows-symlinks.md),
[`docs/design/dotgit-synthesis.md`](../design/dotgit-synthesis.md) (parent ladder),
and [`docs/design/dotgit-index.md`](../design/dotgit-index.md) (the A1+ rung
shipped 2026-05-18).

## Where we are right now

```
  HEAD    docs(coalescing): retract recommendation; reframe §7 #3 as structural   ← latest committed
23bffba  docs(readme): add 'What works inside a projgit mount' section
e9d5f45  feat(dotgit): synthesize a clean .git/index matching HEAD (A1+)
453f17d  docs(design): plan dotgit A1+ (clean read-only .git/index)
60699a4  feat: synthesize a default .git/ at the mount root (dotgit A1)
e127c5b  fix(fuse): echo request uid/gid as file ownership
825c8e8  docs: legitimize GvfsFetcher as the backend for Azure DevOps remotes
3b6e53d  fix(cli): normalize URL for cache-dir hashing; clarify SUPPORTED semantics
ab17659  docs: annotate problem-statement §7 with shipped status; reframe GVFS scope
6fbb6d9  docs(readme): clarify no support and not for production
5126a87  docs: move handoff and initial-plan into docs/implementation/
5e2699b  docs: restructure README and polish crate-level rustdoc
92e3ccb  docs(design): rename batch-fault.md to fetch-coalescing.md
6b6b496  docs(design): name the workload shape and explore batch-fault hydration
b6adab9  docs(handoff): refresh for GVFS, e2e mount test, and bench baseline
4547df0  fix(cli, core): finish GVFS plumbing missed by f62efe4
8fd7b19  docs(readme): clarify small-blob LRU as a cache in Measured Behavior
a564f2c  bench: reproducible mount benchmark vs. git partial-clone
a12eeb7  test(fuse): add network-gated end-to-end mount test against real remote
f62efe4  feat(fetcher): add optional GVFS backend
ff2138c  feat(core): prefetch headers after readdir
8bc74b8  feat(cli): --ref + --subtree resolution and --stats flag
5e1f518  feat(core): add small-blob LRU to ObjectStore
44c1f4b  perf(core): keep one git cat-file --batch-check child per fetcher
82d83f3  feat(core): add tree-listing LRU to ObjectStore
2d7f069  feat(cli): Phase 4 -- `projgit mount` end-to-end
58a9f06  feat(core): add GitCliFetcher (promisor-aware fallback)
9ab5449  feat(core): Phase 3e -- ProjectionFsProvider glue
ffd6ff6  feat(fuse): Phase 3b -- fuser backend, gated on Linux/macOS
0c53505  feat(core): Phase 3a -- FsProvider trait, InodeAllocator
1033f98  feat(core): Phase 2 -- Fetcher, single-flight, hydrating store
97a3b90  feat(core): Phase 1 -- ObjectStore, TreeNavigator, Projection
```

### Done
- **Phase 1 — Object store + projection engine.** Pure Rust, no FS, no
  network. 8 unit + 17 integration tests.
- **Phase 2 — Fetcher.** Sync trait, `GixFetcher` (validated end-to-end
  against GitHub), `NoopFetcher`, single-flight `Coalescer`,
  `HydratingObjectStore<F>`, `partial_clone` helper. 4 always-on +
  1 network-gated test.
- **Phase 3a — FsProvider foundation.** Trait, `Attr`, `DirEntry`,
  `FsError`, `InodeAllocator` (with synthetic-inode-bit reservation),
  `InMemoryFsProvider`. 9 unit tests.
- **Phase 3b — fuser backend.** `ProjgitFuse<F>` adapter +
  `mount(provider, mountpoint, config)` blocking helper +
  `mount_background(...)` returning a `BackgroundSession` (drop the
  handle to unmount). Cfg-gated to Linux/macOS; empty crate on
  Windows. Verified at runtime by
  `crates/projgit-fuse/tests/mount_smoke.rs` (run inside the
  devcontainer; see [`.devcontainer/README.md`](../../.devcontainer/README.md)).
- **Phase 3e — ProjectionFsProvider glue.** `ProjectionFsProvider<F>`
  in `projgit-core::projection_fs` bridges `Projection` +
  `HydratingObjectStore<F>` + `RootOverlay` to the `FsProvider` trait.
  Generic over `Fetcher`; per-inode `AttrSnapshot` cache populated
  lazily on `lookup`; gitlinks render as empty dirs; symlinks resolve
  via blob hydration; `mtime` stamped from `ObjectStore::commit_time`.
  `readdir` is deliberately blob-free: it emits `(inode, kind, name)`
  only and never hydrates to compute size, so `ls` of a partial-clone
  mount stays cheap. 13 integration tests + 1 commit_time test.
  Pure logic, no platform code.
- **Phase 4 — CLI.** `crates/projgit-cli` (`projgit` binary) ships
  one subcommand:
  `projgit mount <SOURCE> <MOUNTPOINT> [--ref|--commit|--subtree]
  [--cache-dir DIR] [--remote NAME] [--offline]`. URL sources are
  partial-cloned into `$XDG_CACHE_HOME/projgit/<basename-hash>` (or
  `--cache-dir`); local paths open in place. Uses
  `mount_background` + a Ctrl-C handler so dropping the session
  unmounts cleanly. Wires CLI → `partial_clone` → `ObjectStore` →
  fetcher (`GitCliFetcher` for URLs, `NoopFetcher` for `--offline` /
  local) → `HydratingObjectStore` → `ProjectionFsProvider` →
  `projgit_fuse::mount_background`. 4 unit tests; verified end-to-end
  in the devcontainer against `octocat/Hello-World` and
  `rust-lang/log`.
- **`GitCliFetcher` (production-default URL fetcher).** Drives the
  partial-clone *promisor* fetch path via a long-lived
  `git -C <git_dir> cat-file --batch-check` child: write the OID,
  read one status line, classify present / `missing` / unknown.
  The single child amortises fork/exec across an entire mount
  session and reuses the underlying TLS connection git keeps to the
  remote.
  Needed because GitHub's current policy rejects the bare-OID
  `allow-tip-sha1-in-want` requests `GixFetcher` issues for many
  repositories (server returns `RejectedSourceObjectNotFound`,
  `gix` reports `receive()` succeeded with an empty pack).
  `GixFetcher` stays around for environments without a system `git`,
  for benchmarks, and as the home for future native-Rust transport
  work. See `crates/projgit-core/src/fetcher/git_cli.rs` for the
  full rationale.
- **Phase 5a/5b — perf polish + nice-to-haves.** A small bundle of
  improvements that started after the Phase 4 demo highlighted
  per-`ls` cost on real repos:
  - **Tree-listing LRU.** `ObjectStore` memoises parsed
    `Vec<RawTreeEntry>` keyed by tree OID in a small bounded LRU
    (`tree_cache.rs`, default 256 entries). Implementation is a
    HashMap + BTreeMap reverse index for O(log n) eviction.
  - **Small-blob LRU.** `ObjectStore::read_blob` consults a
    byte-bounded LRU (`blob_cache.rs`, default 16 MiB total /
    64 KiB per entry; payloads above the per-entry cap are served
    fresh and skipped on insert). Mirrors the tree LRU's shape.
  - **Long-lived `git cat-file --batch-check`.** `GitCliFetcher`
    keeps one `git` child alive for the lifetime of the fetcher
    and shuttles OIDs over its stdin / stdout pipes. If the child
    dies it's torn down and respawned lazily on the next miss.
  - **`--ref + --subtree`.** Resolved against the open
    `ObjectStore` (peel ref → commit → `Subtree`). Removes the
    Phase 4 "requires --commit" sharp edge.
  - **Header LRU + T1 prefetch.** `ObjectStore::header` now has a
    bounded header cache. `ProjectionFsProvider::readdir` posts
    regular-file, executable-file, and symlink OIDs to a background
    prefetch worker, which batches header probes and warms the cache.
    See [`docs/design/prefetch.md`](../design/prefetch.md).
  - **Optional `GvfsFetcher`.** Behind the `gvfs-fetcher` feature,
    projgit can hydrate loose objects through GVFS `GET /gvfs/objects/{oid}`
    and warm blob sizes through `POST /gvfs/sizes`. CLI selection is explicit
    with `--fetcher gvfs --gvfs-url ...` (and optional `PROJGIT_GVFS_TOKEN`
    for bearer auth); default URL mounts still use `GitCliFetcher`. Always
    built in CI, never in the default feature set. See
    [`docs/design/fetchers.md`](../design/fetchers.md).
  - **`projgit mount --stats`.** On unmount, prints tree, header,
    blob, and T1 prefetch counters. The stats types are re-exported
    from `projgit_core` for future programmatic consumers.

  End-to-end on `rust-lang/log` inside the devcontainer:
  - Cold `ls -la src/` (5 files): ~1.5 s (network-bound; 5
    sequential HTTPS RTTs).
  - Warm `ls -la src/`: **~2 ms** (tree LRU + blobs already local).
  - Cold `ls -la tests/` (5 files, batch-check child alive): ~530 ms
    instead of 5 fork+exec + 5 TLS handshakes.
  - `pgrep` confirms exactly one persistent `git cat-file
    --batch-check` child for the whole mount.
- **Network-gated end-to-end mount test.**
  `crates/projgit-fuse/tests/mount_real_remote.rs` partial-clones
  `https://github.com/rust-lang/log`, mounts the projection through the real
  FUSE backend with `GitCliFetcher`, and walks `Cargo.toml`, `src/`, and
  `src/lib.rs` from the kernel side. Doubly gated by `#[ignore]` and
  `PROJGIT_NETWORK_TESTS=1` so the default workspace test run is unchanged.
  Runs in ~10 s.
- **Reproducible mount benchmark.**
  `crates/projgit-cli/examples/bench_mount.rs` times `readdir`, recursive
  walk, and 3-file reads cold and warm against a fresh
  `git clone --filter=blob:none --no-checkout` baseline. Two scenarios:
  `--scenario single` (one mount) and `--scenario sequential` (mount,
  unmount, fresh `ObjectStore` against the same cache dir). Two
  targets shipped: `rust-lang/log` and `rust-lang/cargo`. Headlines
  on `rust-lang/log` (median of 3, AMD Ryzen 7800X3D, WSL2):
  - `readdir` of root: **~15×** faster than `git ls-tree`, even cold.
  - Warm reads of 3 files: **~4,500×** faster than `git cat-file`.
  - **Cold first-read of 3 uncached files is currently ~2.8×
    *slower*** than `git cat-file` cold (~3.4 s vs ~1.2 s on `log`,
    ~6× / ~6.9 s vs ~1.1 s on `cargo`); `GitCliFetcher` hydrates
    one blob per fault and does not pipeline blob bytes the way
    native `git`'s promisor fetch does. Treated as structural per
    the fetch-coalescing retraction; the bench exists to catch if
    it changes.
  - **`--scenario sequential`: mount 2 cold cat is ~1 ms on both
    targets, a ~3,000–4,750× cross-mount speedup vs mount 1 cold.**
    Validates workload-doc §1.6 amortisation empirically for the
    first time.

  Methodology and full numbers in
  [`docs/bench/baseline.md`](../bench/baseline.md); README links the headline
  table.
- **License decision.** Project is dual-licensed **MIT OR Apache-2.0**.
  We hand-roll WinFsp FFI bindings (no GPL-3.0 `winfsp-rs`).
  See repo memory + [`docs/implementation/initial-plan.md` §10](initial-plan.md).
- **dotgit A1 synthesized at the mount root (2026-05-17).**
  `crate::dotgit::a1_overlay(commit_oid, objects_dir)` produces a
  minimal `.git/` (detached `HEAD`, `[core]` config, empty `refs/`,
  empty `packed-refs`, `objects/info/alternates` pointing at the
  shared store). `projgit mount` synthesizes it by default for `Ref`
  and `Commit` projections (`Subtree` and `--no-dotgit` opt out).
  Closes problem-statement §7 #4 (`git log <path>` works inside the
  mount). Network-gated e2e test
  `mount_real_remote_with_dotgit_supports_git_log` exercises it
  against `rust-lang/log`. Full rationale + the axis-split insight
  in [`docs/design/dotgit-synthesis.md`](../design/dotgit-synthesis.md) §9.5.
- **dotgit A1+ clean read-only index (2026-05-18).**
  `crate::dotgit::a1_plus_overlay(store, commit_oid, objects_dir)`
  wraps A1 and splices in a synthetic `.git/index` matching HEAD with
  every entry's `ASSUME_VALID` flag set. CLI defaults to A1+; same
  opt-outs. Before A1+, `git status` inside the mount showed 36 fake
  deletions and `git diff --cached` was 2,897 lines of phantom diff;
  after A1+, `git status` reports "nothing to commit, working tree
  clean" with zero user configuration. 5 unit tests in
  `crates/projgit-core/tests/dotgit_index.rs` + 1 network-gated e2e
  test. Full design in [`docs/design/dotgit-index.md`](../design/dotgit-index.md).
- **dotgit A2 ref visibility for branch projections (2026-06-02).**
  `crate::dotgit::apply_a2_ref_visibility(overlay, branch_full, oid)`
  mutates an A1 / A1+ overlay in place: symbolic `.git/HEAD` →
  `ref: refs/heads/<branch>\n` plus a loose ref file at
  `.git/refs/heads/<branch>` containing `<oid>\n`. Pure data
  mutation; no store dependency. Supports nested branch names
  (`feature/foo` creates the intermediate directory on the fly).
  `ObjectStore::try_resolve_branch_full_name(refname)` does
  short-name → full-name resolution and gates by ref kind
  (returns `None` for tags, remote-tracking refs, non-existent
  refs, and `HEAD` itself; those correctly stay on A1's detached
  HEAD). All three overlay-building call sites (`projgit mount`,
  `projgit mount-multi`, `projgitd` Mount handler) apply A2 when
  applicable; no separate CLI flag — it's just "what `--no-dotgit`
  opts out of". Inside the mount: `git symbolic-ref HEAD` →
  `refs/heads/<branch>`, `git branch --show-current` → `<branch>`
  (was empty under A1+), IDE branch indicators show the branch
  name instead of "detached HEAD". 6 unit tests in dotgit.rs
  + 1 resolver integration test + 1 network-gated e2e test
  (`mount_real_remote_with_dotgit_a2_shows_branch_name` against
  `rust-lang/log`). Manual smoke against `/workspaces/projgit`:
  `cat .git/HEAD` shows `ref: refs/heads/main`,
  `git rev-parse refs/heads/main` matches `git rev-parse HEAD`
  (`686dd10`, the de-flake commit). Full design in
  [`docs/design/dotgit-synthesis.md`](../design/dotgit-synthesis.md) §9.7.
- **FUSE adapter echoes the requesting process's uid/gid as file
  ownership (2026-05-17).** Previously every file in a mount showed
  as `root`-owned regardless of who was running `projgit`, which
  tripped git's `safe.directory` check for any non-root user. Now
  the FUSE adapter uses `req.uid()`/`req.gid()` per operation, so
  the mount looks owned by whoever's asking. Same shape as Gotcha
  #9 below — the WinFsp backend will need its own version of this
  for the same reason.
- **GvfsFetcher reframed as the Azure DevOps backend.** `Fetcher`
  trait is now explicitly multi-backend: `GitCliFetcher` (default)
  for stock Git remotes; `GvfsFetcher` for Azure DevOps Server /
  Azure Repos. Replaces the earlier "testbed for trait honesty"
  framing that didn't pass the workload-doc §6 discipline check.
- **URL cache canonicalisation.** `cache_subdir_for_url` now folds
  trivial URL variations (`https://x/y`, `https://x/y.git`,
  `https://x/y/`, case-only) into the same cache directory so the
  README's "one on-disk object store is shared across every mount"
  promise is actually delivered. HTTPS vs SSH stay deliberately
  distinct.
- **Fetch coalescing investigated, deprioritized.** Prior
  implementation attempt (not in repo) tried several strategies from
  [`docs/design/fetch-coalescing.md`](../design/fetch-coalescing.md) §6;
  none closed the cold 3-file `cat` gap, and the most aggressive
  variant destabilized the host badly enough to require a reboot.
  §9.5 of that doc captures the post-mortem and retracts §7's
  recommended build order. Cold path is now treated as structural
  (network-bound by design); problem-statement §7 #3 reframed as
  "Partially met (cold path is structural)" rather than a stepping
  stone to a met state.
- **Project audit + presentation pass (2026-05-17/18).** README
  restructured to lead with engineering before the prototype
  disclaimer; "What works inside a projgit mount" added as a
  consolidated reference; problem-statement §7 turned from a flat
  bullet list into a status table; handoff + initial-plan moved
  into `docs/implementation/`; README clarifies no support / not
  for production. `SUPPORTED` const semantics unified across the
  two backend crates.
- **`--allow-other` for container deployment (2026-05-18).** CLI
  flag `projgit mount --allow-other` passes the kernel `allow_other`
  FUSE mount option (via `MountConfig.acl = SessionACL::All`), which
  is the single blocker for every container topology: without it,
  even `root` gets `EACCES` because the kernel-level FUSE check
  fires before our adapter sees the request. Verified end-to-end
  in the devcontainer (cross-UID and cross-mount-namespace reads
  both work; `git log` succeeds inside the mount). Non-root users
  additionally need `user_allow_other` in `/etc/fuse.conf`; the
  flag's help text + the new design doc both call this out. Full
  topology framing, experiment log, and open follow-ups (attribute
  cache vs per-op uid echo; mount propagation) in
  [`docs/design/container-deployment.md`](../design/container-deployment.md).
- **Container T1 non-root user smoke test passed (2026-05-18).**
  `unshare --mount + mount --bind` (proxies for Docker `-v`) +
  `setpriv --reuid=5000` (proxies for a non-root container UID) +
  `git -C /mnt/repo log --oneline -n 3` all succeed with no
  `safe.directory` complaint. `.git/HEAD`, `.git/config`,
  `.git/index`, `.git/objects/` all appear owned by UID 5000
  because the per-op uid echo correctly populates the kernel attr
  cache for the in-container UID (which is the first reader of
  nested inodes in the canonical T1 deployment). Means the `--uid`
  flag the design doc hypothesized in §4 is **not needed** for the
  canonical T1 case. Full data in
  [`docs/design/container-deployment.md`](../design/container-deployment.md) §5.1.
- **`projgitd` daemon + sidecar topology designed (2026-05-20).**
  Architecture commitment for the next major piece of work: a
  per-host `projgitd` daemon owns the shared `ObjectStore` /
  `Fetcher` / in-memory caches / fetch coalescer; per-container
  sidecars hold the FUSE fd and run the protocol loop locally
  (so a daemon crash degrades to a brief cold-path outage instead
  of killing every mount); the agent is a pure read-only consumer.
  Closes audit A1 (no daemon) and A3 (cross-process single-flight
  gap) when shipped. Reuses the existing `Fetcher` trait as the
  daemon-sidecar wire (a new `DaemonFetcher` impl); bytes flow
  through the shared on-disk CAS via the OS page cache, not over
  the socket. Last-mile tenancy choice (T1.5 bind-subdirs vs T4
  per-namespace fd-passing) deliberately deferred — contained to
  one trait impl per Stage 4. Five-stage build plan, risk-ordered
  (Stage 0 spike to de-risk FUSE fd passing; Stage 1 multi-projection
  refactor; Stage 2 daemon scaffold; Stage 3 sidecar holds fd;
  Stage 4 T4 last mile; Stage 5 production polish). No code yet;
  full design in [`docs/design/projgitd.md`](../design/projgitd.md).

### Deferred / archived
- **`projgitd` Stage 0 — FUSE fd-passing spike landed GREEN (2026-05-20).**
  Throwaway code in [`spikes/fuse-fd-passing/`](../../spikes/fuse-fd-passing/README.md).
  Proved end-to-end that a process which did NOT open `/dev/fuse` and
  did NOT call `mount(2)` can fully serve the FUSE protocol on the
  resulting fd received via `SCM_RIGHTS`, using fuser's public
  `Session::from_fd`. Means **Stage 4 of the projgitd plan (T4
  last-mile via Harbor → sidecar fd handoff) is now green-lit**;
  Stages 1–3 proceed without modification. Findings include: mount
  decoupled from opener (mount lives in kernel namespace regardless
  of opener lifetime), `FUSE_INIT` is read-on-demand (opener must
  NOT wrap fd in fuser or it would consume INIT), clean teardown
  via external `fusermount3 -u`. Full writeup in the spike's README
  and in [`docs/design/projgitd.md`](../design/projgitd.md) §8.
- **`projgitd` Stage 1 — multi-projection in one process (2026-05-20).**
  `projgit mount-multi <SOURCE> --mount REF=PATH [--mount …]`
  hosts N projections in one process sharing one `Arc<ObjectStore>`,
  one `Arc<HydratingObjectStore<F>>`, one `Fetcher`. Each projection
  gets its own FUSE mount (Path B from the plan §1.3 — "many mounts
  shared store", chosen over the dispatcher-based "one mount many
  subdirs" because the substrate was already there and it matches
  Stage 3/4 naturally). Verified by `mount_multi.rs` integration
  test (isolation: each mount sees its own ref's content; sharing:
  mount B's read of the same OID hits the cache mount A populated)
  and CLI smoke run (two mounts of `main` at distinct paths; `--stats`
  shows shared tree/header/blob caches across both mounts).
  No changes to `projgit-core` or `projgit-fuse` needed — the
  substrate was already correct: `projection_id` plumbed through
  `InodeAllocator`, `ProjgitFuse<F: FsProvider>` generic, `Arc<ObjectStore>`
  shared-friendly. Means **§1.6-in-memory amortisation now works
  in process**; Stage 2 inherits it for free when wrapping the
  daemon. Full design decision + commit boundary in
  [`docs/implementation/projgitd-plan.md`](projgitd-plan.md) §1.3–1.6.
- **`projgitd` Stage 2 — daemon scaffold + Mount/Umount + client (2026-05-20).**
  New crate `projgit-daemon` (workspace member) plus a `projgit attach`
  client subcommand. The daemon binary `projgitd` listens on a unix
  socket, accepts JSON-framed control-plane RPCs (`ping`, `status`,
  `mount`, `umount`, `shutdown`), and hosts the FUSE mounts itself
  (the Stage 2 / T1.5 model; Stage 3 will move the FUSE fd to a
  per-container sidecar). V1 is **one source per daemon** — the first
  `Mount` request fixes the source, subsequent mounts must use the
  same source. All mounts share `Arc<ObjectStore>` and
  `Arc<HydratingObjectStore<F>>`, so the §1.6-in-memory amortisation
  from Stage 1 now also works over the wire (verified by the
  mount_smoke integration test: mount B’s read of the same OID
  produces a shared-cache `blob_cache` hit). Means audit **A1
  (no daemon)** and **A3 (cross-process single-flight gap) are now
  architecturally closed**; Phase C bench measures the actual gain
  (now measured — see the Phase C Done bullet below).
  Three commits (2a/2b/2c — f33601e, 8291a05, cb98d5a) shipped in
  one focused session, vs the plan’s two-to-three-session estimate.
  Full sub-stage notes in
  [`docs/implementation/projgitd-plan.md`](projgitd-plan.md) §2.
- **`projgitd` Stage 3 — sidecar holds the FUSE fd (2026-06-02).**
  Three new control-plane RPCs (`Attach { source } → Attached
  { git_dir }`, `Fetch { oid } → Ok | Err`, `PrefetchHeaders { oids }
  → HeaderProbes { probes }`) plus a new `DaemonFetcher` in
  `projgit-daemon::fetcher` that implements `projgit_core::Fetcher`
  by per-call connect to the daemon's unix socket. New
  `projgit mount --daemon-socket <PATH>` flag selects sidecar mode:
  the consumer process holds its own `/dev/fuse` fd, opens its own
  `ObjectStore` against the daemon's CAS (discovered via `Attach`),
  and runs the FUSE protocol loop locally; only cold-path object
  hydration goes over the wire (bytes don't — they flow through the
  shared on-disk CAS / OS page cache; see
  [`docs/design/projgitd.md`](../design/projgitd.md) §4.2/§5). The
  load-bearing failure-mode property is **verified by integration
  test**: with the daemon killed, the sidecar's mount keeps serving
  cached pages and `readdir` keeps working; only cold-path fetches
  fail (return `FetcherError::Transport`, surfaced as I/O error to
  the kernel). Three sub-commits (3a protocol + handlers, 3b
  DaemonFetcher, 3c CLI sidecar mode) shipped in one focused
  session. 17 new tests total:
  - **In-process / library-level** (always-on unless noted):
    `tests/fetch_smoke.rs` (6), `tests/daemon_fetcher_smoke.rs` (5),
    and `tests/sidecar_mount_smoke.rs` (3, FUSE-gated).
  - **Cross-process** (`tests/xprocess_mount_smoke.rs`, 2,
    FUSE-gated): spawns the real `projgitd` and `projgit` binaries
    as separate OS processes via `Command::new`; covers the actual
    binary lifecycle including `kill -9` of the daemon mid-mount.
  - **Cross-mount-namespace** (`tests/xns_mount_smoke.rs`, 1,
    FUSE+userns-gated): runs the sidecar inside
    `unshare --user --map-root-user --mount --propagation=private`,
    the closest in-CI proxy for "daemon on host, sidecar in
    container" without needing docker. Probes for unprivileged-userns
    support and skips with a clear message if disabled.
  End-to-end CLI smoke against `/workspaces/projgit` confirmed:
  `projgitd --socket …` plus `projgit mount --daemon-socket … --ref
  main` serves the workspace cleanly; `projgit attach … status`
  shows the daemon with `mounts: 0` (sidecar owns the fd, not the
  daemon). Full sub-stage notes in
  [`docs/implementation/projgitd-plan.md`](projgitd-plan.md) §3.
- **Phase C concurrent cold-fetch bench (2026-06-02).** Two new
  scenarios on `crates/projgit-cli/examples/bench_mount.rs`:
  `daemon-concurrent` (one in-thread `projgitd` + N sidecar threads
  holding `DaemonFetcher`) and `naive-concurrent` (no daemon; N
  independent `GitCliFetcher`s racing one shared `.git/objects/pack/`,
  the actual A3 scenario). Captured at N ∈ {1, 4, 10} on
  `rust-lang/log` plus a 20-file secondary probe at N=10. Full
  table + writeup in [`../bench/baseline.md`](../bench/baseline.md)
  §"Results — Phase C concurrent"; design retrospective in
  [`../design/phase-c-bench.md`](../design/phase-c-bench.md) §4.
  Audit A3 (cross-process single-flight gap) is **architecturally
  closed and empirically measured**: daemon's coalescer does dedupe
  N×duplicate upstream fetches down to N unique fetches, but at
  this workload scale that's not a wall-clock win — the headline
  ratio at N=10 is 1.04× (within noise), and at 20-file/N=10 the
  daemon actually loses by ~12% because it serialises N unique
  fetches through one shared `git cat-file --batch-check` child
  while the naive arm pipelines them across N parallel cat-file
  children + N parallel HTTPS connections. Naive arm doesn't fail
  at any tested N (git's promisor protocol handles concurrent
  lazy fetches into a shared pack dir more gracefully than design
  doc §6.1 suspected). The daemon's load-bearing wins for the
  target workload remain Stage 3's sidecar/FUSE-fd isolation and
  the persistent on-disk CAS measured in the sequential section
  (~3,000× sequential-mount amortisation). Five commits (Stage
  1 refactor, Stage 2 daemon arm, Stage 3 naive comparator, Stage
  4 result capture, this handoff update). Stop condition §7.3
  fired during Stage 4 (N=10 ratio < 1.5×); per plan instructions,
  investigated before declaring done rather than massaging numbers.
  Full per-stage detail in
  [`docs/implementation/phase-c-plan.md`](phase-c-plan.md).
- **Sparse-access bench (2026-06-02).** Two new scenarios on
  `crates/projgit-cli/examples/bench_mount.rs`: `sparse-single`
  (one agent, three configurations — projgit, `partial-cat`,
  `depth1`) and `sparse-shared` (N agents with 100 % blob
  overlap; projgit-shared vs N independent partial clones).
  Measures the workload projgit is actually for (sparse access
  on a moderately-sized repo, possibly by multiple agents),
  rather than cargo-build / recursive-walk shapes that were
  always off-target. Full table + writeup in
  [`../bench/baseline.md`](../bench/baseline.md) §sparse-access;
  design in
  [`../design/sparse-access-bench.md`](../design/sparse-access-bench.md);
  plan in
  [`sparse-access-plan.md`](sparse-access-plan.md).
  **Three findings:**
  (a) **multi-agent shared-CAS pitch validated** — at N=10 on
  cargo, projgit-shared wins **1.59× on wall clock and ~10× on
  disk** vs N independent partial clones; the crossover happens
  between N=4 (slight loss, 0.93×) and N=10 (decisive win), via
  amortising the ~3 s partial-clone cost once across N agents;
  (b) **single-agent surprise** — `depth1` (`--depth=1` clone
  + direct reads) wins every axis for source-heavy repos like
  cargo, because partial-clone metadata + lazy-fetched packs
  (~24 MB) exceeds a single-snapshot working tree (~22 MB);
  partial-clone disk savings only materialise when working tree
  bytes >> history bytes (large media / generated artifacts);
  (c) **reframing of the daemon's empirical value** — Phase C
  showed the daemon's fetch *coalescing* ties at N≤10;
  sparse-shared shows the daemon's clone *amortisation* wins
  decisively at N=10. The daemon's load-bearing value for the
  target workload is *eliminating per-agent setup redundancy*,
  not *coalescing per-agent fetches*. The pitch becomes "100
  agents sharing one clone" not "100 agents fetching through one
  coalescer". Four commits (Stage 1 sparse-single, Stage 2
  sparse-shared, Stage 3 capture, this handoff update).
  > **Update 2026-06-04:** finding (a) was vs a strawman
  > comparator. The worktree-comparator bench (next bullet)
  > replaced N independent partial clones with N `git worktree
  > add` and the wall-clock pitch flipped — projgit-shared
  > loses to `worktree-depth1 on-demand` by 3.77× at the same
  > N=10 cell. Disk pitch (~8×) still holds. Finding (c)'s
  > 'shared CAS amortisation wins' framing also needs updating:
  > it wins vs the strawman, not the steelman.
- **Worktree-comparator bench (2026-06-04).** New
  `worktree-shared` scenario on `bench_mount.rs` with two
  orthogonal flags (`--worktree-strategy {full|depth1}` and
  `--worktree-mode {pre-stage|on-demand}`) covering the four-way
  matrix of the steelman comparator a competent operator would
  reach for: one shared `git clone` + N `git worktree add`
  agents. Captured full matrix on `rust-lang/cargo` (median of
  3) plus a 4-cell follow-up on `rust-lang/rust` (1 iter each)
  to probe scaling. Full table + writeup in
  [`../bench/baseline.md`](../bench/baseline.md) §worktree-comparator;
  design in [`../design/worktree-comparator-bench.md`](../design/worktree-comparator-bench.md);
  plan in [`worktree-comparator-plan.md`](worktree-comparator-plan.md).
  **Four findings, in order of importance for the project:**
  (a) **the wall-clock pitch is broken at every scale measured.**
  On cargo at N=10, `projgit-shared` (~11.3 s) loses to
  `worktree-depth1 on-demand` (~3.0 s) by **3.77×**. The
  sparse-access '1.59× win' claim was vs N independent partial
  clones (a strawman); against the comparator any competent
  operator would deploy, projgit-shared loses.
  (b) **the disk pitch is the only robust structural win.** ~8×
  at cargo N=10; would have been ~6× at rust if the bench had
  completed. Holds across worktree strategies and modes because
  every worktree variant materialises N working trees while
  projgit holds metadata + only the touched blobs.
  (c) **projgit's data plane did not complete on `rust-lang/rust`.**
  The N=4 `projgit-shared` cell on `rust-lang/rust` ran for 36
  minutes without completing and was killed. At kill time the
  daemon's CAS had ~33 pack files (~417 MB of metadata +
  lazy-fetched blobs). Single isolated `cat-file --batch-check`
  promisor fetches outside the bench take ~0.45 s on the same
  repo, so per-blob promisor cost is **not** the bottleneck —
  the slowness lives in projgit's data-plane orchestration
  under load (likely the per-mount prefetch worker × N sidecars
  × batched cat-file calls all serializing through one
  `Mutex<BatchChild>`, but the precise cause wasn't isolated
  this session). **This is the most important finding for
  projgit's roadmap** and reshapes the next-up queue.
  (d) **`on-demand` beats `pre-stage` for worktrees at every
  cell.** Pre-stage's sequential `worktree add` loop scales
  linearly with N (cargo N=10: pre-stage 8.9 s vs on-demand
  3.0 s; rust N=10: 59.4 s vs 20.5 s). The 'operator
  pre-provisions a worktree pool' deployment shape is
  uniformly slower than letting agents spawn worktrees in
  parallel.

  **Pitch reframing the worktree-comparator forces:** projgit's
  load-bearing claim against worktrees is now disk efficiency
  (~6–11×) plus containerization-cleanness (worktrees need
  two bind-mounts per container; per-worktree state lives in
  the shared `.git`; cross-tenant `.git` writeability is a real
  hole — see [`../design/container-deployment.md`](../design/container-deployment.md) §6).
  The wall-clock pitch is gone at measured scales until the
  data-plane finding (c) is investigated and fixed. Four
  commits (Stage 0 design+plan, Stage 1 implementation, Stage
  2 capture, this handoff update).
- **Data-plane investigation + shallow partial clone (2026-06-04).**
  Shipped two parallel tracks responding to the worktree-comparator
  finding (c) (projgit-shared didn't complete on `rust-lang/rust`):
  shallow `--depth=N` support at every layer (Stage 1) plus daemon
  per-RPC `--trace` instrumentation (Stage 2), then ran a minimal
  diagnostic on `rust-lang/rust` with both enabled (Stage 3) and
  captured findings in [`../bench/baseline.md`](../bench/baseline.md)
  §Diagnostic. Design + plan in
  [`data-plane-investigation-plan.md`](data-plane-investigation-plan.md).
  **Three findings**:
  (a) **Diagnosis confirmed (hypothesis 1 from the plan).** The
  rust-scale hang is per-mount prefetch worker × N sidecars ×
  batched cat-file calls all serializing through one
  `Mutex<BatchChild>` in the daemon. Trace output shows two
  simultaneous `PrefetchHeaders(31 OIDs)` calls each consume
  ~15 s of cat-file time, and on-demand `Fetch` RPCs that
  arrive in the middle are head-of-line blocked behind the
  prefetch batches (one `Fetch` had 14.8 s wall time, of which
  ~99 % was mutex wait, ~1 % actual fetch work). Per-blob
  promisor cost in isolation is ~0.45 s on the same repo — the
  slowness is orchestration, not git.
  (b) **Shallow partial clone is independently load-bearing at
  big-history repos.** Without `--depth=1`, `Attach` on
  `rust-lang/rust` is >40 s (full partial clone of deep
  history). With `--depth=1`, 1.2 s. Disk dropped from ~417 MB
  to ~2.5 MB on the same iteration. Shipped as opt-in flag at
  every layer (`projgit mount --depth N`, `projgitd --depth N`,
  `bench_mount --daemon-depth N`); default unchanged because
  shallow projections can't serve `git log` / `git blame` /
  history navigation.
  (c) **The Coalescer works for `Fetch` but doesn't cover
  `PrefetchHeaders`.** Trace shows two sidecars asking for the
  same README blob produced one upstream fetch (Coalescer
  dedup), but two `PrefetchHeaders(31)` calls with fully
  overlapping OID sets each got a full mutex turn. Per-batch
  coalescing is a smaller follow-up fix.

  **Combined wins from this session**: with shallow + the
  existing architecture, the rust-lang/rust `sparse-shared`
  N=2 cell now completes in ~16 s and projgit-shared beats
  `partial-cat-independent` by **3.65× wall** + **335× disk**
  (the worktree-comparator's strawman comparator). The
  cat-file pool fix should drop wall further by ~10× (per-blob
  cost would dominate instead of mutex wait).

  Five commits (Stage 0 design+plan, Stage 1 shallow, Stage 2
  trace, Stage 3 bench `--daemon-depth`+`--daemon-trace` flags,
  this Stage 4 capture+handoff). 4 new clone unit tests; full
  workspace `cargo test --all-targets` + clippy green.
- **Cat-file pool + handler lock-release (2026-06-18).**
  Completed Stage 1-3 of
  [`cat-file-pool-plan.md`](cat-file-pool-plan.md):
  - `GitCliFetcher` now uses `BatchChildPool` (K slots,
    round-robin acquire, lazy spawn, per-slot respawn on I/O
    failure); regression tests cover K=1 and K=4.
  - daemon/CLI/bench wiring: `pool_size` in `DaemonConfig`,
    `projgitd --pool-size N`, and bench `--daemon-pool-size N`
    (rejects N=0).
  - Stage 3 initially hit stop condition (<2x wall gain).
    Root cause was a second serialization point:
    `state.active` mutex held across backend calls in
    `handle_fetch` / `handle_prefetch_headers`.
    Fix clones `ActiveBackend` out of lock and releases the
    mutex before executing backend work.
  - Final rust diagnostic shape: ~15.3 s -> ~1.75 s wall;
    on-demand Fetch RPCs return in ~0.4-0.9 s while prefetch
    continues in background.
  - Cargo sparse-shared N=10 refresh: projgit-shared 7.39 s
    wall vs 8.50 s comparator (1.15x wall, 9.98x disk).
- **Prefetch coalescing (2026-06-18).** Closed the duplicate-
  prefetch finding the post-pool trace surfaced: N daemon
  sidecars prefetching the same commit's root tree each drove a
  full 31-OID `PrefetchHeaders` batch (62 upstream fetches where
  31 would do). `GitCliFetcher` now holds a `PrefetchClaims`
  set (non-blocking per-OID single-flight, separate from the
  `fetch_object` coalescer so on-demand reads never block on a
  prefetch batch). `prefetch_headers` claims its missing OIDs;
  the lead caller batches them, peers `skip` and return
  best-effort probes. Verified on the rust N=2 diagnostic: one
  sidecar `lead=31 skipped=0`, the other `lead=0 skipped=31`
  (zero duplicate fetches), wall ~0.9-1.2 s (from ~1.75 s
  pool-only). Design in
  [`../design/prefetch-coalescing.md`](../design/prefetch-coalescing.md);
  2 new always-on unit tests (57 projgit-core total).
  crates were removed from the public repo surface; the useful findings
  are preserved in
  [`docs/design/winfsp-implementation-plan.md`](../design/winfsp-implementation-plan.md).
  The next Windows step is to implement `projgit-winfsp` directly using
  the WinFsp `FspService*` lifecycle from the C samples.

### Not yet started
- **Phase 3d — Production `projgit-winfsp`.** Not started. Includes:
  `FspService*` lifecycle, the symlink classifier per
  [`docs/design/windows-symlinks.md`](../design/windows-symlinks.md),
  per-user volume ownership, and a WinFsp adapter over
  `ProjectionFsProvider` (3e). The CLI remains cfg-gated to
  Linux/macOS; the Windows arm reports that support is deferred.
- **Phase 5 — remaining polish.** With Phases 5a/5b in (tree LRU +
  blob LRU + batched `cat-file` + `--stats` + `--ref + --subtree`),
  the remaining items here are convenience flags that don't block
  the Windows backend: a `projgit mount --background` PID-file
  flow with a `projgit umount` companion, and `tracing-subscriber`
  wiring for the existing `-v` flag.

### Phase 3e gotchas worth knowing for callers
- `ProjectionFsProvider::new` resolves the projection's commit OID
  up front and seeds `commit_time`. For `Projection::Ref` this means
  ref tip is captured at construction; ref-refresh is post-MVP.
- `getattr` on an inode the provider has never seen returns
  `NotFound`. Callers must `lookup` first (matches FUSE/WinFsp
  contract); the cache shape (`AttrSnapshot`) deliberately does not
  carry enough info to re-resolve a path from an inode alone.
- Gitlinks render as empty directories. The provider does **not**
  attempt to resolve the submodule's commit OID against this store
  (it would be missing or point into another repo).
- Symlink targets live in blob bytes; `readlink` triggers a
  `HydratingObjectStore::read_blob` on the symlink's OID.

Pack proliferation (many tiny packs from per-blob fetches) needs a
background-repack policy somewhere in here; currently noted as an open
TODO.

## Repo layout

```
e:\repos\gitfs\
├── README.md                        public overview + quick start
├── Cargo.toml                       workspace root, MSRV 1.85
├── LICENSE-MIT, LICENSE-APACHE      dual license
├── .devcontainer/                   Linux + FUSE dev environment
│   ├── devcontainer.json            base image, runArgs, mounts
│   └── README.md                    how to use it; FUSE quirks
├── crates/
│   ├── projgit-core/                Phase 1+2+3a+3e -- engine + GitCliFetcher
│   ├── projgit-cli/                 Phase 4 -- `projgit mount`
│   ├── projgit-fuse/                Phase 3b -- empty on Windows
│   └── projgit-winfsp/              placeholder (Phase 3d)
├── spikes/                          throwaway crates, NOT in workspace
│   └── ondemand-fetch/              0a: gix on-demand fetch (DONE)
├── docs/
│   ├── EXAMPLES.md                  worked CLI examples
│   ├── problem-statement.md         use case + prior-art comparison
│   ├── bench/baseline.md            checked-in benchmark numbers
│   ├── implementation/
│   │   ├── initial-plan.md          historical pre-implementation plan
│   │   └── handoff.md               THIS FILE
│   └── design/
│       ├── workload.md              workload shape projgit is built for
│       ├── fetchers.md             URL fetcher strategy + GixFetcher trade-off
│       ├── fetch-coalescing.md           designed: body batching + anticipatory hydration
│       ├── winfsp-implementation-plan.md  Windows backend resume plan
│       ├── prefetch.md              T1 implemented; later tiers designed
│       ├── windows-symlinks.md      decided
│       ├── dotgit-synthesis.md      parent ladder; A2/A3 still deferred
│       └── dotgit-index.md          A1+ index synthesis shipped 2026-05-18
└── .github/
    ├── workflows/ci.yml             fmt + clippy + tests
    └── skills/commit-work/          preferred commit workflow
```

## How to resume work

### Two ways to develop

**On the Windows host (default).** Edits, the projgit-core test
suite, and Linux compile-checks all work here. You cannot actually
mount FUSE from Windows. Future WinFsp work should resume from
[`docs/design/winfsp-implementation-plan.md`](../design/winfsp-implementation-plan.md).

**Inside the devcontainer (for FUSE work).** Open the workspace in
VS Code, then **Dev Containers: Reopen in Container**. Provides a
Debian + fuse3 + Rust environment with `/dev/fuse` available so
`mount_background` can actually serve a filesystem. See
[`.devcontainer/README.md`](../../.devcontainer/README.md) for the
full walkthrough, including the `target/` named-volume trick that
makes builds fast on Windows hosts.

### Verify the current state builds + tests pass

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cd e:\repos\gitfs
cargo build --workspace                 # should be clean
cargo test --workspace --all-targets    # default suite
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings   # clean
# Linux compile-check (requires `rustup target add x86_64-unknown-linux-gnu`):
cargo check -p projgit-fuse --target x86_64-unknown-linux-gnu --tests
```

### Run the Phase 4 end-to-end demo (inside the devcontainer)

```sh
mkdir -p /tmp/mp
cargo run -p projgit-cli -- mount https://github.com/rust-lang/log /tmp/mp --ref master
# in another shell:
ls -la /tmp/mp
cat /tmp/mp/Cargo.toml
# Ctrl-C the foreground process to unmount.
```

Local repos work too:
```sh
cargo run -p projgit-cli -- mount /path/to/repo /tmp/mp --ref main --offline
```

### Run the FUSE mount smoke test (inside the devcontainer)

Proves the fuser glue actually dispatches kernel ops to our
`FsProvider`, with real git data flowing through
`ProjectionFsProvider`.

```sh
cargo test -p projgit-fuse --test mount_smoke -- --ignored --nocapture
```

The test is `#[ignore]`-gated because FUSE isn't available on every
host (Windows native, CI runners without `/dev/fuse`).

### Run the network-gated end-to-end mount test

Proves the URL-mount path (partial clone + GitCliFetcher + FUSE) works
against a live remote.

```sh
PROJGIT_NETWORK_TESTS=1 \
  cargo test -p projgit-fuse --test mount_real_remote -- --ignored --nocapture
```

### Reproduce the mount benchmark

```sh
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release
# different target:
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
  --url https://github.com/<owner>/<repo> --ref <ref>
```

Results shape and methodology in
[`docs/bench/baseline.md`](../bench/baseline.md).

### Run the network-gated Fetcher test

Validates Phase 0a end-to-end. Hits GitHub once.
```powershell
$env:PROJGIT_NETWORK_TESTS = "1"
cargo test -p projgit-core --test fetcher gix_fetcher_hydrates
```

### Resume WinFsp work

The WinFsp prototypes are archived into
[`docs/design/winfsp-implementation-plan.md`](../design/winfsp-implementation-plan.md).
Resume directly in `crates/projgit-winfsp`; do not add a new spike crate
unless the implementation plan first proves wrong.

## Pre-installed environment on this machine

These are already set up; a fresh dev box would need to install them.

- Rust toolchain >= 1.85.0 stable (`rustup update stable`).
- Linux cross-compile target: `rustup target add x86_64-unknown-linux-gnu`.
- **Docker Desktop + Dev Containers VS Code extension.** For runtime
  FUSE work — see [`.devcontainer/README.md`](../../.devcontainer/README.md).
  The container itself bootstraps fuse3 / libfuse3-dev / pkg-config /
  rust-analyzer / clippy / rustfmt / lldb / gdb on first open via
  `postCreateCommand`.
- WinFsp 2.1.25156 with developer features
  (`winget install WinFsp.WinFsp`, then re-install with `ADDLOCAL=ALL`
  via msiexec to get the SDK headers + samples + import library).
- LLVM/clang (`winget install LLVM.LLVM`) for bindgen.
- Git (committer: `KittsKevin / kittskevin@hotmail.com`).
- VS Code with this folder as the workspace root.

## Gotchas worth remembering

These are the places a fresh session is most likely to slip on a banana
peel.

1. **PowerShell is the default shell here.** Avoid the `&` chain
   operator (reserved); use `;` to chain commands. Some terminal output
   gets a stray `§` prefix that looks like an error but isn't.
2. **`cargo` not on PATH in fresh PowerShell sessions.** Prefix:
   `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"`.
3. **`projgit-fuse` cross-checks need the Linux target installed**
   (above). Without it, `cargo check --target x86_64-unknown-linux-gnu`
   fails with a target-not-installed error, not a real compile error.
4. **`projgit-core`'s `gix-fetcher` feature is default-on**, but
  `projgit-cli` and `projgit-fuse` depend on `projgit-core` with
  `default-features = false`. Reason: gix's network deps pull
  reqwest + rustls + ring + a C compiler at build time, which would
  break Linux cross-checks from Windows. If you add network code
  anywhere, gate it behind `cfg(feature = "gix-fetcher")`. The
  `GitCliFetcher` and partial-clone helper are **not** behind this
  feature — they shell out to the system `git` and have no extra
  build deps.
5. **`ObjectStore` is `Send + Sync`** because it holds a
   `gix::ThreadSafeRepository` and creates per-call thread-local
   `Repository` handles. Don't "fix" it by going back to
   `gix::Repository`; the `Fetcher: Send + Sync` bound transitively
   needs `ObjectStore: Sync`.
6. **`MissingObject(oid)` is the only error variant the Fetcher hot
   path matches on.** Don't bury it inside a generic
   `Backend(String)`; consumers depend on the discriminant.
7. **Spikes are NOT workspace members** (`exclude = ["spikes"]` in
   the root `Cargo.toml`). They are deliberately throwaway. Don't
   promote spike code into a `crates/` member without rewriting it.
8. **WinFsp DLL on PATH at runtime.** Any binary that links WinFsp
  (eventually `projgit-winfsp`) needs
   `C:\Program Files (x86)\WinFsp\bin` on PATH or the process exits
   with `0xC06D007E` (delay-load failure).
9. **Modern `git`'s `safe.directory` check fails inside our future
   WinFsp mount** because the FSD reports volume ownership as
   `BUILTIN\Administrators`. Phase 3d step 15 must synthesize per-user
   ownership in `get_security_by_name` / `get_security`. (Surfaced in
  [`docs/design/winfsp-implementation-plan.md`](../design/winfsp-implementation-plan.md).)

## Decisions you don't have to re-litigate

(Full context in [`docs/initial-plan.md` §10](initial-plan.md) and the
two design docs.)

- **Read-only MVP.** No write path. No "commit-on-write." Cuts scope
  ~50%. Anything writing to the mount is a Phase 6+ thing.
- **`GitCliFetcher` is the production-default for URL mounts;
  `GixFetcher` stays for offline-friendly / no-git-on-PATH callers.**
  Phase 0a validated `gix` could fetch a single blob via
  `allow-tip-sha1-in-want`. As of 2026-05 GitHub rejects that path for
  many repos with `RejectedSourceObjectNotFound` and a silent empty
  pack; the same fetch framed as a partial-clone *promisor* request
  works. The shell-out fetcher gets us correct behaviour today; the
  `gix` path is where future native-Rust transport work can land if
  the policy changes back or moves to a sniffable feature.
- **One CAS, many mounts** is a hard architectural invariant. The
  store API never knows which projection is asking. This is what makes
  blob dedup free.
- **WinFsp on Windows for MVP code-sharing**, ProjFS deferred.
  Hand-rolled FFI bindings (no GPL-3.0 `winfsp-rs`) so the project
  stays MIT/Apache.
- **Windows symlinks default to `Auto` mode** (Native reparse points
  via WinFsp, Text fallback). Out-of-tree targets emit file-symlinks
  with a logged warning. Text-mode marker is an NTFS Alternate Data
  Stream (`:projgit.symlink`).
- **Submodules render as empty directories** in MVP.
- **`.git/` synthesis content is deferred** but the `RootOverlay`
  mechanism ships in Phase 1 (already done). Future ship-default is
  a sentinel `.projgit/info.json` + opt-in `--emit-dotgit=minimal`.
- **No daemon in MVP.** Per-process mounts share the on-disk store
  via file locks. A `projgitd` daemon can be added later without
  breaking the CLI surface.

## What I'd do next

Reprioritized 2026-06-18 after cat-file-pool Stage 1-3 and
prefetch coalescing shipped. The prior top two items (cat-file
pool, prefetch coalescing) are done. Remaining queue:

1. **`projgitd` Stage 5 — production polish.**
  systemd unit, restart policy, health checks, persistent daemon
  state, and structured logging (`tracing-subscriber`).
2. **CI bench job (B3).**
  Add a perf job to `.github/workflows/ci.yml` to guard baseline
  regressions now that the key bottleneck fixes are landed.
3. **Container deployment recipe doc.**
  Operator cookbook for host/sidecar deployment, mount propagation,
  and daemon socket wiring.
4. **Phase 3d production WinFsp backend.**
  Resume only if Windows target deployment is still in scope.
5. **Optional bench follow-ups.**
  rust N=4/N=10 post-pool matrix (only N=2 measured so far),
  higher-N worktree comparator, and target-scale workload
  validation. The prefetch-coalescing disk/upstream win in
  particular should be most visible at high N — worth a
  dedicated N=10 rust capture.

**Items folded into the projgitd plan and removed from the
standalone list:**

- *`projgit mount --background` + `projgit umount`.* In the
  projgitd world the daemon is the supervised long-lived process;
  per-mount background management becomes Harbor's / kubelet's
  job. Useful in pre-daemon T1 deployments; reconsider only if
  the projgitd plan slips significantly.

**Explicitly off the actionable list** (recorded so they don't sneak
back in by accident):

- *`projgitd` Stage 4 (T4 last mile via per-namespace fd-passing).*
  Stop condition met 2026-06-02: Harbor is a single-operator,
  shared-host, parallel-agents framework (Scenario A in
  [`docs/design/container-deployment.md`](../design/container-deployment.md)
  §6); T1.5 / Stage 3 is sufficient for that shape. T4's headline
  win (per-namespace isolation) protects against an attacker
  Harbor doesn't have; remaining benefits don't justify the
  `CAP_SYS_ADMIN` Harbor would need + ~330 LOC of
  code-without-a-customer. Full decision rationale in
  [`docs/design/projgitd.md`](../design/projgitd.md) §8 Stage 4.
  Spike + sidecar architecture stay in place so if multi-tenant
  ever lands as a real requirement, Stage 4 is ~1 session away.
- *Fetch coalescing.* Tried; doesn't close the cold gap; cold path
  is structural. See `docs/design/fetch-coalescing.md` §9.5.
- *Asciinema / video demo for the README.* User preference; the
  static captured terminal session in README's Quick Start is the
  no-recording substitute.

Whichever path you take, **commit per the
[`commit-work` skill](../../.github/skills/commit-work/SKILL.md)** —
write a message file, `git commit -F`, split logically related changes
from unrelated ones.
