# projgit

[![CI](https://img.shields.io/github/actions/workflow/status/KittsKevin/projgit/ci.yml?label=CI&logo=github)](https://github.com/KittsKevin/projgit/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Status: experimental](https://img.shields.io/badge/status-experimental-yellow)](#status)

**Mount any Git commit as a read-only filesystem and fetch blob bytes lazily,
on first read, from a stock Git remote.** Directory listings are complete
immediately; file contents arrive on `open()` through the same partial-clone
promisor protocol GitHub already speaks. One on-disk object store is shared
across every mount on the host.

The motivating use case is agent-evaluation infrastructure: many short-lived
containers each pointed at a different commit, exploring a monorepo with
sparse, unpredictable access patterns. The longer motivation, prior-art
comparison (VFS for Git / Scalar, EdenFS, sparse-checkout), and concrete
success criteria are in [docs/problem-statement.md](docs/problem-statement.md).

## Architecture

```text
┌──────────────────────────────────────────────────────────┐
│  ls, cat, ripgrep, language servers, agent tool calls    │
└────────────────────────────┬─────────────────────────────┘
                             │  POSIX syscalls
                             ▼
┌──────────────────────────────────────────────────────────┐
│  Kernel: FUSE (Linux/macOS)  •  WinFsp (Windows, planned)│
└────────────────────────────┬─────────────────────────────┘
                             ▼
┌──────────────────────────────────────────────────────────┐
│  ProjectionFsProvider    inode allocator + attr cache    │
│  Projection (Ref|Commit|Subtree) + RootOverlay           │
└────────────────────────────┬─────────────────────────────┘
                             ▼
┌──────────────────────────────────────────────────────────┐
│  HydratingObjectStore    single-flight coalescing        │
│  ObjectStore             tree / header / small-blob LRUs │
└──────┬────────────────────────────────────┬──────────────┘
       │ object hit                         │ object miss
       ▼                                    ▼
┌──────────────────┐                ┌──────────────────────┐
│  shared on-disk  │ ◀── promisor ──│  Fetcher             │
│  git odb (one    │     fetch      │  GitCli | GVFS |     │
│  CAS, N mounts)  │                │  Gix    | Noop       │
└──────────────────┘                └──────────┬───────────┘
                                               ▼
                                    ┌──────────────────────┐
                                    │  upstream git remote │
                                    │  (GitHub, GitLab,    │
                                    │   Gitaly, SSH, …)    │
                                    └──────────────────────┘
```

Everything below the kernel boundary is OS-agnostic Rust. The FUSE adapter
(`projgit-fuse`) and the planned WinFsp adapter (`projgit-winfsp`) both
target the same `FsProvider` trait, so the projection engine, caches, and
fetchers are tested once and shared across platforms. The shared CAS is the
architectural lever: N mounts of the same commit pay the network cost once.

## Measured Behavior

Captured by [crates/projgit-cli/examples/bench_mount.rs](crates/projgit-cli/examples/bench_mount.rs)
against `https://github.com/rust-lang/log` at `master`, median of 3 iterations,
times in milliseconds. Full methodology in
[docs/bench/baseline.md](docs/bench/baseline.md).

| Operation              | projgit cold | projgit warm | git baseline |
| ---------------------- | -----------: | -----------: | -----------: |
| `readdir` of root      |        0.93 |        0.97 |        6.78 |
| recursive walk         |        8.04 |        1.57 |        5.67 |
| `cat` 3 files          |     8,754.7 |        0.48 |     2,904.3 |

`git baseline` is `git ls-tree` and `git cat-file blob` against a fresh
`git clone --filter=blob:none --no-checkout` of the same repo.

- `readdir` is **~7×** faster than `git ls-tree` even cold; tree objects
  ship with the partial clone and projgit serves them in-process.
- Warm reads are **~6,000×** faster than the git baseline because the
  bytes live in projgit's small-blob LRU cache.
- Cold first-read of an uncached file is currently **slower** than
  `git cat-file` cold; `GitCliFetcher` issues one promisor fetch per
  fault and does not yet batch blob requests. This is the next fetcher
  improvement (designed in [docs/design/fetch-coalescing.md](docs/design/fetch-coalescing.md));
  the bench exists to catch when it changes.

Reproduce on Linux/macOS:

```sh
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release
```

## Quick Start

Requirements: Rust 1.85+, the system `git` executable on `PATH`, and (on
Linux/macOS) `/dev/fuse` available. The included VS Code devcontainer sets
all of this up.

```sh
cargo build --workspace
cargo test --workspace --all-targets
```

What `projgit mount` looks like end-to-end against
[`rust-lang/log`](https://github.com/rust-lang/log):

```console
$ mkdir -p /tmp/log
$ projgit mount https://github.com/rust-lang/log /tmp/log --ref master --stats
projgit: partial-cloning https://github.com/rust-lang/log into ~/.cache/projgit/log-58b87cfa
projgit: mounting at /tmp/log (Ctrl-C to unmount)

# ── in another shell ──────────────────────────────────────
$ ls /tmp/log
benches    Cargo.toml      CHANGELOG.md   LICENSE-APACHE   LICENSE-MIT
README.md  rfcs            src            tests            triagebot.toml

$ head -5 /tmp/log/Cargo.toml
[package]

name = "log"
version = "0.4.29" # remember to update html_root_url
authors = ["The Rust Project Developers"]

$ wc -l /tmp/log/src/lib.rs
2010 /tmp/log/src/lib.rs

# ── Ctrl-C the foreground mount process ──────────────────
projgit: unmounting…
projgit: tree cache    hits=14 misses=2 inserts=2 evictions=0 len=2/256
projgit: header cache  hits=4 misses=14 inserts=11 evictions=0 len=8/4096
projgit: blob cache    hits=1 misses=2 inserts=2 evictions=0 bytes=68092/16777216
projgit: prefetch (T1) posted=1 dropped=0 batches=1 resolved=7 headers=7 failed=0
projgit: unmounted.
```

The on-disk partial clone in `~/.cache/projgit/` is reusable: a second
`projgit mount` of the same URL skips the clone step entirely and shares
the object store with the first mount. See [docs/EXAMPLES.md](docs/EXAMPLES.md)
for more command examples, and `projgit mount --help` for the full flag
surface (refs, commits, subtrees, `--offline`, alternate fetchers).

## Engineering Highlights

Pieces worth pointing at if you're skimming the code:

- **OS-agnostic projection engine.** [`FsProvider`](crates/projgit-core/src/fs_provider.rs)
  is a small read-only filesystem trait. The FUSE and WinFsp adapters both
  consume it, so the projection logic, caches, and fetchers have a single
  test surface that doesn't depend on a kernel module being loaded.
- **One CAS, many projections.** [`ObjectStore`](crates/projgit-core/src/object_store.rs)
  is `Send + Sync` (via `gix::ThreadSafeRepository` + per-call thread-local
  handles) so a single in-process store can back arbitrarily many concurrent
  mounts without copying blobs.
- **Layered caches with explicit budgets.** Hand-rolled bounded LRUs for
  parsed trees ([`tree_cache.rs`](crates/projgit-core/src/tree_cache.rs)),
  object headers ([`header_cache.rs`](crates/projgit-core/src/header_cache.rs)),
  and small blobs ([`blob_cache.rs`](crates/projgit-core/src/blob_cache.rs),
  byte-bounded with a per-entry cap so one big file can't evict the
  working set). Counters exposed through `--stats`.
- **Single-flight fetch coalescing.** [`Coalescer`](crates/projgit-core/src/fetcher/coalesce.rs)
  ensures a thundering herd of kernel reads for the same OID dedupes to one
  upstream request; the rest park on the in-flight future.
- **Pluggable fetchers behind one trait.**
  [`GitCliFetcher`](crates/projgit-core/src/fetcher/git_cli.rs) drives the
  partial-clone promisor path via a long-lived `git cat-file --batch-check`
  child (one process / one TLS session for the whole mount).
  [`GixFetcher`](crates/projgit-core/src/fetcher/gix_fetcher.rs) is a pure-Rust
  fallback. [`GvfsFetcher`](crates/projgit-core/src/fetcher/gvfs.rs) (feature-gated)
  talks the GVFS protocol. Trade-offs in
  [docs/design/fetchers.md](docs/design/fetchers.md).
- **Anticipatory header prefetch.**
  [`prefetch.rs`](crates/projgit-core/src/prefetch.rs) warms the header cache
  for a directory's children on `readdir`, so the kernel's follow-up `lookup`
  burst hits the cache instead of forking a `git cat-file` per file. Tier
  ladder in [docs/design/prefetch.md](docs/design/prefetch.md).
- **Checked-in reproducible benchmark.** [`bench_mount.rs`](crates/projgit-cli/examples/bench_mount.rs)
  measures cold and warm latency for the three operations above against a
  fresh `git clone --filter=blob:none` baseline. Results land in
  [docs/bench/baseline.md](docs/bench/baseline.md) and the table above; a
  regression in any cell is visible to the next contributor.
- **`#![forbid(unsafe_code)]`** across every crate, including the WinFsp stub.
  The handful of unsafe surface that the eventual WinFsp backend will need
  is isolated to that crate by design.

## Designed For One Workload Shape

projgit is opinionated for a specific access pattern, not a general-purpose
virtual git client. Every cache, prefetch tier, and fetcher choice flows from
this shape:

> **Many short-lived processes pointed at a Git commit, performing wide-shallow
> access with bursty concurrency and predictable ordering, that tolerate
> speculative work in exchange for low interactive latency.**

In concrete terms:

- **Wide-shallow access.** Most files are never touched. Reads target a small,
  unpredictable subset (`README.md`, lockfiles, a few source files surfaced by
  search). Total bytes read are usually well under 1% of the commit.
- **Bursty concurrency.** Traffic comes in storms (directory walks,
  `ripgrep`, language-server indexing, parallel agent tool calls) separated
  by idle gaps that prefetch can use.
- **Predictable ordering.** `readdir → lookup → read` is the canonical
  sequence; each step structurally hints at what's next, so prefetch isn't
  guessing.
- **Tolerance for over-fetching.** Bytes-not-read (especially for small files)
  are a cheap mistake; reads blocked on the network are an expensive one.
  Anticipatory work to make file contents available, within a budget, is safe.
- **High parallelism with shared storage.** Many mounts on one host share the
  on-disk Git object store, so the first cold fetch is amortised across every
  subsequent reader of the same commit.

Workloads projgit deliberately is **not** for: long-lived dev workstations
(just clone), heavy writes (read-only), full-tree static analysis (no lazy-fetch
win), binary or multimedia repos (blows our small-blob budgets), and anything
that wants commit-history semantics (no graph walking inside the mount).

The design discipline that follows from this shape, and a per-subsystem map
of which workload property each cache/prefetch tier serves, lives in
[docs/design/workload.md](docs/design/workload.md).

## Why This Exists

Stock `git clone --filter=blob:none` can lazy-fetch historical blobs, but a
normal checkout still writes a complete working tree. Sparse checkout avoids
some working-tree cost, but files outside the sparse patterns do not appear at
all. For agents exploring an unknown codebase, that breaks the contract: every
file in the commit must be visible before its bytes are fetched.

projgit's design target is the missing middle ground:

- **Lazy fetch:** disk and bandwidth proportional to touched objects.
- **Total enumerability:** `os.walk`, `find`, language servers, and build tools
  see the whole tree.
- **Shared storage:** many projections reuse the same Git object store.

The longer motivation and prior-art comparison are in
[docs/problem-statement.md](docs/problem-statement.md).

## Status

This is a prototype, not a production filesystem.

What works today:

- Core object-store, projection, overlay, and path-resolution logic.
- Lazy blob hydration through Git partial-clone/promisor behavior.
- Linux/macOS FUSE backend through `fuser`.
- `projgit mount` for refs, commits, and subtrees.
- Tree, blob, and header caches, plus T1 readdir-time header prefetch.
- Optional GVFS protocol fetcher behind the `gvfs-fetcher` feature.
- Local test coverage for the core, CLI, projection filesystem, and FUSE smoke
  path, plus a network-gated end-to-end mount test against a real remote.

What is deliberately deferred:

- Windows mounting. The `projgit-winfsp` crate is a stub; the implementation
  plan lives in [docs/design/winfsp-implementation-plan.md](docs/design/winfsp-implementation-plan.md).
- Synthesized `.git/` contents. The root overlay mechanism exists, but the
  default mount currently exposes the projected tree only.
- A long-running daemon for many concurrent mounts sharing one upstream
  connection.
- Writes. The MVP is read-only.

## Project Layout

```text
crates/projgit-core/      projection engine, object store, fetchers, caches
crates/projgit-cli/       `projgit mount` command
crates/projgit-fuse/      Linux/macOS FUSE adapter
crates/projgit-winfsp/    Windows backend stub; implementation deferred
docs/                     problem statement, design notes, status handoff
spikes/ondemand-fetch/    completed Git on-demand fetch spike
.devcontainer/           Linux + FUSE development environment
```

## Verification

Default local checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Optional checks:

```sh
PROJGIT_NETWORK_TESTS=1 cargo test -p projgit-core --test fetcher -- --nocapture
cargo test -p projgit-fuse --test mount_smoke -- --ignored --nocapture
PROJGIT_NETWORK_TESTS=1 \
  cargo test -p projgit-fuse --test mount_real_remote -- --ignored --nocapture
```

Network tests hit GitHub. The FUSE smoke test requires `/dev/fuse` and suitable
mount permissions. The `mount_real_remote` test partial-clones
`https://github.com/rust-lang/log` and walks it through the real FUSE mount.

## Design Docs

- [docs/problem-statement.md](docs/problem-statement.md) explains the concrete
  agent-evaluation use case.
- [docs/initial-plan.md](docs/initial-plan.md) captures the phased architecture.
- [docs/handoff.md](docs/handoff.md) is the current status document.
- [docs/design/prefetch.md](docs/design/prefetch.md) covers the prefetch tier
  ladder.
- [docs/design/fetchers.md](docs/design/fetchers.md) covers why URL mounts use
  system `git` today and where `GixFetcher` still fits.
- [docs/design/dotgit-synthesis.md](docs/design/dotgit-synthesis.md) covers
  future `.git/` synthesis.
- [docs/design/windows-symlinks.md](docs/design/windows-symlinks.md) covers the
  Windows symlink policy.
- [docs/design/winfsp-implementation-plan.md](docs/design/winfsp-implementation-plan.md)
  preserves the deferred Windows backend plan.

## License

Dual licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))
