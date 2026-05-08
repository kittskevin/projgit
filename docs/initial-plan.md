# projgit — Initial Plan

> Status: **draft / pre-implementation**. This document captures the
> high-level design and phased plan for `projgit` before any code is written.
> It is intentionally light on syntax and heavy on decisions, scope, and
> risk. Detailed designs live in follow-up documents per phase.

## 1. TL;DR

Build a cross-platform user-mode filesystem in **Rust** that uses standard
git client protocols (smart-HTTP / SSH, protocol v2) via **gitoxide** to
lazily fetch objects into a single shared object store, then exposes one or
more read-only **projections** of that store through a FUSE / WinFsp mount.

A projection can be:

- a **ref** (e.g. `/refs/heads/main/...`),
- a **commit SHA** (e.g. `/commits/<sha>/...`), or
- a **subtree** of either.

Multiple projections can be mounted simultaneously and **share one
content-addressable object store** — every blob exists on disk at most once
regardless of how many mounts reference it.

## 2. Motivation & prior art

The "user-mode filesystem backed by the git wire protocol" idea sits in a
well-explored design space. We are not the first; the novel angle is
**git-native, open, multi-projection over a single CAS**.

- **Microsoft VFS for Git / Scalar / GVFS** — virtualizes the working tree
  on Windows via ProjFS, lazy-fetches blobs from a GVFS-protocol remote.
  Single working copy per repo.
- **Meta EdenFS (Sapling)** — FUSE / ProjFS / NFS frontends over a
  content-addressable backing store; many checkouts share one store. This
  is the closest spiritual ancestor of `projgit`, but it is **not** git-native
  (uses Mononoke/Mercurial-style backends).
- **Stock git `partial clone` + promisor remotes** — `--filter=blob:none`,
  `core.partialCloneFilter`. The protocol primitive for on-demand object
  fetch already exists in upstream git; we build on top.
- **presslabs/gitfs** — FUSE mount of a git repo where writes become
  commits. Different goal (writeable working copy) but overlapping
  mechanics.
- **git-annex / git-lfs** — partial-content patterns at the file level.

What's missing in the open ecosystem and what `projgit` aims to provide:

1. A **git-native** virtual filesystem (no custom server protocol — any
   plain git remote works).
2. **Multiple simultaneous projections** of the same object store
   (different refs, commits, or subtrees as different mountpoints).
3. **Cross-platform from day one** behind a single FS abstraction.

## 3. Scope

### In scope (MVP)

- **Language:** Rust.
- **Platforms:** Linux, macOS, Windows.
- **Mode:** **Read-only** projections.
- **Git backend:** `gitoxide` (`gix`, `gix-odb`, `gix-protocol`, `gix-pack`,
  `gix-ref`, `gix-object`).
- **Projections:** ref-as-directory, commit-as-directory, multiple
  simultaneous mounts sharing one object store.
- **Use cases:** very large monorepos; cheap inspection of many
  branches / commits at once; lightweight dev environments / containers;
  research / personal exploration.

### Out of scope (MVP, may revisit)

- Read-write semantics (commit-on-write or overlay).
- Sparse / path-filter projections.
- LFS pointer resolution.
- ProjFS backend on Windows (deferred in favor of WinFsp for code-sharing).
- macOS NFS-loopback frontend (Eden-style).
- A central long-running daemon (per-process mounts in MVP).

## 4. Key terminology

| Term | Meaning |
|------|---------|
| **Object store** | The shared, on-disk content-addressable store of git objects (loose + packed). One per `projgit` install (or per user-chosen path). |
| **Projection** | A logical view of the object store presented as a directory tree. Three kinds: `Ref(name)`, `Commit(oid)`, `Subtree(commit, path)`. |
| **Mount** | A projection materialized at an OS path via FUSE / WinFsp. |
| **Hydration** | Fetching a previously-missing object from the remote on demand and writing it to the object store. |
| **Promisor remote** | Stock git's term for a remote that promises to serve any object on request — what enables partial clones. |

## 5. Architecture

```
+-----------------------------+    +-----------------------------+
|  FS frontend (per OS)       |    |  CLI / control              |
|  - fuser  (Linux/macOS)     |    |  projgit mount/umount/ls/gc |
|  - winfsp (Windows)         |    +--------------+--------------+
|  - projfs (Windows, later)  |                   |
+--------------+--------------+                   |
               | trait FsProvider                 |
               v                                  v
+-----------------------------------------------------------+
|  Projection Engine                                        |
|   - resolves path -> (commit, tree-path)                  |
|   - tree / blob lookup, dir listings, mode mapping        |
|   - per-projection root: Ref | Commit | Subtree           |
+--------------------------+--------------------------------+
                           |
                           v
+-----------------------------------------------------------+
|  Object Store (single, shared)                            |
|   - gix-odb: loose + packed + multi-pack-index            |
|   - in-memory LRU for trees and small blobs               |
|   - "missing object" -> Fetcher                           |
+--------------------------+--------------------------------+
                           |
                           v
+-----------------------------------------------------------+
|  Fetcher / Remote Layer                                   |
|   - gix-protocol v2: ls-refs, fetch                       |
|   - bulk: initial partial clone (--filter=blob:none)      |
|   - on-demand: single-object fetch (promisor pattern)     |
|   - fallback: shell out to `git fetch --filter=...`       |
|     if gitoxide on-demand support is incomplete           |
+-----------------------------------------------------------+
```

### 5.1 FS frontend abstraction

A single `FsProvider` trait exposes the read-only operations needed by both
backends:

- `lookup`, `getattr`, `readdir`
- `read`, `open`, `release`
- `readlink`

Two backends implement it:

- **`fuser` backend** — Linux + macOS (via macFUSE or FUSE-T).
- **`winfsp` backend** — Windows. WinFsp is a well-maintained,
  FUSE-spirit-compatible Windows FS driver; the Rust `winfsp` crate gives
  us safe bindings.

ProjFS (the API behind GVFS / Scalar) is the philosophically perfect
choice on Windows but is intentionally **deferred** — WinFsp lets us share
more code with the Linux/macOS backend on day one.

### 5.2 Projection engine

Pure logic, no I/O of its own beyond what the object store provides.

- `Projection` enum: `Ref(name) | Commit(oid) | Subtree(commit, path)`.
- `TreeNavigator`: given a commit OID and a `/`-separated virtual path,
  walks tree objects to return `(child_oid, mode)` or a directory listing.
- Maps git modes to OS attributes:
  - `100644` → regular file
  - `100755` → regular file + exec bit (no-op on Windows)
  - `120000` → symlink (see §9.1)
  - `040000` → directory
  - `160000` (gitlink / submodule) → see §9.2
- Allocates stable inode / FileId values:
  `(projection_id, blob_oid, path_hash) -> u64`, cached for O(1) repeat
  lookups.

### 5.3 Object store

A thin wrapper around `gix-odb` that:

- Opens an existing `.git`-style directory.
- Resolves objects by OID.
- Returns a `MissingObject(oid)` error on miss instead of panicking.
  This is the **only** error variant the Fetcher intercepts on the hot
  path; extracting it from gix's internal error organization keeps the
  consumer pattern-match cheap and decouples us from gix's error layout.
- Surfaces an LRU cache for parsed trees (the hot path — stored as
  decoded `TreeData`, not raw bytes, so `readdir` and `lookup` skip
  re-parsing) and small blobs (≤ 64 KB; larger blobs served via mmap
  from the underlying packfile).
- Uses gix's `ThreadSafeRepository` + per-task `Repository` handle
  pattern: the store holds the `Arc`-shared base once and clones cheap
  per-thread handles on demand. This is the gix idiom for many readers
  over one repo and avoids non-`Sync` buffer state on the hot path.

The store is **read-only**. The Fetcher is the only component that
mutates the store (via gix's pack-receive APIs), and a successful fetch
is followed by an explicit re-read — there are no write methods on
`ObjectStore` itself. This keeps the read API provably side-effect-free
and lets `Arc<ObjectStore>` be shared without interior-mutation worries.

The store is also **projection-agnostic** — it never knows which mount
is asking. This is a hard architectural invariant and the reason a
single store can back many mounts with no duplication.

### 5.4 Fetcher

```rust
trait Fetcher {
    async fn fetch_object(&self, oid: ObjectId) -> Result<()>;
}
```

**Branch A confirmed by Phase 0a spike** (see
[../spikes/ondemand-fetch/RESULTS.md](../spikes/ondemand-fetch/RESULTS.md)):
MVP ships `GixFetcher`. `GitCliFetcher` is reserved as a future fallback
for environments / protocols where the gix path degrades.

- **`GixFetcher`** (in MVP) — uses `gix`'s `Remote` API to fetch a
  single OID via the protocol-v2 `want`-line (`allow-tip-sha1-in-want`
  / `allow-reachable-sha1-in-want`). Refspec form
  `+<oid>:refs/projgit/wanted/<oid>`. Cold-fetch latency observed at
  ~430 ms on a small public repo over residential broadband; warm /
  pooled latency to be measured in Phase 2.
- **`GitCliFetcher`** (deferred) — shells out to the system `git` (e.g.
  `git fetch origin <oid> --depth=1 --filter=blob:none`). Slower per
  fetch (process startup), but inherits the user's git config and
  credential helpers verbatim.

Concurrency: requests for the same OID are coalesced via a single-flight
map (`tokio::sync::Mutex<HashMap<Oid, Shared<Future>>>`) so a thundering
herd of `read()` calls hydrates the blob exactly once.

Pack proliferation (each on-demand fetch creates a tiny pack) is
mitigated by a periodic background repack triggered when pack count
crosses a threshold; design lives in Phase 2.

## 6. Phased plan

Risk is concentrated in two places: **(a)** can gitoxide do on-demand
single-object fetch, and **(b)** does our trait abstraction really cover
both `fuser` and `winfsp` semantics? Phase 0 de-risks both before any
real product code is written.

### Phase 0 — Spikes (parallelizable)

- **0a. Gitoxide on-demand fetch spike. — DONE.** Branch A confirmed.
  See [../spikes/ondemand-fetch/RESULTS.md](../spikes/ondemand-fetch/RESULTS.md).
  Outcome: MVP ships `GixFetcher`; `GitCliFetcher` deferred.
- **0b. FS hello-world.** Trivial read-only "hello.txt" mount on `fuser`
  *and* `winfsp` behind a shared trait. No git involved. Confirms the
  abstraction.
- **0c. WinFsp reparse-point round-trip. — DONE.** Verified via the
  WinFsp `memfs-x64` C++ sample (no Rust code shipped to avoid
  premature license commitment). All four consumer tools traverse
  WinFsp-served symlinks transparently as a non-admin user. See
  [../spikes/winfsp-reparse/RESULTS.md](../spikes/winfsp-reparse/RESULTS.md).
  Outcome: `Native` mode default in
  [windows-symlinks.md](./design/windows-symlinks.md) confirmed.

### Phase 1 — Object store + path resolver (no FS yet)

1. Add `gix` as a dependency; build `ObjectStore` wrapper around `gix-odb`.
2. Build `TreeNavigator` over `gix-object` tree parsing.
3. Define the `Projection` enum and resolver.
4. Implement `RootOverlay` — a `BTreeMap<&str, SyntheticEntry>` consulted
   by `lookup` / `readdir` at the projection root **before** falling
   through to the real tree. MVP overlay is empty; the mechanism reserves
   the option to add `.git/`, `.projgit/`, or other synthetic entries
   later without refactoring the engine. See
   [dotgit-synthesis.md](./design/dotgit-synthesis.md).
5. Unit tests against a fixture repo committed under `tests/fixtures/`,
   plus tests for `RootOverlay` with a non-empty fixture overlay.

### Phase 2 — Fetcher

6. Define `Fetcher` trait.
7. Implement `GixFetcher` and/or `GitCliFetcher` per Phase 0a outcome.
8. Implement single-flight coalescing.
9. `projgit clone` helper performs `--filter=blob:none --no-checkout`.

### Phase 3 — FS frontend

10. Define `FsProvider` trait.
11. Implement `fuser` backend (Linux + macOS).
12. Implement `winfsp` backend (Windows).
13. Implement stable inode / FileId allocation.
14. Implement the **symlink classifier + policy enum** for the Windows
    backend (`Native`, `Text`, `Auto`); see
    [windows-symlinks.md](./design/windows-symlinks.md). The bare-minimum
    `Text` mode is the floor; `Native` is gated by the Phase 0c spike
    outcome and degrades gracefully to `Text`.
15. **Per-user volume / file ownership** in the Windows backend.
    `get_security_by_name` and `get_security` must report the calling
    user as the owner so modern git's `safe.directory` check accepts
    the mount. Surfaced by Phase 0c
    ([../spikes/winfsp-reparse/RESULTS.md](../spikes/winfsp-reparse/RESULTS.md)
    finding 5).

### Phase 4 — Mount manager & CLI

16. `projgit` CLI subcommands:
    - `projgit init <store-dir> --remote <url>`
    - `projgit clone <url> <store-dir>` (init + initial partial clone)
    - `projgit mount <store> <projection-spec> <mountpoint>`
      where `<projection-spec>` is `ref:main`, `commit:<sha>`, or
      `subtree:main:src/foo`
    - `projgit umount <mountpoint>`
    - `projgit ls` — list active mounts
    - `projgit fetch [--all-refs|<ref>...]` — bulk update
17. **No daemon in MVP.** Per-process mounts share the on-disk store via
    file locks. A `projgitd` daemon can be added later without breaking the
    CLI surface.

### Phase 5 — Polish

18. LRU caches for parsed trees, small blobs, file handles.
19. `tracing`-based metrics: cache hit rate, fetch latency, bytes
    hydrated, miss count.
20. Integration tests: mount the same fixture repo twice (`ref:main` and
    `commit:<old-sha>`) simultaneously; assert correct contents and zero
    blob duplication on disk.

### Phase 6 — Future (explicitly NOT MVP)

- ProjFS backend on Windows.
- Sparse / path-filter projections.
- Read-write with overlay or commit-on-write.
- LFS pointer resolution.
- macOS NFS-loopback frontend.
- Long-running `projgitd` daemon shared by many mounts.

## 7. Dependencies

| Crate | Purpose |
|-------|---------|
| `gix` (+ `gix-odb`, `gix-pack`, `gix-protocol`, `gix-transport`, `gix-ref`, `gix-object`, `gix-hash`, `gix-traverse`) | Pure-Rust git: object DB, packfiles, wire protocol v2, refs, tree parsing. |
| `fuser` | Linux/macOS FUSE bindings. |
| `winfsp` | Windows WinFsp bindings. |
| `tokio` | Async runtime for fetcher and (future) IPC. |
| `tracing` | Structured logging + metrics. |
| `clap` | CLI parsing. |
| `anyhow` / `thiserror` | Error handling. |
| `lru`, `parking_lot` | Caches and synchronization. |

## 8. Verification plan

1. **Phase 0a outcome documented** with a working spike binary in
   `spikes/ondemand-fetch/` and a one-page note recording which fetch
   strategy we adopt and why.
2. **Unit tests** for `ObjectStore`, `TreeNavigator`, `Projection` against
   a known fixture repo (committed pack + loose objects under
   `tests/fixtures/`).
3. **Cross-platform CI** (GitHub Actions matrix Linux + macOS + Windows)
   running `cargo test`. FS-backend integration tests are feature-gated
   because CI runners may not allow FUSE / WinFsp.
4. **Manual smoke test** documented in `tests/MANUAL.md`:
   - `projgit clone https://github.com/torvalds/linux ./store` (blobless)
   - `projgit mount ./store ref:master /mnt/linux-master`
   - `projgit mount ./store commit:v6.0 /mnt/linux-v6.0`
   - `cat /mnt/linux-master/Makefile` triggers a fetch; second `cat` is a
     cache hit.
   - Confirm only one copy of each blob exists on disk under `./store`.
5. **Object-store sharing test:** mount two projections of the same
   commit; `du -sh ./store` before and after reading the same large file
   from both mounts; size delta must equal **one** copy of the blob, not
   two.

## 9. Open design questions

These need a decision before Phase 3 lands. Defaults below are the
recommended choice.

### 9.1 Symlinks on Windows — **decided**

Git mode `120000` has no clean Windows equivalent: real symlinks need
`SeCreateSymbolicLinkPrivilege` or Developer Mode, and the file-vs-directory
kind must be declared at creation time. Full rationale and per-symlink
classification algorithm: [windows-symlinks.md](./design/windows-symlinks.md).

**Decisions:**

1. **Default mode = `Auto`.** WinFsp serves reparse points from the
   filesystem side, so end users do not need
   `SeCreateSymbolicLinkPrivilege`. We try `Native` reparse points first
   and degrade to `Text` only if the reparse-point path is unavailable.
   Overridable per-mount via `--symlinks={native|text|auto}`.
2. **Out-of-tree targets emit a file-symlink + warning.** Matches POSIX
   behavior for dangling links and what stock git does. We log via
   `tracing` so `projgit doctor` can surface guesses.
3. **`Text`-mode marker = NTFS Alternate Data Stream** (`:projgit.symlink`).
   Discoverable via `dir /R`. Marker intentionally does **not** survive a
   copy off the projgit mount.

### 9.2 Submodules (gitlinks `160000`) — **decided**

A gitlink stores another repo's commit OID in the parent tree. We have
no authoritative way to fetch or project that other repo without
additional configuration (`.gitmodules` parsing, second-remote handling,
recursive fetch policy).

**Decision: A — empty directory.** Gitlink entries are projected as
empty directories. Predictable, no fetch required, no surprises. Users
who want a submodule's contents can `projgit mount <store>
commit:<submodule-oid> <mountpoint>` explicitly.

Deferred for post-MVP consideration:

- **B. Auto-mount the submodule commit recursively.** Big surface area
  (`.gitmodules` parsing, recursive fetch, recursive cleanup). Revisit
  once read-write or genuine user demand arrives.
- **C. Symlink to a sibling mount.** Elegant if the user already has the
  submodule mounted, but couples mounts to each other. Revisit alongside
  any future mount-discovery mechanism.

### 9.3 Synthesize a fake `.git/` inside each mount? — **deferred (mechanism committed)**

Many tools (`rg`, `cargo`, editors, `git log`) walk upward looking for a
`.git/` directory. Without one they fall back to weird defaults or
refuse to run.

Full options ladder (A0–A3, B, B+) and UX trade-offs:
[dotgit-synthesis.md](./design/dotgit-synthesis.md).

**What is decided:**

- **Mechanism (in-MVP):** Phase 1 implements `RootOverlay` (see step 4),
  the architectural hook that lets a projection inject synthetic
  top-level entries on top of the real tree. MVP overlay is empty.
- **Future ship-default (deferred):** **R1** — a small sentinel file
  (working name `.projgit/info.json`) plus an opt-in
  `--emit-dotgit=minimal` flag (the A0 variant). Documented but not
  implemented in MVP.

**What is deferred:**

- The *content* and *schema* of any synthetic entries.
- Whether to ship the sentinel on day one.

**Promotion criteria** (any one moves R1 from "future" to "in-MVP"):

1. A Phase 5 integration test is materially harder to write without a
   sentinel or `.git/`-marker.
2. A beta user reports a real workflow blocker traceable to the missing
   `.git/`.
3. We can't agree on the `info.json` schema after one more design pass
   — in which case shipping the simplest version forces clarity.

## 10. Decisions baked in

- **Read-only MVP.** No write path. Reduces scope ~50%.
- **gitoxide is the Fetcher backend.** Branch A confirmed by Phase 0a
  ([../spikes/ondemand-fetch/RESULTS.md](../spikes/ondemand-fetch/RESULTS.md)).
  `GitCliFetcher` deferred as a future fallback.
- **One object store, many mounts** is a hard architectural invariant;
  the store API never knows which projection is asking.
- **No daemon in MVP.** Per-process mounts using on-disk store + file
  locks. A daemon can be added later without API breakage.
- **ProjFS deferred** despite being the philosophically perfect API on
  Windows; WinFsp gives us more code-sharing on day one.
- **Windows symlinks default to `Auto` mode** (native reparse points via
  WinFsp, text-file fallback if unavailable). Out-of-tree targets emit
  file-symlinks with a logged warning. `Text`-mode marker is an NTFS
  Alternate Data Stream. See
  [windows-symlinks.md](./design/windows-symlinks.md).
- **Submodules render as empty directories** in MVP. Users who want
  submodule contents mount the submodule's commit explicitly.
- **`.git/` synthesis is deferred** but the `RootOverlay` mechanism
  ships in Phase 1. The future ship-default is **R1** (sentinel
  `.projgit/info.json` + opt-in `--emit-dotgit=minimal` flag), gated on
  Phase 5 evidence. See
  [dotgit-synthesis.md](./design/dotgit-synthesis.md).
