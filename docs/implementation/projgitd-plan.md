# projgitd Implementation Plan

> Status: **living doc**. Tracks how the
> [`projgitd` design](../design/projgitd.md) actually gets built,
> stage by stage. Updated as each stage lands or surfaces something
> that changes downstream stages.
>
> Last updated: 2026-05-20 (Stages 0 and 1 done; Stages 2–5 not
> started).
>
> Architecture lives in [`docs/design/projgitd.md`](../design/projgitd.md);
> this doc is one level down: concrete steps, file layout, commit
> boundaries, and decision points per stage. If something here
> contradicts the design doc, the design doc wins and this doc
> updates.

## 0. Why this doc exists

The design doc says *what* projgitd is and *why*. This doc says
*what to do next* and *what to learn from each step before
committing to the next*.

It exists separately because:

- The design is the architectural commitment; it should change rarely.
- The implementation plan is a working document that updates as we
  learn. Stage 0's findings reshape Stage 1's detail; Stage 1
  validates assumptions for Stage 2; etc.
- Splitting them lets the design doc stay stable and short while the
  implementation doc absorbs the operational reality.

The plan is **deliberately detailed for Stage 0 and rough for later
stages.** Concrete planning more than one stage ahead would be
speculation — Stage 0's outcome may reshape what Stage 4 looks
like. We plan one stage in depth, sketch the next, and bullet the
rest.

## Stage 0 — Spike: prove FUSE fd passing works — **DONE 2026-05-20, GREEN**

[`spikes/fuse-fd-passing/`](../../spikes/fuse-fd-passing/README.md)
(not a workspace member). Decisive outcome: `fuser::Session::from_fd`
is the production-ready primitive; a process that did NOT open
`/dev/fuse` and did NOT call `mount(2)` can fully serve the FUSE
protocol on the resulting fd received via `SCM_RIGHTS`. Stage 4 (T4
last-mile via Harbor + fd-passing) is now green-lit; Stages 1–3
proceed as planned without modification.

The original plan for this stage (sub-steps, code layout, decision
points) is preserved below for reference — future stages can use it
as a template for risk-elimination spikes.

### 0.1 Goal

Settle the load-bearing question for Stage 4 (T4 last mile, per-
namespace mount via fd passing) before committing to it. Three
sub-questions:

1. Does `fuser` expose an API to run the FUSE protocol loop against
   an existing `/dev/fuse` fd (i.e. one the library didn't open)?
2. If not, what's the smallest acceptable fallback — fuser's
   low-level types, libfuse via FFI, or a hand-rolled protocol
   shim?
3. Are there kernel-version or capability surprises that bite the
   plan? (E.g. needing `CAP_SYS_ADMIN` in places we didn't expect,
   or `SCM_RIGHTS` interactions with mount-namespace boundaries.)

The answer is *one paragraph* of design-doc commentary in
`docs/design/projgitd.md` §8 Stage 0 status, plus a `README.md` in
the spike directory documenting what worked. Throwaway code; no
production crate.

### 0.2 Pre-flight: read fuser's source (~30 minutes)

The fuser crate is local at
`/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/fuser-0.17.0/src/`
(we read parts of it earlier for the `SessionACL` work). Targets:

- `session.rs` — `Session`, `SessionACL`, how the fd is opened
  and held.
- `mnt.rs` (if present) — the mount syscall side.
- `channel.rs` (if present) — the fd-holding abstraction.
- `lib.rs` — public re-exports; tells us what callers can construct.

Specific things to look for:

- A constructor on `Session` that takes a raw fd.
- A `pub` `Channel` or equivalent that we could populate ourselves.
- Any documented "external fd" / "borrowed fd" / "split mount and
  serve" pattern.

**Output of pre-flight:** add a short paragraph to the spike's
`README.md` saying "fuser supports X" or "fuser does NOT support
this; fallback is Y."

### 0.3 The spike code

Throwaway crate at `spikes/fuse-fd-passing/`. **Not** a workspace
member (`spikes/` is already excluded from the workspace per the
[handoff Gotchas](../implementation/handoff.md)). Suggested layout:

```
spikes/fuse-fd-passing/
├── Cargo.toml
├── README.md                  # what this proves, what we learned
└── src/
    └── main.rs                # argv-dispatched subcommands
```

Dependencies (minimal):

- `nix` — for `socketpair`, `sendmsg`/`recvmsg` with `SCM_RIGHTS`,
  `mount` syscall.
- `libc` — raw FUSE protocol structs and the `mount(2)` interface
  if `nix` doesn't expose what we need.

Optionally:

- `fuser` — only if pre-flight (§0.2) says we can attach to an
  existing fd cleanly.

#### 0.3.1 Two subcommands in one binary

```
spike open <mountpoint> <socket_path>
   # opens /dev/fuse
   # calls mount(2) to attach FUSE at <mountpoint>
   # connects to <socket_path>
   # sends fd via SCM_RIGHTS
   # exits (proves the mount survives without the original opener)
   # OR: sleeps for N seconds while child serves
```

```
spike serve <socket_path>
   # listens on <socket_path>
   # accepts one connection
   # receives fd via SCM_RIGHTS
   # runs minimal FUSE protocol loop
   # logs every op for visibility
```

This split lets us test the actually-relevant property:
**a process that did not open /dev/fuse can still serve the FUSE
protocol on it.** That's what makes Stage 4 possible.

#### 0.3.2 Minimal FUSE protocol shim

Goal: respond to enough ops that `ls /tmp/mp` and `cat /tmp/mp/hello`
return sensible results.

Required ops (in order they'll be requested):

- `FUSE_INIT` — negotiate protocol version, return our supported
  flags. Mandatory; without it the kernel kills the mount.
- `FUSE_GETATTR` (for inode 1) — return mode `040755`, owner,
  empty.
- `FUSE_OPENDIR` (inode 1) — return a directory handle.
- `FUSE_READDIR` (inode 1) — return one entry: `("hello", inode 2)`.
- `FUSE_RELEASEDIR` — accept and acknowledge.
- `FUSE_LOOKUP` (parent 1, name "hello") — return inode 2, size 5.
- `FUSE_GETATTR` (inode 2) — file, mode `0100644`, size 5.
- `FUSE_OPEN` (inode 2) — return a file handle.
- `FUSE_READ` (inode 2, offset 0, size N) — return "world".
- `FUSE_RELEASE` — accept.

Everything else: respond `ENOSYS` and let the kernel back off.

The FUSE wire format is documented in the kernel headers
(`include/uapi/linux/fuse.h`) and in fuser's own source. For the
spike we can hand-roll the structs we need (they're small) or steal
fuser's `fuse_abi` module if it's public.

**If pre-flight says fuser exposes an "attach to existing fd" API:**
use that instead of hand-rolling. Spike becomes a 50-line
"connect fuser's Session to an external fd" demo.

#### 0.3.3 Verification

```sh
# Terminal 1: serve
cargo run -- serve /tmp/spike.sock

# Terminal 2: open + send fd
mkdir -p /tmp/spike-mp
cargo run -- open /tmp/spike-mp /tmp/spike.sock

# Terminal 3: verify
ls -la /tmp/spike-mp       # → "hello" with mode 0644 and size 5
cat /tmp/spike-mp/hello    # → "world"
```

Success = both commands return correctly. Failure = kernel kills
the mount with an error in `dmesg`, or the protocol loop deadlocks.

### 0.4 Decision point

The spike's `README.md` ends with one of three outcomes:

- **Green:** fuser supports external-fd, or the hand-rolled shim
  is small enough that Stage 4 can use the same approach. Proceed
  to Stage 1 with confidence in the full plan.
- **Yellow:** the mechanism works but the production path needs
  more care than expected (e.g. libfuse FFI). Stage 1 proceeds;
  Stage 4 gets a "needs additional planning" note.
- **Red:** the mechanism does not work in our environment.
  Reframe the design: drop T4 as a future possibility, ship
  T1.5-only via Stages 1–3. The architecture mostly survives
  because T1.5 doesn't need fd passing.

Whatever the outcome, **update the design doc's Stage 0 row** with
the finding before moving on.

### 0.5 Commit boundary

One commit at the end of Stage 0:

```
spike(fuse-fd-passing): prove (or refute) external-fd FUSE serving
```

Body includes: what was tested, the outcome (green/yellow/red),
specific fuser APIs / libfuse / hand-roll path chosen if
applicable. The spike code itself doesn't go to production — it
exists so future-us can re-run the demo if our environment changes.

### 0.6 Time estimate

One focused session if fuser already exposes what we need; two if
we need a hand-rolled shim.

## Stage 1 — Multi-projection in one process — **DONE 2026-05-20**

### 1.1 Goal

One projgit process can host multiple `ProjectionFsProvider`s
sharing one `ObjectStore`, one `Fetcher`, and one set of in-memory
caches. Ships value even without the daemon — it's the substrate
the daemon plugs into in Stage 2.

### 1.2 Inventory the current single-projection assumptions

Before designing, grep the workspace for places that bake
"one projection per process" or "one mount per CLI invocation."
Specific places likely affected:

- `crates/projgit-cli/src/main.rs` — `cmd_mount` builds exactly one
  provider.
- `crates/projgit-fuse/src/adapter.rs` — `ProjgitFuse<F>` wraps one
  `Arc<F>`.
- `crates/projgit-core/src/projection_fs.rs` — `ProjectionFsProvider`
  has a fixed `projection_id`; need to confirm the inode-allocator
  comments on multi-projection.
- `crates/projgit-core/src/fs_provider.rs` — `InodeAllocator` reserves
  a synthetic-inode bit; does it accommodate multiple providers
  sharing one allocator, or one per?

Output: a short notes file in the PR description listing what
needs to change vs what already supports multi-projection.

### 1.3 Design decision (settled 2026-05-20: many mounts, shared store)

The inventory (§1.2) surfaced two relevant facts:

- `projection_id`, `InodeAllocator`, and `ProjectionFsProvider`
  already accommodate multiple projections in one process. The only
  thing single-projection about the codebase is `/* projection_id */
  1` hardcoded in callers.
- **`ROOT_INODE = 1` is per-projection.** Each `InodeAllocator`
  treats inode 1 as its own root. Two `ProjectionFsProvider`s
  behind one `FsProvider` dispatcher both claim inode 1; the
  dispatcher would need either an inode-layout change (carve
  high bits for projection ID) or a per-op HashMap reverse map.

Given that, two paths to "multi-projection in one process":

| | Path A: one mount + dispatcher | Path B: many mounts, shared store |
|---|---|---|
| Cache sharing | ✅ | ✅ |
| Fetch coalescing | ✅ | ✅ |
| Code change | medium-large (inode-layout work or HashMap routing) | small (CLI + plumbing) |
| Per-projection failure isolation | dispatcher could affect all | each mount independent |
| Stage 3 fit (sidecar holds fd) | dispatcher splits oddly | natural — each sidecar one fd |
| Stage 4 fit (T4 per-namespace) | each sidecar a dispatcher slice | natural — each sidecar one mount |
| Inode-collision bug risk | real | none |
| UX for agents | identical (bind subdir) | identical (bind sub-mount) |

**Picked Path B.** Same observable behaviour at a fraction of the
implementation cost, and naturally matches Stages 3–4. The "one big
mount" version's only advantage was "ls /var/projgit/ shows all
projections" — which is also true of many mounts (kernel shows
mountpoint directories like normal dirs).

Consequences of Path B:

- **Inode layout: unchanged.** Each `ProjectionFsProvider` keeps
  its own `InodeAllocator` with `ROOT_INODE = 1`.
- **FUSE adapter dispatch: unchanged.** Each provider gets its own
  `ProjgitFuse<F>` and its own `/dev/fuse` fd.
- **Mountpoint shape: many mountpoints, one per projection.**
  `projgit mount-multi --mount main=/mp-a --mount v1=/mp-b SOURCE`
  produces two distinct FUSE mounts.
- **`.git/` synthesis per projection: unchanged.** Each provider's
  overlay is built per-projection, no cross-projection coordination
  needed.
- **What's shared:** `Arc<ObjectStore>` (so the tree / header /
  blob LRUs are shared), `Arc<HydratingObjectStore<F>>` (so the
  in-flight fetch coalescer is shared), `Arc<F>` (one batch-check
  child per host instead of per projection).

### 1.4 Implementation outline (Path B)

In rough order:

1. **CLI: `projgit mount-multi`** subcommand. Same flags as
   `mount` (source, cache_dir, remote, offline, stats, no_dotgit,
   allow_other, fetcher, gvfs_url) plus repeated
   `--mount REF=PATH` for the projection list. `--commit` and
   `--subtree` deferred (`mount` retains them for single-projection
   use).
2. **`cmd_mount_multi` in `projgit-cli`.** Resolves source once,
   opens store once, builds one shared `HydratingObjectStore`,
   then loops over `--mount` entries creating one
   `ProjectionFsProvider` (sequential `projection_id` 1..N) and
   one `mount_background` per entry. Holds all
   `BackgroundSession` handles until Ctrl-C, then drops them all
   (unmounting cleanly in fuser's drop impl).
3. **No changes to `projgit-core` or `projgit-fuse`.** The
   substrate is already correct.
4. **Integration test** in `projgit-fuse/tests/`: mount two
   projections of the same local repo in one process sharing an
   `Arc<HydratingObjectStore>`. Verify (a) isolation — files read
   from mount A match projection A's content, (b) shared cache —
   reading a blob via mount B after reading it via mount A produces
   a `blob_cache` hit (proves cache state is shared).

### 1.5 Decision point — **CLEAR 2026-05-20**

Outcome: Stage 1 landed cleanly via Path B (many mounts, shared
store). Both predictions from the plan held:

- **Multi-projection works in our type system.** No changes to
  `projgit-core` or `projgit-fuse` were needed. Stage 1 was pure
  CLI plumbing and a Ctrl-C handler. The substrate was already
  correct (`projection_id` already plumbed through
  `InodeAllocator`, `ProjgitFuse<F: FsProvider>` already generic,
  `Arc<ObjectStore>` already shared-friendly).
- **Cache locality works.** The integration test verifies the
  shared `blob_cache` records a hit when mount B reads the same
  OID that mount A read first. Manual CLI test with `--stats`
  shows tree/header/blob caches all aggregate correctly across
  mounts; one tree-cache miss seeds the cache for both mounts'
  subsequent reads. **§1.6-in-memory amortisation works in
  process; Stage 2 inherits it for free.**

Implications for Stage 2:

- No need to re-design the cache architecture. The daemon just
  hosts the same `Arc<ObjectStore>` + `Arc<HydratingObjectStore>`
  with N providers behind it (or with the daemon directly hosting
  the mount, depending on T1.5 vs sidecar deployment).
- Per-provider prefetch workers (one per projection) are
  duplicative on cold paths — each worker walks the tree of its
  projection independently. Not a correctness issue, but worth
  noting for Stage 2: a per-host prefetch worker would be more
  efficient. Defer until measurement says it matters.

### 1.6 Commit boundary — actual

Per Path B, no `projgit-core` changes were needed; only two
commits actually landed:

- `feat(cli): mount-multi subcommand + Stage 1 integration test`
  (the code change + the integration test, which doesn't need the
  network because it uses a local fixture repo).
- `docs(projgitd): Stage 1 done; update plan, handoff, audit`
  (this doc, handoff Done bullet, audit memory snapshot).

### 1.7 Time estimate

Two to three focused sessions. Largest single piece in the plan.

## Stage 2 — Daemon scaffold + DaemonFetcher

### 2.1 Goal (high level)

Single-tenant T1.5 deployment end-to-end. `projgitd` daemon hosts
the multi-projection mount; `projgit attach` is a client that asks
the daemon to mount a projection at a given mountpoint.
`DaemonFetcher` is a new `Fetcher` impl that talks over the unix
socket.

### 2.2 Open questions to settle here (not now)

- IPC encoding: JSON vs MessagePack vs custom. Start with JSON,
  revisit if profiling warrants.
- Daemon process lifecycle: foreground? PID file? systemd
  socket-activation? Pick one for the prototype.
- Authentication: unix socket file-mode permissions for V1;
  per-projection authorisation comes later.

### 2.3 Sketch

- New crate `projgit-daemon` with the server.
- New module `projgit-core::fetcher::daemon` with `DaemonFetcher`.
- Protocol module documenting message shapes.
- Smoke test: daemon + two clients with different projections of
  the same repo; verify shared cache state.

### 2.4 Decision point

After Stage 2:
- We can measure §1.6-in-memory amortisation. **Phase C bench from
  the audit becomes runnable here.**
- We have a daemon process that can be supervised, restarted, and
  monitored. Stage 5 polish (systemd unit) becomes the path to
  production.
- We know whether the protocol design is awkward or natural.

### 2.5 Time estimate

Two to three focused sessions.

## Stages 3–5 (outline only)

Detail lands after Stage 2.

### Stage 3 — Sidecar holds the FUSE fd

Move the FUSE mount from the daemon to per-container sidecars. The
daemon becomes pure data plane; sidecars run the protocol loop
locally. Failure-mode upgrade — daemon crash becomes recoverable
instead of mount-killing.

Stage 3 only makes sense after Stage 2 ships a working daemon and
we have the supervision story in mind.

### Stage 4 — T4 last mile (per-namespace fd passing)

Add the second `MountSource` impl: sidecar accepts a FUSE fd from
Harbor (via SCM_RIGHTS) and runs the protocol loop against it.
Per-namespace agent isolation.

Detail entirely dependent on Stage 0's outcome.

### Stage 5 — Lifecycle / supervision

systemd unit (or kubelet recipe), restart policy, persistent
daemon state for fast recovery, health checks, `tracing-subscriber`
wiring. Production polish.

## Cross-cutting notes

### Test workflow

Per
[`docs/design/projgitd.md`](../design/projgitd.md) §8 each stage
ships its own tests:

- Stage 0: manual verification (the spike is throwaway).
- Stage 1: unit tests + integration test against a local repo.
- Stage 2: smoke test with daemon + two clients (network-gated
  follow-up exercising real URL mounts).
- Stage 3+: TBD.

Default `cargo test --workspace --all-targets` stays green at every
commit. Network-gated tests live behind `PROJGIT_NETWORK_TESTS=1`
as today.

### Documentation cadence

Every stage updates:

1. [`docs/implementation/handoff.md`](handoff.md) Done section —
   what landed, what gotchas were learned.
2. [`docs/design/projgitd.md`](../design/projgitd.md) Stage row in
   §8 — actual outcome vs planned outcome.
3. This doc — promote the next stage from "outline only" to
   "detailed plan" once the predecessor's data is in.
4. [`/memories/repo/audit.md`](/memories/repo/audit.md) if A1 / A3
   move from "design landed" toward "closed."

### Editing discipline

Repository memory at [`/memories/coding-gotchas.md`](/memories/coding-gotchas.md)
notes that the IDE replace tool can silently drop edits. **Verify
every edit via shell** (`grep`, `git status`, `git diff`) before
running tests or committing. Python `<<'PY'` scripts with
`assert old in src` are the reliable fallback when the IDE tool
misbehaves.

### Stop conditions

Each stage has a "stop conditions" line in
[`docs/design/projgitd.md`](../design/projgitd.md) §8 — if a stage
surfaces something that invalidates a design assumption, pause and
update the design before pressing on. The risk-ordered staging
exists precisely so the early stops don't cost much.

## What this doc is not

- A spec for any specific stage. Each stage's PR carries its own
  module-level docs and tests.
- A schedule. No dates, no commitments to a release.
- A binding plan. As Stages 0–2 surface findings, this doc gets
  rewritten. The architecture (the design doc) is what stays
  stable; the plan adapts to what the architecture meets in
  practice.
- A user-facing roadmap. That's the handoff's "What I'd do next"
  list, derived from this plan.
