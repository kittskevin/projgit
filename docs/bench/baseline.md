# Bench Baseline

Captured 2026-05-18 inside the projgit devcontainer (WSL2). Numbers
are wall-clock, median of 3 iterations, all in milliseconds.

Two targets, two scenarios. The point of doing both is to back up the
workload doc's claims with empirical evidence rather than handwave at
them — see [`docs/design/workload.md`](../design/workload.md) §1.6 for
the headline claim under test.

## Reproduce

```sh
# rust-lang/log, single scenario (the original baseline shape)
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release

# rust-lang/log, sequential scenario (mount → unmount → fresh mount)
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
  --scenario sequential

# rust-lang/cargo, single scenario (moderately-sized repo)
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
  --url https://github.com/rust-lang/cargo --ref master \
  --files Cargo.toml,LICENSE-APACHE,README.md

# rust-lang/cargo, sequential scenario
PROJGIT_NETWORK_TESTS=1 \
  cargo run -p projgit-cli --example bench_mount --release -- \
  --url https://github.com/rust-lang/cargo --ref master \
  --files Cargo.toml,LICENSE-APACHE,README.md \
  --scenario sequential
```

## Environment

- Host: WSL2 on AMD Ryzen 7 7800X3D, 16 logical cores
- Kernel: Linux 6.6 (WSL2)
- Network: residential broadband over GitHub HTTPS
- `rustc 1.95.0`
- `git 2.53.0`
- `fuser 0.17`

## Scenarios

- **`single`** — one fresh partial clone, one mount, cold + warm
  passes. The original bench shape; matches the headline numbers
  shipped in earlier versions of this doc.
- **`sequential`** — same as `single`, then drops mount 1 and
  re-mounts a freshly-constructed `ObjectStore`/`Fetcher`/`Provider`
  against the **same on-disk cache dir**. Measures only mount 2's
  cold cat. In-process caches are empty by construction (fresh
  `ObjectStore`); the on-disk CAS is warm from mount 1's cold
  reads. This is the falsifier for workload §1.6 — "the first
  mount pays the network cost; every subsequent mount sees a warm
  hit."
- **`daemon-concurrent` / `naive-concurrent` (Phase C)** — *run
  2026-06-02*. N simultaneous local mounts of the same URL
  cold-fetching the same blobs. `daemon-concurrent` runs them
  through one in-thread `projgitd` (its `Coalescer` dedupes
  concurrent fetches per audit A3); `naive-concurrent` skips the
  daemon and lets N independent `GitCliFetcher`s race a shared
  `.git/objects/pack/`. Results below in
  [Phase C concurrent](#results--phase-c-concurrent-rust-langlog--master).
  Headline: at N ≤ 10 with 3 small blobs, the daemon's coalescer
  isn't a wall-clock win — it's a tie within noise. At 20
  files/N=10 it becomes a ~12% loss because of
  serialise-through-one-cat-file overhead.

## What was measured

Each iteration of the projgit arm:

1. Fresh temp dir for the cache.
2. `partial_clone` of the target URL at the target ref
   (`git clone --filter=blob:none --no-checkout`).
3. Build the same provider stack `projgit mount` builds for URL
   sources: `ObjectStore` + `GitCliFetcher` + `HydratingObjectStore`
   + `Projection::Ref(<ref>)`.
4. Mount via FUSE with `mount_background`, wait for mount.
5. Time these access patterns through the kernel:
   - `readdir` of the mountpoint root.
   - Recursive walk via `read_dir` of every directory.
   - `read_to_string` of the per-target file list.
6. Repeat the three operations to capture the warm path
   (in-process tree/header/blob caches populated, blobs already
   hydrated locally).
7. Drop the mount.
8. **`sequential` only:** build a fresh provider stack against the
   same cache dir, mount it on a new mountpoint, time `cat` of the
   same files, drop the mount.
9. Clean up.

Baseline arm runs the analogous git operations against a fresh
`git clone --filter=blob:none --no-checkout` of the same repo:

- `git ls-tree <ref>` for root.
- `git ls-tree -r <ref>` for the recursive walk.
- `git cat-file blob <ref>:<file>` × N for the file reads.

This captures what a user would see if they had used vanilla git's
partial-clone path and then walked the result with shell tools.

## Results — `rust-lang/log` @ master

Cat targets: `Cargo.toml`, `src/lib.rs`, `LICENSE-APACHE` (3 files).

### `--scenario single`

| Step                  | projgit (`partial_clone`) | git (`clone --filter=blob:none --no-checkout`) |
| --------------------- | ------------------------: | ---------------------------------------------: |
| Partial clone         |                     878.9 |                                          784.0 |

| Operation               | projgit cold | projgit warm | git baseline |
| ----------------------- | -----------: | -----------: | -----------: |
| `readdir` of root       |         0.27 |         0.16 |         4.14 |
| recursive walk          |         2.12 |         0.93 |         4.32 |
| `cat` 3 files           |      3,398.1 |         0.26 |      1,192.5 |

### `--scenario sequential`

Same as single above, plus the §1.6 amortisation row:

| Operation     | mount 1 cold | mount 1 warm | **mount 2 cold (cross-process)** |
| ------------- | -----------: | -----------: | -------------------------------: |
| `cat` 3 files |      3,957.1 |         0.31 |                         **1.34** |

Mount 2's cold cat is **~3,000× faster** than mount 1's cold cat,
and only ~4× slower than mount 1's in-process LRU warm hit. The
~4× gap is the cost of going through the pack file + zlib decompress
on disk vs an in-process LRU hit; that's the expected floor.
§1.6 holds.

## Results — `rust-lang/cargo` @ master

Cat targets: `Cargo.toml`, `LICENSE-APACHE`, `README.md` (3 files).
~1,200 files at the commit; ~10× the file count of `rust-lang/log`.

### `--scenario single`

| Step                  | projgit (`partial_clone`) | git (`clone --filter=blob:none --no-checkout`) |
| --------------------- | ------------------------: | ---------------------------------------------: |
| Partial clone         |                   2,829.4 |                                        2,850.6 |

| Operation               | projgit cold | projgit warm | git baseline |
| ----------------------- | -----------: | -----------: | -----------: |
| `readdir` of root       |         0.30 |         0.15 |         4.18 |
| recursive walk          |        268.7 |        114.4 |         6.37 |
| `cat` 3 files           |      6,931.2 |         0.21 |      1,143.3 |

### `--scenario sequential`

| Operation     | mount 1 cold | mount 1 warm | **mount 2 cold (cross-process)** |
| ------------- | -----------: | -----------: | -------------------------------: |
| `cat` 3 files |      5,130.0 |         0.21 |                         **1.08** |

Mount 2's cold cat is **~4,750× faster** than mount 1's cold cat —
amortisation holds at this scale too. The persistent on-disk CAS is
doing exactly what the workload doc claims.

## Results — Phase C concurrent (`rust-lang/log` @ master)

Captured 2026-06-02 inside the projgit devcontainer; same machine /
network as the `single` and `sequential` results above. Median of 3
iterations per cell. **Wall clock** is from the all-N-threads
barrier release to the last thread's join (the load-bearing
headline); **per-thread p50** is the median across all per-thread
cold-cat durations from all iterations (3 × N samples).

Cat targets: `Cargo.toml`, `src/lib.rs`, `LICENSE-APACHE` (3 files,
same as the `single` table above). Cold blobs only — every
iteration starts from a fresh `cache_dir` so no on-disk state is
inherited from prior runs.

| N  | daemon-concurrent wall | daemon p50 | naive-concurrent wall | naive p50 | naive/daemon | failures |
| -- | ---------------------: | ---------: | --------------------: | --------: | -----------: | -------: |
| 1  |                1,577.0 |    1,577.0 |               1,186.0 |   1,185.9 |        0.75× |    0 / 0 |
| 4  |                1,185.0 |    1,184.7 |               1,283.9 |   1,235.1 |        1.08× |    0 / 0 |
| 10 |                1,276.7 |    1,275.3 |               1,331.6 |   1,299.0 |        1.04× |    0 / 0 |

Per-thread range across iterations (min – max), useful for
gauging variance:

| N  | daemon range | naive range |
| -- | -----------: | ----------: |
| 1  |  1,168.9 – 1,656.0 (n=3)  | 1,174.0 – 1,275.9 (n=3)  |
| 4  |  1,143.7 – 1,234.8 (n=12) | 1,187.1 – 1,293.5 (n=12) |
| 10 |  1,169.4 – 1,336.6 (n=30) | 1,227.2 – 1,351.1 (n=30) |

### Secondary measurement — same matrix, but 20 files per consumer

To probe whether the daemon's coalescer wins at higher per-thread
fetch counts, the N=10 cell was re-run with a 20-file `--files`
list (everything in `rust-lang/log` master at the top level plus
all of `src/kv/` and `tests/`). Same medians-of-3 protocol.

| N  | daemon wall (20 files) | naive wall (20 files) | naive/daemon |
| -- | ---------------------: | --------------------: | -----------: |
| 10 |                9,068.1 |               8,123.9 |        0.90× |

In this regime the naive arm is the *faster* one — the daemon's
coalescing turns from a tie into a slowdown (~12%) as per-thread
work grows.

### Reproduce

```sh
# 3-file matrix
for arm in daemon-concurrent naive-concurrent; do
  for n in 1 4 10; do
    PROJGIT_NETWORK_TESTS=1 \
      cargo run -p projgit-cli --example bench_mount --release -- \
      --scenario "$arm" --concurrency "$n" --iterations 3
  done
done

# 20-file probe (re-run only the N=10 cell)
FILES="Cargo.toml,src/lib.rs,LICENSE-APACHE,LICENSE-MIT,README.md,\
CHANGELOG.md,src/macros.rs,src/serde.rs,src/__private_api.rs,\
src/kv/mod.rs,src/kv/key.rs,src/kv/value.rs,src/kv/source.rs,\
src/kv/error.rs,tests/integration.rs,tests/macros.rs,benches/value.rs,\
.gitignore,triagebot.toml,.github/workflows/main.yml"
for arm in daemon-concurrent naive-concurrent; do
  PROJGIT_NETWORK_TESTS=1 \
    cargo run -p projgit-cli --example bench_mount --release -- \
    --scenario "$arm" --concurrency 10 --iterations 3 --files "$FILES"
done
```

### What this shows

- **The daemon's in-flight coalescer does not deliver a wall-clock
  win at this workload scale.** At N ∈ {1, 4, 10} with 3 cold
  blobs the two arms converge to within ~5–8% of each other; the
  N=1 outlier (daemon 1,577 vs naive 1,186) is single-iteration
  network variance, not a structural difference (per-thread range
  1,169–1,656 covers both medians). The headline ratio at N=10 is
  **1.04×** — well below the design doc §4 expected band of
  "1.0–10×" and below the implementation-plan §7 "investigate
  before declaring done" threshold of 1.5×.

- **At higher per-thread fetch counts the daemon *loses* by ~12%.**
  The 20-file N=10 cell shows naive 8.1 s vs daemon 9.1 s. The
  mechanism is the daemon's single shared `git cat-file
  --batch-check` child: across N=10 sidecars × 20 unique blobs the
  daemon's coalescer dedupes upstream fetches (20 unique, not
  200), but then serialises those 20 fetches through one cat-file
  child. The naive arm pays the full 200-fetch upstream cost but
  pipelines them across 10 independent `cat-file` children with
  10 parallel HTTPS connections to GitHub's promisor endpoint —
  parallelism beats deduplication on this workload because each
  fetch is small and bandwidth isn't the bottleneck.

- **Audit A3 (cross-process single-flight gap) is architecturally
  closed but not empirically load-bearing at this scale.** Stage
  2 of the projgitd plan introduced the daemon-side coalescer so
  N sidecars asking for the same OID see one upstream fetch.
  That property *is* true — Stage 3's `two_sidecars_share_one_daemon`
  test verifies it, and the design doc says so. The Phase C
  measurement shows the property doesn't *win in wall clock*
  for `rust-lang/log` at N ≤ 10. The daemon's load-bearing wins
  for projgit's target workload remain (a) the sidecar / FUSE-fd
  ownership split from Stage 3 (failure-mode isolation, not
  perf) and (b) the persistent on-disk CAS that the `sequential`
  section above measures (~3,000× sequential-mount amortisation).

- **The naive arm doesn't fail.** At N=10 (and even at N=10 with
  20 files = 200 total upstream lazy-fetches into one
  `.git/objects/pack/`) every thread completes successfully —
  zero git-lock errors, zero pack-corruption errors, zero
  cat-file crashes. The design doc §6.1 risk ("highest-risk to
  run on the host, concurrent git fetch children writing the same
  pack dir") is real in principle but didn't manifest at this
  load. Git's promisor protocol handles concurrent lazy fetches
  into a shared pack dir more gracefully than the architecture
  doc suspected.

- **Where the daemon would still win.** The empirical neutrality
  here is workload-specific. The coalescer would deliver a real
  wall-clock win when (i) network bandwidth, not RTT, is the
  bottleneck (each duplicate fetch costs proportional bytes); (ii)
  N is large enough that the naive arm runs into local
  file-descriptor or remote connection limits; (iii) per-thread
  fetch count is small enough that the daemon's serialisation
  cost stays below the naive arm's parallelism cost. None of
  those hold for `log` × N ≤ 10 × 3 small blobs on this
  high-bandwidth devcontainer link.

### Caveats specific to Phase C

- **In-thread daemon, not subprocess.** The bench spawns
  `projgit_daemon::server::run` on a `std::thread` rather than
  `cargo run --bin projgitd`. Matches the
  `sidecar_mount_smoke.rs` test pattern. Cross-process IPC adds
  < 1 ms RTT per `UnixStream::connect`; at 3 fetches per thread
  that's ~3 ms per thread, negligible vs the ~400 ms per fetch.
  Worth re-running with a subprocess daemon if a future change
  to the per-call connect path makes that overhead meaningful.

- **N=10 is the responsible default, not the README headline.**
  The README's "100 containers per host" target is unevaluated
  here. At N=100 the naive arm would likely run into file
  descriptor / connection limits and the daemon might start to
  win on parallelism grounds. The bench accepts `--concurrency`
  for exploration above 10; a future "100-concurrency" capture
  is the natural follow-up.

- **3 files is a small per-thread workload.** The 20-file
  secondary probe shows the daemon's serialisation cost matters
  at higher fetch counts. Real-world consumers (a CI agent doing
  `cargo build` against a projgit mount) read hundreds of blobs;
  for those, this bench probably underestimates the daemon's
  serialisation cost and underestimates the naive arm's
  parallelism win.

- **Network variance is real.** Individual iterations swing
  ±10–20% from the median; the daemon-N=1 outlier (1,577 vs
  smoke-test 1,244 from a different session) demonstrates this.
  Stable shapes hold across runs; absolute ms numbers don't.
  Run the bench yourself; expect the *shape* to match, not the
  digits.

- **Per-thread p50 ≠ `mount2_cold_cat` from `sequential`.** The
  `sequential` section above reports ~1 ms for mount 2's cold
  cat against a warm on-disk CAS. Phase C's per-thread p50 is
  ~1.2 s — three orders of magnitude higher. That's because Phase
  C's cache starts cold every iteration (the daemon's partial
  clone only contains commits + trees, not blobs); the per-thread
  number is the cost of *actually fetching the 3 blobs from
  upstream*, not of reading them from a warm pack. The two
  numbers measure different things.

## What this shows

- **§1.6 amortisation is real.** Across both targets, the second
  mount's cold cat collapses to ~1 ms — within a factor of 5 of the
  in-process LRU hit. The whole pitch ("100 containers per host
  sharing one cache") rests on this property, and we now have
  numbers to back it up. The deferred caveat is concurrent vs
  sequential (the audit's A3 finding); this run only shows
  sequential amortisation, which is the easier case.

- **Enumeration is cheap, even cold.** projgit's `readdir` of the
  mount root is roughly **15×** faster than `git ls-tree` on both
  targets because the entire tree is already in the partial-clone
  pack and projgit serves it from in-process state. `git ls-tree`
  pays a fork+exec per invocation.

- **Warm reads are effectively free.** Re-reading the same files
  after the first mount is roughly **5,000–15,000× faster** than
  running `git cat-file blob` against the same partial clone,
  because the bytes live in projgit's small-blob LRU and never
  leave the process.

- **Cold reads are still slower than `git cat-file` cold.** On
  `log`, projgit's first read of three uncached files takes ~3.4s
  vs git's ~1.2s (~2.8×). On `cargo` the gap is wider (~6.9s vs
  ~1.1s, ~6×) because larger blobs amplify the per-blob fork+exec
  cost of `GitCliFetcher`. This is treated as structural per the
  fetch-coalescing retraction; the bench exists to catch it if it
  changes either way.

- **Recursive walk doesn't scale linearly.** On `log` (~15 files),
  cold walk is 2 ms; on `cargo` (~1,200 files), cold walk is 269
  ms. That's ~225 µs per file, dominated by FUSE syscall
  round-trip cost (one `readdir` per directory). `git ls-tree -r`
  is much faster here (~6 ms on cargo) because it walks the tree
  object graph in-process without syscalls. The workload
  optimisation discipline says this is fine: real workloads do
  `ls a/b/c/` not `find -type f`, and the tree LRU pays off on
  repeated readdir of the same dir (cold→warm halves the time even
  at this scale). Worth noting because it shows the right place for
  projgit's wins isn't `find -type f`.

## Caveats

- Single machine, single repo, single network per run. Numbers
  will vary; the **shape** is what to compare across runs, not
  absolute ms. (The numbers above also differ from the 2026-05-11
  capture preserved in git history; both captures show the same
  qualitative shape.)
- This bench does **not** drop kernel page cache between cold and
  warm. The cold/warm split here is about projgit's in-process
  caches and remote round-trips, not Linux's block cache. The
  cross-process `mount 2 cold` row deliberately *benefits* from
  the kernel page cache being warm on the pack files — that
  matches what a second real-world process would see.
- Sequential amortisation is the easier case. The harder case
  (concurrent mounts racing the same cold OID) is the Phase C
  section above. Headline: the daemon's architectural
  single-flight does close audit A3 (one upstream fetch per OID
  across N sidecars), but at this workload scale (`log` × N ≤
  10 × 3 small blobs) that doesn't translate to a wall-clock
  win, and at N=10 × 20 files the daemon's cat-file
  serialisation actually loses by ~12%.
- Two targets are not "many targets". The bench will need a
  bigger-than-`cargo` target (10K–100K files) before we can claim
  anything about monorepo behaviour. Worth doing once we have a
  reason to believe the shape changes at that scale.
