# projgit

`projgit` is an experimental Rust filesystem that exposes a Git commit as a
read-only directory tree while fetching object contents lazily from a normal Git
remote.

The motivating use case is agent-evaluation infrastructure for large
repositories: many short-lived containers need a complete-looking checkout at a
specific commit, but most of them only touch a small and unpredictable subset of
the tree. projgit aims to make directory enumeration complete, blob reads lazy,
and object storage shared across mounts.

Because projgit sits between readers and Git object bytes, it can add
workload-specific policy later: prefetching known hot paths, sharing warmed
metadata across mounts, or shaping fetch behavior around observed access
patterns.

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
  path.

What is deliberately deferred:

- Windows mounting. The `projgit-winfsp` crate is a stub; the implementation
  plan lives in [docs/design/winfsp-implementation-plan.md](docs/design/winfsp-implementation-plan.md).
- Synthesized `.git/` contents. The root overlay mechanism exists, but the
  default mount currently exposes the projected tree only.
- A long-running daemon for many concurrent mounts sharing one upstream
  connection.
- Writes. The MVP is read-only.

## Quick Start

On Linux/macOS, or inside the provided devcontainer:

```sh
cargo build --workspace
cargo test --workspace --all-targets
```

You need Rust 1.85 or newer and the system `git` executable on `PATH`. URL
mounts use Git's partial-clone promisor support for lazy hydration.

projgit deliberately uses system `git` for URL hydration today instead of the
native `GixFetcher` path, because hosted servers are more reliable when missing
objects are requested through Git's partial-clone promisor machinery. See
[docs/design/fetchers.md](docs/design/fetchers.md) for the trade-off.

GVFS-capable remotes can be tried explicitly with `--features gvfs-fetcher`,
`--fetcher gvfs`, and `--gvfs-url`; this is an optional backend, not the
default.

To run the FUSE smoke test, use an environment with `/dev/fuse` available. The
VS Code devcontainer is configured for this.

```sh
cargo test -p projgit-fuse --test mount_smoke -- --ignored --nocapture
```

Mount a public repository:

```sh
mkdir -p /tmp/projgit-log
cargo run -p projgit-cli -- mount https://github.com/rust-lang/log /tmp/projgit-log --ref master --stats
```

In another shell:

```sh
ls -la /tmp/projgit-log
cat /tmp/projgit-log/Cargo.toml
```

Press `Ctrl-C` in the foreground `projgit mount` process to unmount. See
[docs/EXAMPLES.md](docs/EXAMPLES.md) for more command examples.

## Why This Exists

Stock `git clone --filter=blob:none` can lazy-fetch historical blobs, but a
normal checkout still writes a complete working tree. Sparse checkout avoids
some working-tree cost, but files outside the sparse patterns do not appear at
all. For agents exploring an unknown codebase, that breaks the contract: every
file in the commit must be visible before its bytes are fetched.

projgit's design target is the missing middle ground:

- **Lazy fetch:** disk and bandwidth proportional to touched objects.
- **Total enumerability:** `os.walk`, `find`, language servers, and build tools
  can see the whole tree.
- **Shared storage:** many projections can reuse the same Git object store.

The longer motivation and prior-art comparison are in
[docs/problem-statement.md](docs/problem-statement.md).

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
  Anticipatory work to make files contents available, within a budget, is safe.
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

## Measured Behavior

Captured by [crates/projgit-cli/examples/bench_mount.rs](crates/projgit-cli/examples/bench_mount.rs)
against `https://github.com/rust-lang/log` at `master`, median of 3 iterations,
times in milliseconds. Full results and methodology in
[docs/bench/baseline.md](docs/bench/baseline.md).

| Operation              | projgit cold | projgit warm | git baseline |
| ---------------------- | -----------: | -----------: | -----------: |
| `readdir` of root      |        0.93 |        0.97 |        6.78 |
| recursive walk         |        8.04 |        1.57 |        5.67 |
| `cat` 3 files          |     8,754.7 |        0.48 |     2,904.3 |

`git baseline` is `git ls-tree` and `git cat-file blob` against a fresh
`git clone --filter=blob:none --no-checkout` of the same repo.

Take-aways:

- `readdir` is **~7×** faster than `git ls-tree` even cold; tree objects
  ship with the partial clone and projgit serves them in-process.
- Warm reads are **~6,000×** faster than the git baseline because the
  bytes live in projgit's small-blob LRU cache.
- Cold first-read of an uncached file is currently **slower** than
  `git cat-file` cold; `GitCliFetcher` does not yet pipeline blob bytes
  the way native git's promisor fetch does. This is the next fetcher
  improvement, and the bench exists to catch when it changes.

Reproduce on Linux/macOS:

```sh
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release
```

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
