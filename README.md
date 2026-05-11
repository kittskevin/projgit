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
