# projgit — Handoff

> Living document. Updated whenever we land a phase or change direction.
> Last updated: 2026-05-08, after devcontainer + FUSE smoke test.

If you're resuming work on this project after a break (or you're a fresh
AI session), read this file first. It's the shortest path back to context.
The deep references are linked; trust them, not memory.

## What is projgit?

A cross-platform user-mode filesystem in Rust. It lazily fetches git
objects from a normal git remote (via gitoxide) into a single shared
content-addressable store, then exposes one or more **read-only
projections** of that store as filesystem mounts (FUSE on Linux/macOS,
WinFsp on Windows).

Three projection kinds:
- **Ref** — `refs/heads/main` mounted as a directory tree.
- **Commit** — a specific commit OID mounted as a directory tree.
- **Subtree** — a sub-path inside either of the above.

The architectural killer feature: many simultaneous projections share
**one** object store, so blob deduplication is free.

Core decisions, design docs, and the phased plan all live in
[`docs/initial-plan.md`](initial-plan.md). Two design docs go deeper on
specific subsystems: [`docs/design/windows-symlinks.md`](design/windows-symlinks.md)
and [`docs/design/dotgit-synthesis.md`](design/dotgit-synthesis.md).

## Where we are right now

```
  HEAD    chore: devcontainer + FUSE mount_background + smoke test    ← latest
01e54a6  docs(handoff): mark Phase 3e done; bump test counts
9ab5449  feat(core): Phase 3e -- ProjectionFsProvider glue
d5f8836  feat(core): add ObjectStore::commit_time helper
aba421e  spike(3c): WinFsp hello-world via bindgen FFI -- mount works,
                    dispatch unresolved
ffd6ff6  feat(fuse): Phase 3b -- fuser backend, gated on Linux/macOS
0c53505  feat(core): Phase 3a -- FsProvider trait, InodeAllocator
ca93cea  chore(license): add MIT and Apache-2.0 licenses
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
  devcontainer; see [`.devcontainer/README.md`](../.devcontainer/README.md)).
- **Phase 3e — ProjectionFsProvider glue.** `ProjectionFsProvider<F>`
  in `projgit-core::projection_fs` bridges `Projection` +
  `HydratingObjectStore<F>` + `RootOverlay` to the `FsProvider` trait.
  Generic over `Fetcher`; per-inode `AttrSnapshot` cache populated
  lazily from `lookup`/`readdir`; gitlinks render as empty dirs;
  symlinks resolve via blob hydration; `mtime` stamped from
  `ObjectStore::commit_time` (new). 13 integration tests + 1
  commit_time test. Pure logic, no platform code.
- **License decision.** Project is dual-licensed **MIT OR Apache-2.0**.
  We hand-roll WinFsp FFI bindings (no GPL-3.0 `winfsp-rs`).
  See repo memory + [`docs/initial-plan.md` §10](initial-plan.md).

### In progress / partial
- **Phase 3c — WinFsp hello-world spike.** PARTIAL. The
  bindgen-driven FFI approach builds, links, loads `winfsp-x64.dll`
  via delay-load, and successfully establishes a mountpoint
  (`fsptool lsvol` confirms). But **no IRPs reach our user-mode
  callbacks** — `dir Z:\` returns "Incorrect function." See
  [`spikes/winfsp-helloworld/RESULTS.md`](../spikes/winfsp-helloworld/RESULTS.md)
  for ranked hypotheses. Top of the list: switch from bare
  `FspFileSystemStartDispatcher` to the `FspService*` lifecycle that
  the bundled C samples use.

### Not yet started
- **Phase 3d — Production `projgit-winfsp`.** Builds on the 3c spike's
  lessons. Includes: `FspService*` lifecycle, the symlink classifier
  per [`docs/design/windows-symlinks.md`](design/windows-symlinks.md),
  per-user volume ownership (Phase 0c finding 5). Now also consumes
  `ProjectionFsProvider` (3e) directly.
- **Phase 4 — CLI + mount manager.** With 3e landed, this is the
  shortest path to a Linux end-to-end demo: wire CLI →
  `partial_clone` → `ProjectionFsProvider` → `projgit-fuse::mount`.
- **Phase 5 — Polish (caches, metrics, integration tests).** Notably:
  tree-listing LRU cache (currently `ProjectionFsProvider` re-reads
  trees via `HydratingObjectStore::read_tree` on every `readdir`).

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
├── Cargo.toml                       workspace root, MSRV 1.85
├── LICENSE-MIT, LICENSE-APACHE      dual license
├── .devcontainer/                   Linux + FUSE dev environment
│   ├── devcontainer.json            base image, runArgs, mounts
│   └── README.md                    how to use it; FUSE quirks
├── crates/
│   ├── projgit-core/                Phase 1+2+3a+3e -- engine
│   ├── projgit-cli/                 placeholder (Phase 4)
│   ├── projgit-fuse/                Phase 3b -- empty on Windows
│   └── projgit-winfsp/              placeholder (Phase 3d)
├── spikes/                          throwaway crates, NOT in workspace
│   ├── ondemand-fetch/              0a: gix on-demand fetch (DONE)
│   ├── winfsp-reparse/              0c: reparse-point semantics (DONE)
│   └── winfsp-helloworld/           3c: hand-rolled FFI (PARTIAL)
├── docs/
│   ├── initial-plan.md              the plan; status notes inline
│   ├── handoff.md                   THIS FILE
│   └── design/
│       ├── windows-symlinks.md      decided
│       └── dotgit-synthesis.md      mechanism decided, content deferred
└── .github/skills/commit-work/      preferred commit workflow
```

## How to resume work

### Two ways to develop

**On the Windows host (default).** Edits, the projgit-core test
suite, the WinFsp spike, and Linux compile-checks all work here.
You cannot actually mount FUSE from Windows.

**Inside the devcontainer (for FUSE work).** Open the workspace in
VS Code, then **Dev Containers: Reopen in Container**. Provides a
Debian + fuse3 + Rust environment with `/dev/fuse` available so
`mount_background` can actually serve a filesystem. See
[`.devcontainer/README.md`](../.devcontainer/README.md) for the
full walkthrough, including the `target/` named-volume trick that
makes builds fast on Windows hosts.

### Verify the current state builds + tests pass

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cd e:\repos\gitfs
cargo build --workspace                 # should be clean on Windows
cargo test  -p projgit-core             # 22 + 4 + 18 + 13 = 57 tests pass
cargo clippy --workspace --all-targets -- -D warnings   # clean
# Linux compile-check (requires `rustup target add x86_64-unknown-linux-gnu`):
cargo check -p projgit-fuse --target x86_64-unknown-linux-gnu --tests
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

### Run the network-gated Fetcher test

Validates Phase 0a end-to-end. Hits GitHub once.
```powershell
$env:PROJGIT_NETWORK_TESTS = "1"
cargo test -p projgit-core --test fetcher gix_fetcher_hydrates
```

### Re-run a spike (each spike is standalone)

Spikes are excluded from the workspace; build / run them in their own
directory. The 3c spike requires WinFsp + LLVM installed (see its
RESULTS.md for the install commands).
```powershell
cd spikes/winfsp-helloworld
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
$env:PATH = "C:\Program Files (x86)\WinFsp\bin;" + $env:PATH
cargo build
.\target\debug\spike-winfsp-helloworld.exe Z:
# in another shell, while the spike runs:
& 'C:\Program Files (x86)\WinFsp\bin\fsptool-x64.exe' lsvol
```

## Pre-installed environment on this machine

These are already set up; a fresh dev box would need to install them.

- Rust toolchain ≥ 1.95.0 stable (`rustup update stable`).
- Linux cross-compile target: `rustup target add x86_64-unknown-linux-gnu`.
- **Docker Desktop + Dev Containers VS Code extension.** For runtime
  FUSE work — see [`.devcontainer/README.md`](../.devcontainer/README.md).
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
   `projgit-fuse` depends on `projgit-core` with
   `default-features = false`. Reason: gix's network deps pull
   reqwest + rustls + ring + a C compiler at build time, which would
   break Linux cross-checks from Windows. If you add network code
   anywhere, gate it behind `cfg(feature = "gix-fetcher")`.
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
   (the spike, eventually `projgit-winfsp`) needs
   `C:\Program Files (x86)\WinFsp\bin` on PATH or the process exits
   with `0xC06D007E` (delay-load failure).
9. **Modern `git`'s `safe.directory` check fails inside our future
   WinFsp mount** because the FSD reports volume ownership as
   `BUILTIN\Administrators`. Phase 3d step 15 must synthesize per-user
   ownership in `get_security_by_name` / `get_security`. (Surfaced in
   Phase 0c [`spikes/winfsp-reparse/RESULTS.md`](../spikes/winfsp-reparse/RESULTS.md).)

## Decisions you don't have to re-litigate

(Full context in [`docs/initial-plan.md` §10](initial-plan.md) and the
two design docs.)

- **Read-only MVP.** No write path. No "commit-on-write." Cuts scope
  ~50%. Anything writing to the mount is a Phase 6+ thing.
- **gitoxide is the Fetcher backend.** Branch A confirmed by Phase 0a.
  `GitCliFetcher` deferred as a future fallback.
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

In rough order of "smallest unit of work that yields the most
visible progress":

1. **Phase 4 CLI scaffolding.** With Phase 3e landed, the smallest
   visible step is a `projgit mount <repo> <projection> <mountpoint>`
   subcommand that wires `partial_clone` (or open existing) →
   `HydratingObjectStore` → `ProjectionFsProvider` →
   `projgit_fuse::mount`. This delivers the first Linux end-to-end
   demo: clone a public repo, mount `ref:main`, `ls` real git data.
2. **Phase 3d.** Production `projgit-winfsp` on top of the
   `FspService*` lifecycle (per the 3c spike findings). Now also
   consumes `ProjectionFsProvider` directly. The riskiest remaining
   piece. Best done in a fresh focused session.
3. **Phase 5 polish, especially tree-listing cache.** The current
   `ProjectionFsProvider::readdir` re-reads trees on every call; an
   LRU on parsed trees in `ObjectStore` would dramatically cut
   warm-cache latency without changing any public API.

Whichever path you take, **commit per the
[`commit-work` skill](../.github/skills/commit-work/SKILL.md)** —
write a message file, `git commit -F`, split logically related changes
from unrelated ones.
