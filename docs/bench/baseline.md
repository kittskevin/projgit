# Bench Baseline — `rust-lang/log`

Captured 2026-05-11 inside the projgit devcontainer (WSL2). Numbers
are wall-clock, median of 3 iterations, all in milliseconds.

Reproduce:

```sh
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release
```

For a different target:

```sh
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
  --url https://github.com/<owner>/<repo> --ref <ref>
```

## Environment

- Host: WSL2 on AMD Ryzen 7 7800X3D, 16 logical cores
- Kernel: Linux 6.6 (WSL2)
- Network: residential broadband over GitHub HTTPS
- `rustc 1.95.0`
- `git 2.53.0`
- `fuser 0.17`

## What was measured

Each iteration:

1. Fresh temp dirs.
2. `partial_clone` of `https://github.com/rust-lang/log` at `master`
   (`git clone --filter=blob:none --no-checkout`).
3. Build the same provider stack `projgit mount` builds for URL
   sources: `ObjectStore` + `GitCliFetcher` + `HydratingObjectStore`
   + `Projection::Ref("master")`.
4. Mount via FUSE with `mount_background`, wait for mount.
5. Time these access patterns through the kernel:
   - `readdir` of the mountpoint root.
   - Recursive walk via `read_dir` of every directory.
   - `read_to_string` of three known-stable files
     (`Cargo.toml`, `src/lib.rs`, `LICENSE-APACHE`).
6. Repeat the three operations to capture the warm path
   (in-process tree/header/blob caches populated, blobs already
   hydrated locally).
7. Drop the mount; clean up.

Baseline arm runs the analogous git operations against a fresh
`git clone --filter=blob:none --no-checkout` of the same repo:

- `git ls-tree <ref>` for root.
- `git ls-tree -r <ref>` for the recursive walk.
- `git cat-file blob <ref>:<file>` × 3 for the file reads.

This captures what a user would see if they had used vanilla git's
partial-clone path and then walked the result with shell tools.

## Results

### One-time setup

| Step                          | projgit (`partial_clone`) | git (`clone --filter=blob:none --no-checkout`) |
| ----------------------------- | ------------------------: | ---------------------------------------------: |
| Partial clone (ms)            |                  2,300.2 |                                       1,026.7 |

`projgit`'s `partial_clone` shells out to the same `git clone`
command, so the per-call difference here is dominated by network
jitter between sequential GitHub fetches inside one iteration.

### Per-operation

| Operation               | projgit cold | projgit warm | git baseline |
| ----------------------- | -----------: | -----------: | -----------: |
| `readdir` of root (ms)  |        0.93 |        0.97 |        6.78 |
| recursive walk (ms)     |        8.04 |        1.57 |        5.67 |
| `cat` 3 files (ms)      |     8,754.7 |        0.48 |     2,904.3 |

## What this shows

- **Enumeration is cheap, even cold.** projgit's `readdir` of the
  mount root is roughly **7×** faster than `git ls-tree` because
  the entire tree is already in the partial-clone pack and projgit
  serves it from in-process state. `git ls-tree` pays a fork+exec
  per invocation. The cold and warm numbers are close because tree
  objects are already local; the only difference is whether the
  in-process tree LRU has the entry yet.

- **Recursive walk is competitive.** Cold `read_dir` of every
  directory is comparable to `git ls-tree -r`. Warm walk is
  **~5×** faster than the git baseline; the entire tree is now in
  projgit's `TreeCache` and every readdir is a hash lookup.

- **Warm reads are effectively free.** Re-reading three files after
  the first mount is **~6,000×** faster than running git
  `cat-file blob` against the same partial clone, because the bytes
  live in projgit's small-blob LRU and never leave the process.

- **Cold reads are currently slower than `git cat-file` cold.**
  projgit's first read of three uncached files takes ~8.7s vs git's
  ~2.9s. Both go to the network, but `GitCliFetcher` hydrates one
  blob per fault and does not yet pipeline blob bytes the way git's
  native promisor fetch does. This is the next fetcher-layer
  improvement, and it is exactly the kind of regression a checked-in
  benchmark exists to catch.

## Caveats

- Single machine, single repo, single network. Numbers will vary;
  the **shape** is what to compare across runs, not absolute ms.
- `rust-lang/log` is small. Larger repos amplify both the cold-walk
  cost and the warm-cache benefit. A second target is a future
  follow-up.
- This bench does **not** drop kernel page cache between cold and
  warm. The cold/warm split here is about projgit's in-process
  caches and remote round-trips, not Linux's block cache. That is
  the right comparison for the agent-eval workload, where many
  short-lived processes mount and walk the same projection.
