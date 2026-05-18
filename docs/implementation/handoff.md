# projgit — Handoff

> **Internal status notes for contributors.** Not user documentation. If you
> landed here from outside the project, start with the [README](../../README.md)
> and [docs/problem-statement.md](../problem-statement.md) instead. This file is
> a running scratch pad for whoever is actively working on projgit; it's
> deliberately written in the second person and assumes you're about to edit
> code.
>
> Living document. Updated whenever we land a phase or change direction.
> Last updated: 2026-05-18, after a full project audit, the dotgit A1
> and A1+ shipping, the fetch-coalescing retraction, and a README
> restructure / 'What works inside a projgit mount' documentation pass.

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

### Deferred / archived
- **Windows / WinFsp backend.** Deferred. The tracked WinFsp spike
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

Reprioritized 2026-05-18 after the project audit. The audit lives in
repo-scoped session memory at `/memories/repo/audit.md` (persists across
conversations); the actionable items below are its top open findings.

1. **A2 ref visibility** (audit A2 row, dotgit ladder).
   Symbolic `HEAD` → `refs/heads/<name>` + the ref file populated
   when the projection is a `Ref`. Enables `git branch
   --show-current`, IDE branch indicators, `git log --all` seeing
   the one ref. ~150 LOC per [`docs/design/dotgit-synthesis.md`](../design/dotgit-synthesis.md) §6;
   cleanly orthogonal to A1+ now that the axis-split insight
   landed.
2. **Phase C concurrent bench** (audit A3, the remaining open
   piece of the bench audit). Two simultaneous mounts of the same
   URL racing to cold-fetch the same blob. Would put a number on
   the cross-process single-flight gap. Deferred from this round
   because it's the highest-risk scenario (concurrent `git fetch`
   children writing into the same `.git/objects/pack/`); worth
   doing on purpose with a "if the cache dir gets weird, nuke and
   retry" stance. Smaller now that `--scenario sequential` proved
   the harness shape works.
3. **B3: CI bench job.** README + bench doc claim the bench
   protects against regression; CI runs only fmt/clippy/test.
   Add a perf job to `.github/workflows/ci.yml` that runs the
   bench and compares to the checked-in baseline. Moderate.
4. **Phase 3d. Production `projgit-winfsp`** on top of the
   `FspService*` lifecycle. Consume `ProjectionFsProvider`
   directly, exactly like Phase 4's CLI does on Linux. The
   riskiest remaining piece. Best done in a fresh focused session
   on the Windows host; first decide whether the Linux-focused
   workload makes this worth the cost (C1 leans "no").
5. **`projgit mount --background` + `projgit umount`.** Today the
   foreground process owns the mount; a PID-file flow plus an
   `umount` companion would let scripts manage many mounts.
   Designs need a small mount registry under `$XDG_RUNTIME_DIR`.
6. **`tracing-subscriber` wiring for the existing `-v` flag.** The
   verbosity flag stashes `PROJGIT_LOG` in env today; nothing reads
   it. Wiring `tracing-subscriber` (with optional crate feature)
   would surface fetcher/provider events at `-v` / `-vv`.

**Explicitly off the actionable list** (recorded so they don't sneak
back in by accident):

- *Fetch coalescing.* Tried; doesn't close the cold gap; cold path
  is structural. See `docs/design/fetch-coalescing.md` §9.5.
- *Asciinema / video demo for the README.* User preference; the
  static captured terminal session in README's Quick Start is the
  no-recording substitute.

Whichever path you take, **commit per the
[`commit-work` skill](../../.github/skills/commit-work/SKILL.md)** —
write a message file, `git commit -F`, split logically related changes
from unrelated ones.
