# projgit Examples

These examples assume Linux/macOS or the devcontainer, because the current
mount backend is FUSE-based. Windows mounting is deferred; see
[design/winfsp-implementation-plan.md](design/winfsp-implementation-plan.md).

## Build And Test

```sh
cargo build --workspace
cargo test --workspace --all-targets
```

The real FUSE smoke test is ignored by default because it needs `/dev/fuse`:

```sh
cargo test -p projgit-fuse --test mount_smoke -- --ignored --nocapture
```

## Mount A Public Repository

```sh
mkdir -p /tmp/projgit-log
cargo run -p projgit-cli -- mount https://github.com/rust-lang/log /tmp/projgit-log --ref master --stats
```

In a second shell:

```sh
ls -la /tmp/projgit-log
cat /tmp/projgit-log/Cargo.toml
find /tmp/projgit-log/src -maxdepth 2 -type f
```

Stop the foreground `projgit mount` process with `Ctrl-C` to unmount. If
`--stats` is set, projgit prints tree, header, blob, and T1 prefetch counters on
unmount.

## Mount A Local Repository Offline

Use `--offline` when the source already has all objects needed for the
projection and projgit should never try the network.

```sh
mkdir -p /tmp/projgit-local
cargo run -p projgit-cli -- mount /path/to/repo /tmp/projgit-local --ref main --offline
```

## Mount A Specific Commit

```sh
commit=$(git -C /path/to/repo rev-parse HEAD~1)
mkdir -p /tmp/projgit-commit
cargo run -p projgit-cli -- mount /path/to/repo /tmp/projgit-commit --commit "$commit" --offline
```

## Mount A Subtree

Subtree mounts expose a path inside a ref or commit as the mount root.

```sh
mkdir -p /tmp/projgit-src
cargo run -p projgit-cli -- mount https://github.com/rust-lang/log /tmp/projgit-src --ref master --subtree src
```

## Run Network-Gated Fetcher Tests

These tests hit GitHub and are opt-in.

```sh
PROJGIT_NETWORK_TESTS=1 cargo test -p projgit-core --test fetcher -- --nocapture
```

## Troubleshooting

- `mountpoint ... does not exist`: create the mount directory first.
- `Operation not permitted` during the FUSE smoke test: run inside the
  devcontainer or another environment with `/dev/fuse`, `SYS_ADMIN`, and
  suitable mount permissions.
- Windows: `projgit mount` is intentionally disabled until the WinFsp backend
  is implemented.
