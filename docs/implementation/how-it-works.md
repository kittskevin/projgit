# How projgit works today (as implemented)

> Status: **current as of 2026-06-19.** Describes the system *as built*,
> not as designed — every claim here is backed by code in `crates/` and
> by the tests/demo at the bottom. Design rationale lives in
> [`../design/cache-transform-tier.md`](../design/cache-transform-tier.md);
> the build sequence in
> [`cache-transform-tier-plan.md`](cache-transform-tier-plan.md). When
> the two disagree, *this* document follows the code.

## 1. What projgit does

projgit mounts a **read-only virtual filesystem** that projects a git
commit's tree, fetching objects lazily from a stock-git remote (or
serving them from a local repo). Files appear with real `size` / `mode`
/ `mtime` in directory listings *without* their bytes being fetched;
the bytes arrive on first `read()`. A synthesized `.git/` makes
git-aware tooling work inside the mount.

It runs in two modes:

- **Standalone** — one process owns the object store, the fetcher, and
  the FUSE mount (`projgit mount`).
- **Daemon + sidecar** — a per-host `projgitd` owns the upstream
  connection + shared CAS; per-mount sidecars (`projgit mount
  --daemon-socket`) serve FUSE locally and coordinate cold fetches with
  the daemon. This is the multiplexing path for the agent-eval workload
  ([`../problem-statement.md`](../problem-statement.md) §1).

## 2. The pieces (by crate)

```
projgit-core
  object_store.rs    ObjectStore  — read-only gix wrapper + tree/blob/header LRUs
  fetcher.rs         Fetcher trait (fetch_object, prefetch_headers, fetch_objects)
                     HydratingObjectStore — read-on-miss + warm_tree_closure
                     GitCliFetcher / GvfsFetcher / GixFetcher / NoopFetcher
  prefetch.rs        background worker: Headers (T1) + Blobs (Arch B) tasks
  maintenance.rs     run_maintenance — git MIDX + repack + commit-graph
  projection.rs      Projection (Ref / Commit / Subtree) -> tree/commit OIDs
  projection_fs.rs   ProjectionFsProvider — the FUSE provider (readdir/lookup/read)
  dotgit.rs/overlay  synthesized `.git/` (RootOverlay)

projgit-fuse         FUSE adapter over the FsProvider trait
projgit-winfsp       Windows adapter (deferred)

projgit-daemon
  protocol.rs        Request/Response wire (Ping/Status/Mount/Umount/Attach/
                     Fetch/FetchMany/PrefetchHeaders/Shutdown)
  server.rs          DaemonState, dispatch + handlers, background maintenance_loop
  fetcher.rs         DaemonFetcher — a Fetcher that proxies cold fetches to projgitd

projgit-cli          `projgit mount` (standalone + sidecar) and `projgitd`
```

## 3. The object layers

Three layers, bottom to top:

1. **Shared CAS** — a stock git odb (partial clone for URL sources).
   Holds loose objects + packs; readable by any git tool. One per
   source; shared on disk across mounts.
2. **`ObjectStore`** ([object_store.rs](../../crates/projgit-core/src/object_store.rs)) —
   a read-only `gix` wrapper with three in-process LRUs: parsed **trees**,
   small **blobs** (≤64 KiB), and **headers** `(kind, size)`. Never
   networks; raises `MissingObject(oid)` on a miss.
3. **`HydratingObjectStore`** ([fetcher.rs](../../crates/projgit-core/src/fetcher.rs)) —
   wraps an `ObjectStore` + a `Fetcher`. `read_blob` / `read_tree` /
   `header` turn `MissingObject` into hydrate-then-retry. This is the
   only place a miss touches the network.

## 4. The `Fetcher` trait — how objects arrive

[`Fetcher`](../../crates/projgit-core/src/fetcher.rs) has three methods:

| Method | Guarantees | Used by |
|---|---|---|
| `fetch_object(oid)` | one object resident | on-demand `read`/`stat` miss (correctness floor) |
| `prefetch_headers(oids)` | each present OID's **header** decodable | T1 readdir prefetch (warms `stat` sizes) |
| `fetch_objects(oids)` | the objects' **bytes** resident | eager tree warm + Arch-B blob prefetch |

Implementations: **`GitCliFetcher`** (default for URLs) keeps a pool of
long-lived `git cat-file --batch-check` children; resolving a missing
object's header in a partial clone triggers git's promisor fetch, so a
header batch doubles as a bytes batch. **`GvfsFetcher`** speaks GVFS v1
(`/gvfs/objects`, `/gvfs/sizes`). **`NoopFetcher`** never hydrates (local
sources / `--offline`). `fetch_objects` skips already-resident OIDs
before calling the fetcher, so present objects are reported `Present`
regardless of backend.

## 5. The FUSE provider — per-callback behavior

[`ProjectionFsProvider`](../../crates/projgit-core/src/projection_fs.rs)
implements the read path:

- **`readdir(dir)`** — lists the tree (real entries, **blob-free**: no
  size fetched here), allocates stable inode numbers, and posts the
  directory's file/symlink blob OIDs to the prefetch worker via
  `post_prefetch`. Returns name + type + inode per entry.
- **`lookup` / `getattr`** — resolves `(kind, mode, size, mtime, ino)`.
  Size comes from `header()` (warm if the prefetch worker already
  batched it). `mtime` is the commit's committer time (git has no
  per-file mtime). `ino` is synthesized by `InodeAllocator`.
- **`read(ino, off, size)`** — `read_blob` through the
  `HydratingObjectStore`: warm → local mmap; cold → fetch then serve.
- **`readlink`** — the symlink's (tiny) blob.

The provider spawns the **prefetch worker** at construction and joins it
on drop.

## 6. Prefetch — warming ahead of the kernel

[`prefetch.rs`](../../crates/projgit-core/src/prefetch.rs) runs one
background worker per provider, fed a bounded channel. Two task kinds:

- **`Headers`** (T1, always on) — batches a directory's blob OIDs into
  one `cat-file --batch-check` round trip and warms the header cache, so
  the kernel's follow-up `lookup`s (the `ls -la` size column) are local.
- **`Blobs`** (Architecture B, **opt-in**) — bulk-warms a directory's
  small-file blob *bytes* via `fetch_objects`, so a subsequent `read` is
  local. Gated by `PROJGIT_PREFETCH_BLOBS`; a size cap
  (`PROJGIT_PREFETCH_BLOB_CAP_BYTES`, default 1 MiB) skips large files so
  speculative fetch stays bounded on sparse access. The single FIFO
  worker processes a directory's `Headers` task before its `Blobs` task,
  so the cap reads warm sizes.

## 7. Eager tree warm — `os.walk` is network-free

At mount, every mount path triggers a **background** walk of the
commit's tree closure:
[`HydratingObjectStore::warm_tree_closure`](../../crates/projgit-core/src/fetcher.rs)
BFS-walks trees, batch-fetching each level via `fetch_objects` (trees
only — blobs stay lazy). After it completes, `readdir`/`stat` over the
whole tree are served locally. It's spawned off the mount-response path
(`spawn_tree_warm` in the CLI; the daemon's `handle_mount` for direct
mounts), so the mount returns immediately and the closure streams in
behind it.

## 8. The two mount flows

### Standalone (`projgit mount <src> <mnt>`)

```
open/clone CAS -> ObjectStore -> Projection(ref) -> HydratingObjectStore(Fetcher)
  -> spawn_tree_warm (background tree closure warm)
  -> ProjectionFsProvider (spawns prefetch worker)
  -> FUSE mount; serve readdir/lookup/read locally, fetch on miss
```

### Daemon + sidecar (`projgitd` + `projgit mount --daemon-socket`)

```
sidecar: Attach(source) --------> projgitd: clone/open CAS, reply git_dir
sidecar: open OWN ObjectStore on the shared git_dir (mmap'd packs)
sidecar: HydratingObjectStore(DaemonFetcher) -> provider -> FUSE mount
  hot read  : sidecar reads the shared CAS directly (no socket)
  cold miss : DaemonFetcher --Fetch{oid}-----> daemon hydrates into CAS -> Ok
  bulk warm : DaemonFetcher --FetchMany{oids}-> daemon hydrates batch  -> probes
  headers   : DaemonFetcher --PrefetchHeaders-> daemon warms headers   -> probes
```

Key property: **bulk data never crosses the socket** — the daemon writes
objects into the shared CAS and the sidecar reads them via gix's mmap;
the wire carries only fetch *triggers* and tiny header metadata
("notifications, not payloads"). One daemon owns the single upstream
connection for all sidecars; concurrent fetches for the same OID are
coalesced.

## 9. Maintenance — keeping the CAS lean

[`maintenance::run_maintenance`](../../crates/projgit-core/src/maintenance.rs)
shells `git maintenance run --task=incremental-repack --task=commit-graph`
on the shared CAS: a multi-pack-index for fast lookup across many
per-commit fetch-packs, geometric repack so each object is physically
singular (cross-commit disk + page-cache dedup), and a commit-graph for
fast history. The daemon runs it from a background thread
([server.rs](../../crates/projgit-daemon/src/server.rs) `maintenance_loop`),
off the serving path, joined on shutdown. It's promisor-safe and safe
beside live mmap readers (git writes via temp + atomic rename).

## 10. Configuration

| Surface | Knob | Default | Effect |
|---|---|---|---|
| CLI | `projgit mount --offline` | — | local source, `NoopFetcher` |
| CLI | `--no-dotgit` / `--allow-other` / `--stats` | — | overlay / ACL / counters |
| env | `PROJGIT_PREFETCH_BLOBS` | off | Architecture-B blob prefetch |
| env | `PROJGIT_PREFETCH_BLOB_CAP_BYTES` | 1 MiB | blob-prefetch size cap |
| daemon | `projgitd --maintenance-interval-secs N` | off | background maintenance cadence |
| env | `PROJGIT_MAINTENANCE_INTERVAL_SECS` | off | maintenance fallback if flag unset |
| daemon | `--pool-size`, `--cache-dir`, `--depth`, `--pid-file` | — | fetcher pool / clone config |

## 11. Worked example (live, 2026-06-19)

A nested fixture mounted with `--offline` (local source → `NoopFetcher`)
and blob prefetch on:

```
$ projgit mount /tmp/demo/repo /tmp/demo/mnt --offline --stats   # (background)
projgit: mounting at /tmp/demo/mnt (Ctrl-C to unmount)

$ ls -laR /tmp/demo/mnt          # blob-free readdir + real sizes
-rw-r--r-- 1 vscode vscode    12 app.py
drwxr-xr-x 2 vscode vscode     0 src
-rw-r--r-- 1 vscode vscode 15000 src/big.txt
-rw-r--r-- 1 vscode vscode    22 src/main.c
drwxr-xr-x 2 vscode vscode     0 src/util
# plus a synthesized .git/ (HEAD, config, index, objects/info/alternates, refs/)

$ cat /tmp/demo/mnt/app.py        # on-demand read
print("hi")

$ git -C /tmp/demo/mnt rev-parse HEAD     # git works inside the mount
5d979212a06f2868bb34e30690f0639fbad66e63
$ git -C /tmp/demo/mnt log --oneline -1
5d97921 (HEAD) demo fixture
```

Every file is visible with a real size before its bytes are read; `cat`
returns the bytes; the synthesized `.git/` makes `git` work in-tree.

## 12. What is NOT implemented (and where it's tracked)

- **Stage 2 persisted derived metadata** — the `OID→size` memo / tree-
  closure packs are deferred; the shared odb + in-process header cache
  cover the MVP ([design §9](../design/cache-transform-tier.md)).
- **Stage 4 / Architecture C (full hydrate)** — the demand-gated
  whole-commit hydrate is not built; blob prefetch + eager trees cover
  the sparse §1 workload.
- **Blob-prefetch CLI flag** — blob prefetch is env-gated only; not yet
  promoted to a `projgit mount` flag.
- **Windows (WinFsp)** — deferred.
- **Bench validation** — the cold-read-tail (Stage 3 §6.3) and
  cross-commit-dedup (Stage 5 §8.3) wins are implemented but not yet
  measured against a real remote.

## 13. Test coverage (what proves the above)

- Core: `fetch_objects` (noop), `warm_tree_closure`
  ([tests/warm_tree.rs](../../crates/projgit-core/tests/warm_tree.rs)),
  `blobs_under_cap`, `run_maintenance`
  ([maintenance.rs](../../crates/projgit-core/src/maintenance.rs)).
- Daemon: `FetchMany` protocol roundtrip + the `FetchMany` handler and
  `DaemonFetcher` batch client end-to-end
  ([fetch_smoke.rs](../../crates/projgit-daemon/tests/fetch_smoke.rs),
  [daemon_fetcher_smoke.rs](../../crates/projgit-daemon/tests/daemon_fetcher_smoke.rs)),
  plus Attach/Fetch/PrefetchHeaders and mount smokes.
- The live mount above exercises the full standalone read path.

Run `cargo test --workspace` + `cargo clippy --workspace --all-targets
-- -D warnings` (both green as of this writing).
