# Design: `virtio-fs` Host Serving & Uncompressed On-Disk Blob Cache

> Status: **implemented (L2 uncompressed blob cache, `--isolated-dotgit`, and `crates/projgit-virtiofs`).** Extends [`container-deployment.md`](container-deployment.md),
> [`projgitd.md`](projgitd.md), and [`cache-transform-tier.md`](cache-transform-tier.md) §9
> (Organ 2 — the decoded-content store) to sandboxes running inside
> **microVMs** (QEMU, Cloud Hypervisor, Kata Containers, Firecracker-class VMMs).
>
> Shows why microVM deployment makes **Organ 2** (an uncompressed,
> OID-keyed on-disk blob cache on the host) load-bearing rather than a
> tail optimization: it eliminates per-read decompression across VMs,
> enables `O(range)` random reads, and unlocks **`virtio-fs` DAX** so
> multiple microVMs share the same physical host Page Cache pages.

## 0. Problem: Mounting `projgit` into a microVM sandbox

[`container-deployment.md`](container-deployment.md) and
[`projgitd.md`](projgitd.md) assume standard Linux containers sharing a
single host kernel (where a host FUSE mount can be bind-mounted via
`-v ...:ro,rslave`, or a sidecar can mount `/dev/fuse` inside the
container's mount namespace while reading a shared host CAS).

When each agent sandbox runs inside its own **microVM** with a separate
guest kernel, host bind-mounts no longer cross the VM boundary. Two
architectural choices exist:

- **Option A — FUSE daemon inside the guest (`/dev/fuse`):**
  Run `projgit` (or the sidecar) inside every microVM.
    - *Security boundary loss:* the untrusted agent shares a kernel and
    VM with the storage daemon; either git remote credentials or the
    shared CAS must be exposed inside the guest.
    - *RAM multiplication:* every sandbox pays for its own daemon binary,
    in-memory LRUs, and guest page-cache copies of every read file.
- **Option B — `virtio-fs` (FUSE over shared-memory virtqueues on the host):**
    1. **Guest side (`mount -t virtiofs`):** The guest kernel speaks the
     standard FUSE wire protocol (`FUSE_LOOKUP`, `FUSE_READ`,
     `FUSE_GETATTR`), but places requests directly into a **`virtqueue`
     shared-memory ring** instead of `/dev/fuse`.
    2. **Host side (`vhost-user-fs`):** The trusted `projgit` serving
     process runs **on the host**, pops FUSE requests from the
     virtqueue over a `vhost-user` Unix socket connected to the VMM,
     and serves them against the host's shared store.
    3. **Optional `DAX` window (`FUSE_SETUPMAPPING`):** `virtio-fs` can
     expose a host memory window (`DAX`) to the guest. Instead of
     copying bytes through the virtqueue into the guest's page cache,
     the host daemon instructs the VMM to map a **host file descriptor +
     offset** directly into guest physical address space—so $N$
     microVMs on the same physical host map the **same host Page Cache
     pages** with zero guest-RAM duplication.

Option B preserves the security and multiplexing invariants of
[`projgitd.md`](projgitd.md): no credentials, raw packs, or daemon
processes ever enter the microVM.

## 1. Why Git's pack format blocks `DAX` and fast host serving

In `projgit` today ([`object_store.rs`](../../crates/projgit-core/src/object_store.rs)),
hydrated objects live on disk exclusively inside canonical Git
packfiles (`.git/objects/pack/*.pack`), where blobs are **zlib-stream
compressed and delta-chained**.

Serving a file read (`ProjectionFsProvider::read`) from a packfile has
three consequences:

1. **`DAX` (`FUSE_SETUPMAPPING`) is impossible against `.pack` files.**
   `DAX` maps raw bytes from a host `fd` at `(offset, len)` directly
   into the guest's address space as the file's contents. Because bytes
   inside a `.pack` file are compressed/deltified, mapping the pack `fd`
   would hand the guest compressed garbage instead of the file.
2. **Repeated CPU inflate + buffer allocation.** For any blob larger
   than the in-process `BlobCache` entry cap (64 KiB) or evicted from
   it, `ObjectStore::read_blob` decodes the entire git blob into a
   fresh heap `Vec<u8>` on every `read()` call and slices `[offset ..
   offset + size]`.
3. **Whole-blob work for partial reads.** Reading a 4 KiB page at
   offset 10 MiB reconstructs and inflates all 10+ MiB of the blob.

## 2. The on-disk change: Uncompressed OID-keyed Blob Cache (Organ 2)

To serve microVMs (and host containers) directly from the host OS page
cache, we materialize [`cache-transform-tier.md`](cache-transform-tier.md)
§9's **Organ 2 (the decoded-content store)** on disk beside the
canonical Git ODB:

```text
<git-dir>/
├── objects/
│   └── pack/                  # Canonical Git ODB (commits, trees, fetched packs)
└── blobs/                     # Organ 2: uncompressed, flat, OID-keyed blob files
    ├── 00/
    │   └── 00a1b2c3...        # Exact inflated bytes of blob OID 00a1b2c3...
    ├── 01/
    ...
    └── tmp/                   # Staging dir for atomic write + rename
```

### 2.1 Invariants

1. **Canonical ODB remains the acquisition format.** `projgitd` still
   fetches from remotes via stock Git pack transfer into
   `.git/objects/pack/`. Commits and trees stay in the canonical ODB
   (and in-memory `TreeCache`/`HeaderCache`).
2. **Inflate-once, write-atomic.** When a blob OID is hydrated (via
   on-demand `Fetch`, `FetchMany` prefetch, or `warm`) or first read:
    - Check if `blobs/<oid[0..2]>/<oid>` already exists on disk.
    - If absent, decode the blob once from the canonical ODB into
         `blobs/tmp/<oid>.<tid>`, flush, mark read-only (`0444`), and
         atomically `rename()` to `blobs/<oid[0..2]>/<oid>`.
         writers produce identical bytes; atomic rename is lock-free and
         never invalidates existing open file descriptors or `mmap`s.
    - Because Git blobs are content-addressed and immutable, concurrent
3. **Serving reads via host `fd` (`pread` / `mmap` / `DAX`).**
    - **Standard FUSE / virtqueue `FUSE_READ`:** `ObjectStore` opens
     `blobs/<xx>/<oid>` (or caches `Arc<File>` in an fd LRU) and
     executes `FileExt::read_at(buf, offset)` (`pread` on Unix).
     Reading 4 KiB at offset 10 MiB touches only that 4 KiB page in the
     host page cache—no heap allocation of the full blob, no zlib
     inflate on warm reads.
     `lookup`, the host `vhost-user-fs` daemon ensures
     `blobs/<xx>/<oid>` is materialized, keeps its host `RawFd`, and
     hands `(fd, file_offset, len)` to the VMM's DAX window handler.
     Every microVM reading that blob OID maps the **same physical host
     pages**.
    - **`virtio-fs` with `DAX` (`FUSE_SETUPMAPPING`):** On `open` /

## 3. End-to-end architecture with `projgitd` + `virtio-fs`

```text
Host (trusted):
  ┌─────────────────────────────────────────────────────────────────┐
  │  projgitd (1 per host)                                          │
  │    • owns sole upstream connection + Coalescer + PrefetchClaims │
  │    • writes fetched packs to <cache>/repo/.git/objects/pack/    │
  │    • materializes uncompressed blobs to <cache>/repo/blobs/     │
  └──────────────┬───────────────────────────────────┬──────────────┘
                 │ unix socket (miss trigger)        │ shared host FS
                 │                                   ▼
                 │                      ┌───────────────────────────┐
                 │                      │ <cache>/repo/blobs/xx/oid │
                 │                      │   (Host OS Page Cache)    │
                 │                      └──────┬─────────────┬──────┘
                 ▼                             │ fd / pread  │ fd / DAX
  ┌──────────────────────────────┐             │             │
  │ Host VFS Sidecar 1           │◀────────────┘             │
  │  (ProjectionFsProvider       │                           │
  │   + vhost-user-fs adapter)   │   ┌───────────────────────┘
  └──────────────┬───────────────┘   │
                 │ vhost-user sock   │ FUSE_SETUPMAPPING (DAX window)
                 ▼                   ▼
  ═══════════════╪═══════════════════╪═══════════════════════════════ VMM boundary
  MicroVM 1 (untrusted guest):       │
    /repo (mount -t virtiofs) ───────┘ (maps host page-cache pages directly)
```

### Two ways to run the `virtio-fs` host side

1. **Path 1 — Immediate (`virtiofsd` over a host `projgit` FUSE mount):**
   Run `projgit mount --daemon-socket ...` on the host at
   `/run/projgit/<mnt-id>`, and point stock `virtiofsd
   --shared-dir=/run/projgit/<mnt-id>` at it for the microVM.
    - *Pros:* Zero new transport code; benefits immediately from the
         uncompressed `blobs/` cache (fast `pread`, no repeated zlib
         inflate).
         `virtiofsd` → `host /dev/fuse` → `projgit`), and `DAX` cannot pass
         through a userspace FUSE mount without kernel FUSE-passthrough.
    - *Cons:* Double FUSE hop on the host (`guest virtiofs` →
2. **Path 2 — Native (`crates/projgit-virtiofs` crate speaking FUSE + `DAX`):**
   [`crates/projgit-virtiofs`](../../crates/projgit-virtiofs) is a sibling
   crate to `projgit-fuse` that adapts
   [`projgit_core::FsProvider`](../../crates/projgit-core/src/fs_provider.rs)
   directly to the FUSE 7.38 + `virtio-fs` `DAX` wire protocol
   (`ProjgitVirtioFs<F, M>`):
    - Decodes and encodes standard little-endian FUSE frames (`FUSE_INIT`,
      `FUSE_LOOKUP`, `FUSE_GETATTR`, `FUSE_READLINK`, `FUSE_OPEN`, `FUSE_READ`,
      `FUSE_OPENDIR`, `FUSE_READDIR`, `FUSE_STATFS`, `FUSE_SETUPMAPPING` = 48,
      `FUSE_REMOVEMAPPING` = 49) with `#![forbid(unsafe_code)]`.
    - Dispatches `FUSE_SETUPMAPPING` through `FsProvider::backing_file(nodeid)`
      and installs the open `Arc<File>` into a configurable `DaxMapper`
      (`MemoryDaxWindow` / `RecordedDaxMapper`) at `(moffset, len, foffset)`.
      In-memory synthetic files (`.git/HEAD`, `.git/index`) return `-ENOSYS`
      so the guest kernel falls back to `FUSE_READ` over the virtqueue.
    - Provides length-prefixed stream framing (`server::serve_stream`) and a
      Unix domain socket server (`server::serve_unix_socket` /
      `server::spawn_unix_socket_server`).
    - On Linux (`#[cfg(target_os = "linux")]`), [`vhost_user.rs`](../../crates/projgit-virtiofs/src/vhost_user.rs)
      wires `ProjgitVirtioFs` directly into the `rust-vmm` stack
      (`vhost-user-backend`, `vhost`, `virtio-queue`, `vm-memory`):
      - `VhostUserFsBackend<F, M>` implements `VhostUserBackendMut` with
        2 queues (`queue 0` high-priority `FUSE_FORGET`/`FUSE_BATCH_FORGET`,
        `queue 1` request queue), `VIRTIO_F_VERSION_1 | VIRTIO_RING_F_INDIRECT_DESC | VIRTIO_RING_F_EVENT_IDX`,
        40-byte `virtio_fs_config` (`get_config`), and a single DAX window
        (`get_shmem_config`).
      - `VhostShmemDaxMapper` wraps `MemoryDaxWindow` and forwards
        `FUSE_SETUPMAPPING` / `FUSE_REMOVEMAPPING` over the `vhost-user`
        backend request channel (`VhostUserFrontendReqHandler::shmem_map`
        passing the host `File` descriptor over `SCM_RIGHTS`, and
        `shmem_unmap`).
      - Exposed on the CLI as `projgit virtiofs <SOURCE> --socket <PATH> [--tag <TAG>]`
        (defaults to the isolated `.git/` overlay so guest microVMs never see
        host `alternates` paths).

## 4. Extending `FsProvider` for file-descriptor / `DAX` serving

Today [`FsProvider::read`](../../crates/projgit-core/src/fs_provider.rs)
returns `Result<Vec<u8>, FsError>`. Supporting both `pread` and `DAX`
cleanly requires only an additive method on `FsProvider` (and
`ObjectStore`):

1. **`ObjectStore::ensure_uncompressed_blob(&self, oid: ObjectId) -> Result<Arc<File>, ObjectStoreError>`**
    - Opens `blobs/<xx>/<oid>` if present; otherwise decodes the blob
     from `gix`, writes it atomically to `blobs/<xx>/<oid>`, and returns
     an open `Arc<File>`.
     uses `pread` (`read_at`) on that `File`, replacing the whole-blob
     `read_blob` + slice in `ProjectionFsProvider::read`.
    - `ObjectStore::read_blob_range(&self, oid: ObjectId, offset: u64, size: u32)`
2. **`FsProvider::backing_file(&self, ino: u64) -> Result<Option<Arc<File>>, FsError>`** (optional hook, defaults to `Ok(None)`)
    - For `AttrSnapshot::TreeFile { oid, .. }`, hydrates `oid` if needed
     and returns `Some(file)` pointing to `blobs/<xx>/<oid>`.
    - For `AttrSnapshot::SyntheticHostFile { path, .. }` (such as projected
     `.git/objects/pack/pack-*.{pack,idx}` and `.git/objects/info/commit-graph`),
     opens `path` read-only and returns `Some(file)` so `DAX`
     (`FUSE_SETUPMAPPING`) and FUSE passthrough can map packfiles and
     indices directly from the host Page Cache.
    - For in-memory synthetic overlay files (`.git/HEAD`, `.git/index`),
     returns `None` (falling back to normal virtqueue `read()`).

## 5. `.git/` synthesis caveat inside microVMs

[`crates/projgit-core/src/dotgit.rs`](../../crates/projgit-core/src/dotgit.rs)
synthesizes `.git/objects/info/alternates` in [`a1_overlay`] / [`a1_plus_overlay`]
containing the **host filesystem path** to `<cache-dir>/<repo>/.git/objects`.

- In a same-host container where the shared cache volume is mounted at
  the same path, `git log` / `git rev-parse` inside the mount follows
  `alternates` directly.
- Inside a **microVM**, the host path in `alternates` does not exist
  unless the canonical `.git/objects` directory is also exposed inside
  the guest, and exposing host paths inside an untrusted guest is
  undesirable.
- **Implemented fix for microVMs (`a1_isolated_overlay` & `a1_plus_isolated_overlay`):**
  1. **Option 1 (Index + HEAD isolation):** omits `.git/objects/info/alternates`
     while synthesizing `.git/HEAD`, `.git/config`, `.git/packed-refs`,
     `.git/refs/{heads,tags}`, and a read-only `.git/index` (`ASSUME_VALID`
     on every entry) so `git rev-parse HEAD`, `git status`, `git diff`,
     and `git ls-files` work with zero host-path leakage.
  2. **Option 2 (Host-backed pack & commit-graph projection via `SyntheticEntry::HostFile`):**
     `a1_isolated_overlay(commit_oid, objects_dir)` and
     `a1_plus_isolated_overlay(store, commit_oid)` scan `<objects_dir>/pack/*`
     (`*.pack`, `*.idx`, `*.rev`, `*.mtimes`, `multi-pack-index`) and safe
     metadata under `<objects_dir>/info/` (`commit-graph`, `packs`,
     `commit-graphs/*`) and project each regular file into `.git/objects/`
     as a read-only `SyntheticEntry::HostFile { path, size }` (`0o444`).
     `ProjectionFsProvider` serves byte slices from these entries via
     positioned reads (`pread` / `read_at` on Unix, `seek_read` on Windows)
     without loading `.pack` files into heap `Vec<u8>` buffers, and exposes
     their open `Arc<File>` handles via `FsProvider::backing_file` for
     `virtio-fs` `DAX` / FUSE passthrough. As a result, `git log`,
     `git show`, and `git cat-file` work self-contained inside a single
     `virtio-fs` microVM mount with no `alternates` file and no extra mounts.
  3. **CLI & daemon `--isolated-dotgit` flag:**
     `projgit mount --isolated-dotgit`, `projgit mount-multi --isolated-dotgit`,
     and `projgit attach mount --isolated-dotgit` (plus `isolated_dotgit: true`
     on `projgit-daemon`'s `Request::Mount`) select `a1_plus_isolated_overlay`
     (combined with A2 branch ref visibility when the projection is a local
     branch) instead of `a1_plus_overlay`.

## 6. `composefs`-Inspired Kernel Acceleration (Zero Warm Upcalls)

Because every `projgit` projection is an **immutable snapshot of a single Git commit**, we combine the best properties of Linux `composefs` (EROFS + `overlayfs` + shared CAS) with `projgit`'s lazy on-demand partial-clone hydration:

1. **Infinite Kernel `dcache` / `icache` / Page Cache Retention (`FOPEN_KEEP_CACHE` + `FOPEN_CACHE_DIR`):**
   Both `projgit-fuse` and `projgit-virtiofs` return `entry_valid = 86_400s`, `attr_valid = 86_400s`, `FOPEN_KEEP_CACHE` on `FUSE_OPEN`, and `FOPEN_KEEP_CACHE | FOPEN_CACHE_DIR` on `FUSE_OPENDIR` (alongside `FUSE_CACHE_SYMLINKS` and `FUSE_EXPLICIT_INVAL_DATA`). Once a directory or file attribute is read once, subsequent `stat()`, `readdir()`, and path walks inside the sandbox are served 100% from kernel `dcache`/`icache`/Page Cache with **zero userspace FUSE upcalls**.
2. **`FUSE_READDIRPLUS` (Single-Upcall Directory + Attribute Batching):**
   `FsProvider::readdirplus` and `ProjectionFsProvider::readdirplus` batch-warm child blob headers in a single pass (`prefetch_headers`) and return `DirEntryPlus` (`inode + name + FileAttr`) for all children at once. Both `projgit-fuse` (`Filesystem::readdirplus`) and `projgit-virtiofs` (`FuseOpcode::Readdirplus = 44`, `FuseDirentplus`) negotiate `FUSE_DO_READDIRPLUS | FUSE_READDIRPLUS_AUTO | FUSE_PARALLEL_DIROPS`, eliminating $N$ per-file `FUSE_LOOKUP` round-trips during `ls -l`, `find`, or `rg`.
3. **Linux 6.9+ `FOPEN_PASSTHROUGH` (`projgit-fuse`):**
   When the host kernel advertises `FUSE_PASSTHROUGH`, `projgit-fuse` negotiates `InitFlags::FUSE_PASSTHROUGH` (`max_stack_depth = 1`) and wires `FsProvider::backing_file(ino)` directly to `ReplyOpen::open_backing` + `ReplyOpen::opened_passthrough` (caching one `Arc<BackingId>` per inode). After `open()` hydrates `<git-dir>/blobs/<xx>/<oid>` (or opens a projected `.git/objects/pack/*.pack` `HostFile`), all subsequent `read()` and `mmap()` syscalls go straight to the host ext4/xfs backing file in the kernel with **zero FUSE daemon round-trips** and **cross-sandbox host Page Cache `folio` deduplication** — identical to `composefs` on the host, while `projgit-virtiofs` `DAX` (`FUSE_SETUPMAPPING` / `SHMEM_MAP`) provides the same page-cache sharing across a microVM boundary.
4. **Writable `overlayfs` Scratch Layer (`--writable-scratch <DIR>`):**
   `projgit mount --writable-scratch <DIR>`, `projgit mount-multi --writable-scratch <DIR>`, and `projgit attach mount --writable-scratch <DIR>` (via `OverlayScratchConfig` and `ManagedMountSession`) mount the read-only FUSE projection on `<scratch>/lower` and stack a kernel `mount -t overlay` (`lowerdir=<scratch>/lower,upperdir=<scratch>/upper,workdir=<scratch>/work`) at the target mountpoint. Sandboxes get full POSIX copy-on-write (`open(O_WRONLY)`, `unlink`, `rename`, `mkdir`, `cargo build`, `git commit`) in `upperdir` while unmodified files remain lazily hydrated and shared from the read-only CAS.
5. **Persistent On-Disk Header Memo (`<git-dir>/blobs/headers.bin`):**
   `ObjectStore` persists resolved `(ObjectId -> ObjectKind, u64 size)` records in an append-only binary memo (`<git-dir>/blobs/headers.bin`, magic `PGHD\x00\x00\x00\x01`, 29 bytes per record) behind the in-memory `HeaderCache`. Across daemon restarts or subsequent mounts of the same partial clone, `stat()` and `readdirplus()` resolve blob sizes in $O(1)$ from memory without touching packfiles or spawning `git cat-file --batch-check`.

## 7. Trade-offs & disk management

- **Disk amplification vs. packfiles:** Uncompressed blobs take ~2–4×
  more disk space than delta-compressed packfiles, *for the subset of
  blobs actually touched or prefetched*. Because `projgit` only
  materializes `blobs/<xx>/<oid>` for blobs that are read/prefetched
  (Architecture A/B in [`cache-transform-tier.md`](cache-transform-tier.md)),
  untouched blobs in a 140 GB repo stay un-materialized.
- **Eviction is trivial (`prune_uncompressed_blobs` & `--blob-cache-max-bytes`):**
  Because `.git/objects/pack/` remains the canonical source of truth,
  `blobs/<xx>/<oid>` is a pure rebuildable L2 cache.
  [`projgit_core::prune_uncompressed_blobs`](../../crates/projgit-core/src/maintenance.rs)
  (and [`ObjectStore::prune_uncompressed_blobs`](../../crates/projgit-core/src/object_store.rs))
  scans only the 2-hex-character fanout directories (`00`..`ff`) under
  `<git-dir>/blobs/` — never touching `<git-dir>/blobs/headers.bin`,
  `<git-dir>/blobs/tmp/`, or `.git/objects/pack/` — and unlinks the
  oldest files by `atime`/`mtime` until the remaining bytes fit within
  `max_bytes`. `projgitd` enforces this budget automatically on each
  background maintenance tick when `--blob-cache-max-bytes <BYTES>` (or
  `PROJGIT_BLOB_CACHE_MAX_BYTES`) is set alongside
  `--maintenance-interval-secs`. Even if a blob is currently mapped via
  `FOPEN_PASSTHROUGH` or `virtio-fs` `DAX`, POSIX `unlink` keeps the
  open inode alive until the last file descriptor closes, and any future
  read transparently re-inflates the blob from `.git/objects/pack/`.
