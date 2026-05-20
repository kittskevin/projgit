# Design: `projgitd` — Daemon + Sidecar Topology

> Status: **under design 2026-05-20**. No code yet; this doc is the
> architecture commitment we want to align on before any code lands.
> Extends [`container-deployment.md`](container-deployment.md) with the
> next-step topology that closes audit items A1 (no daemon) and A3
> (cross-process single-flight gap). Tenancy (T1.5 vs T4 last-mile) is
> **deliberately deferred** here — the architecture supports both;
> the choice only changes how the sidecar exposes its mount to the
> agent.
>
> Read alongside [`workload.md`](workload.md) §1.6 (the headline
> amortisation claim this design moves from "the on-disk CAS amortises"
> to "the in-process caches also amortise"), [`fetchers.md`](fetchers.md)
> (the `Fetcher` trait this design reuses as the daemon-sidecar wire),
> and [`container-deployment.md`](container-deployment.md) (the three
> topologies T1/T2/T3 this design extends with T4).

## 0. Why this document exists

The shipped topologies (T1, T2, T3 from
[`container-deployment.md`](container-deployment.md)) all run one
projgit process per mount. That process owns its own `ObjectStore`,
its own `Fetcher`, its own header/tree/blob LRUs, and its own
long-lived `git cat-file --batch-check` child. N containers reading
N different projections of the same repository pay N× the in-memory
cache cost and (worse) cold-fetch the same OID N times if the timing
is unlucky.

That's audit A3 in concrete form. The workload doc §1.6 amortisation
claim — "the first mount pays the network cost; every subsequent
mount sees a warm hit" — is only true *sequentially* (different
containers in time, sharing the on-disk CAS), not *concurrently*
(different containers at the same time, racing to cold-fetch the
same OID).

This document specifies the architecture that closes that gap. The
short version: a per-host `projgitd` daemon owns the upstream
connection, the fetcher, and the shared cache state; per-container
sidecars own the FUSE mount fd and run the protocol loop locally;
the agent is a pure read-only consumer.

We commit to this architecture, but we deliberately do *not* commit
yet to the multi-tenancy posture (T1.5 vs T4) — see §5.

## 1. The architecture

```
host:
  ┌─────────────────────────────────────────────────────────┐
  │  projgitd  (one per host)                               │
  │    one ObjectStore + one GitCliFetcher                  │
  │    in-memory tree / header / blob LRUs (the big ones)   │
  │    in-flight fetch coalescer                            │
  │    upstream HTTPS / Git remote, one connection          │
  │    unix-socket listener: /run/projgitd.sock             │
  └────────────────────────┬────────────────────────────────┘
                           │  unix socket
                           │  control-plane RPC
                           │  (fetch this OID, list refs, …)
                           │
       ┌───────────────────┼───────────────────┐
       │                   │                   │
  ┌────▼─────┐       ┌─────▼────┐         ┌────▼─────┐
  │ sidecar 1│       │ sidecar 2│         │ sidecar N│
  │  (Harbor)│       │  (Harbor)│         │  (Harbor)│
  │  holds   │       │  holds   │         │  holds   │
  │  FUSE fd │       │  FUSE fd │         │  FUSE fd │
  │  runs    │       │  runs    │         │  runs    │
  │  protocol│       │  protocol│         │  protocol│
  │  loop    │       │  loop    │         │  loop    │
  └────┬─────┘       └─────┬────┘         └────┬─────┘
       │                   │                   │
       │  FUSE mount       │  FUSE mount       │  FUSE mount
       │  (commit X)       │  (commit Y)       │  (commit Z)
       │                   │                   │
  ┌────▼─────┐       ┌─────▼────┐         ┌────▼─────┐
  │ agent 1  │       │ agent 2  │         │ agent N  │
  │  /repo   │       │  /repo   │         │  /repo   │
  │  read-   │       │  read-   │         │  read-   │
  │  only    │       │  only    │         │  only    │
  └──────────┘       └──────────┘         └──────────┘
```

All three actors live on one host. The daemon may itself be a
container (the prototype will likely run it as a sidecar pod);
sidecars are per-agent-container processes managed by Harbor; agents
are the existing eval workloads, unmodified.

The on-disk CAS at e.g. `/var/lib/projgitd/cache/<url-hash>/` is
shared via volume mount between the daemon and every sidecar. This
is load-bearing — see §4.

## 2. What each actor owns

### 2.1 `projgitd` (daemon)

- One `ObjectStore` (opens the shared on-disk CAS).
- One `Fetcher` (today `GitCliFetcher`; future others).
- The big in-memory LRUs (tree, header, blob).
- The in-flight fetch coalescer — **this is the single-flight that
  closes A3.** N sidecars asking for the same OID concurrently see
  one upstream fetch, not N.
- The persistent `git cat-file --batch-check` child (one per host
  instead of one per container).
- A unix-socket listener at a well-known host path.
- Per-projection session state — mainly resolved ref tip OIDs.
- *Not* the FUSE fd. *Not* the FUSE protocol loop.

### 2.2 Sidecar (per-container, Harbor-managed)

- The FUSE mount and the `/dev/fuse` fd.
- The FUSE protocol loop (`ProjgitFuse` adapter as today).
- A `ProjectionFsProvider` instance for this consumer's projection.
- A *small* hot-path cache for inodes / attrs (avoid daemon
  round-trip on every `getattr` of the same inode).
- A `Fetcher` impl that is "**ask the daemon over the unix
  socket**." Reuses the existing `Fetcher` trait — see §4.
- An `ObjectStore` that opens the **same on-disk CAS** the daemon
  writes to.
- The mount-establishment privilege (`CAP_SYS_ADMIN` /
  `/dev/fuse` access). Agent does *not* have these.

### 2.3 Agent (existing eval workload)

- Sees `/repo` (or whatever path) with files in it. Read-only.
- No FUSE caps. No socket to the daemon. No knowledge of any of
  this.

## 3. Why the sidecar holds the FUSE fd

The single most important architectural choice in this design is
**which process owns the FUSE fd.** The two options:

- **Daemon-owned fd.** Simpler. Daemon does the mount syscall and
  runs the protocol loop for every consumer. One process owns
  everything.
- **Sidecar-owned fd** (this design). Daemon is a data backend
  only. Each sidecar opens `/dev/fuse`, does the mount, runs the
  protocol loop, talks to the daemon for things it can't serve
  from disk alone.

Sidecar-owned wins on failure-mode handling. Concretely:

**With daemon-owned fd:**

```
daemon crashes
   → every consumer's FUSE fd is owned by a dead process
   → every read / stat / open across all N consumer mounts → EIO
   → kubelet restarts the daemon container
   → BUT: consumer mount fds are still tied to the OLD process
   → restart does NOT recover the mounts
   → every consumer container also needs to be restarted
```

Blast radius on daemon crash: every container on the host,
simultaneously, with no automatic recovery.

**With sidecar-owned fd:**

```
daemon crashes
   → sidecar's daemon-socket calls return errors
   → sidecar returns EAGAIN / EIO for *in-flight* requests that
     needed the daemon (cold-path fetches)
   → mount stays alive; reads of already-resident OIDs keep working
     because the sidecar reads them from the shared on-disk CAS
     directly (no daemon round-trip needed)
   → kubelet restarts daemon container; sidecar reconnects
   → previously-failed requests retry; new requests succeed
```

Blast radius on daemon crash: a brief window of cold-fault
unavailability per container. Already-warm data keeps serving. No
mass restart needed.

This is the difference between a *prototype* and a *production
deployment*. It costs one extra process per container, which on an
agent-eval host running ~100 containers is ~100 extra processes —
real, but cheap.

The kernel's FUSE driver is happy as long as *something* is reading
from the `/dev/fuse` fd. The sidecar holds that contract. The
daemon being unavailable is just a slow `Fetcher`, not a dead
filesystem.

## 4. Mapping to existing types

The most important property of this design: it **fits the existing
type system exactly.** The daemon-sidecar split lives on top of
abstractions we already have.

### 4.1 The `Fetcher` trait is the daemon-sidecar wire

`Fetcher::fetch_object(oid)` already abstracts "make sure this OID
is locally present." Today's impls:

- `GitCliFetcher` — shells out to `git fetch`.
- `GixFetcher` — native-Rust.
- `GvfsFetcher` — Azure DevOps backend.
- `NoopFetcher` — offline mode.

This design adds:

- `DaemonFetcher` — sends `{op: "fetch", oid: ...}` over the unix
  socket to `projgitd`; daemon does the actual fetch (using *its*
  fetcher); returns success once the OID is in the shared on-disk
  CAS. Sidecar's `ObjectStore::read_object(oid)` then succeeds
  directly via the disk path.

Everything above the `Fetcher` trait — `HydratingObjectStore`,
`ProjectionFsProvider`, `ProjgitFuse` — is unchanged. The sidecar
is structurally identical to today's `projgit mount` process; only
the fetcher is different.

This is what makes the design genuinely incremental rather than a
ground-up rewrite. The hard part (FUSE protocol handling, attr
caching, dotgit synthesis) is already done.

### 4.2 The shared on-disk CAS is the data plane

Critically, **bytes do not flow over the unix socket.** The socket
carries only coordination messages:

- "fetch this OID" (sidecar → daemon)
- "OID is now resident" (daemon → sidecar)
- "list refs for this URL" (sidecar → daemon, used at mount setup)
- health / liveness pings

Actual blob bytes flow through the shared on-disk CAS:

- Daemon's fetcher writes packs into `/shared/cache/.git/objects/`.
- Sidecar's `ObjectStore` opens the same directory.
- Sidecar reads pack bytes via gix's mmap path — zero-copy from
  kernel page cache.

This is why your "memory-mapped IO" intuition was directionally
correct but pointed at the wrong layer: shared memory between the
daemon and sidecar processes is exactly what we get for free from
the page cache mediating the shared pack files. We don't need an
explicit shmem channel.

### 4.3 Cache locality

In-memory LRU caches in this design:

- **Daemon's LRUs are big** (header, tree, blob). They serve the
  cross-container amortisation goal: an OID resolved by sidecar 1
  is cheap for sidecar 2.
- **Sidecar's LRUs are small** (mainly inode → attr to avoid
  daemon round-trips on `getattr` storms). Sidecar can't afford to
  duplicate the daemon's caches per container; that defeats the
  point.

In practice the sidecar's `ProjectionFsProvider` only needs to
cache inode-level metadata, since pack data is served from the OS
page cache anyway. Detail to settle at Stage 2.

## 5. Last-mile delivery: deferred (T1.5 vs T4)

The sidecar holds a FUSE fd. **How that fd becomes a mount visible
to the agent** is the only piece of this design that differs
between T1.5 and T4. Importantly, that decision is contained to
**one module in one stage** of the build plan. Everything else is
shared.

**T1.5 last mile** (single-tenant, current eval workload):

```
sidecar does the FUSE mount on a host path (e.g. /run/projgit/<id>)
Harbor does docker run -v /run/projgit/<id>:/repo:ro,rslave
agent sees /repo
```

Standard Docker `-v`. No new caps on Harbor or agent. Security
boundary is host filesystem permissions; anyone with shell on the
host can reach any sidecar's mount.

**T4 last mile** (multi-tenant, agent isolation matters):

```
Harbor (privileged) opens /dev/fuse, calls mount(... fuse ... fd=N)
  in the agent container's mount namespace (via setns or by being
  the parent of the agent process)
Harbor passes the fd to the sidecar via SCM_RIGHTS
sidecar runs the FUSE protocol loop against fd
agent sees /repo in its namespace only
```

Stronger isolation; the mount doesn't exist outside the agent's
namespace. Costs: Harbor needs `CAP_SYS_ADMIN` and `/dev/fuse`;
fd-passing protocol; possibly setns gymnastics.

**Both deliveries use the same sidecar and daemon code.** The
difference is purely "how does the sidecar receive its fd": either
it does `mount()` itself on a host path, or it accepts a fd from
Harbor. That's one trait-shaped abstraction (`MountSource`) with
two impls.

This is what "defer the tenancy decision" means in practice:
ship T1.5 first (Stage 3 below); when multi-tenant lands as a real
requirement, add the T4 `MountSource` impl (Stage 4). The rest of
the daemon stays unchanged.

## 6. How this closes audit items

This design closes or substantially closes several open items from
[/memories/repo/audit.md](/memories/repo/audit.md):

- **A1 (no daemon).** This *is* the daemon. The problem-statement
  §7 success criterion #6 — "≥100 concurrent mounts, one upstream
  connection" — becomes architecturally achievable for the first
  time.
- **A3 (cross-process single-flight gap).** The daemon's in-flight
  fetch coalescer is exactly what was missing. N sidecars asking
  for the same OID concurrently see one upstream fetch.
- **§1.6 amortisation across in-memory caches.** The bench
  validates §1.6 for the on-disk CAS today. This design extends
  amortisation to the in-memory LRUs as well.

It does *not* close:

- **A4 (read-only invariant vs agent writes).** Still read-only.
  Writes need an overlay sandwich downstream.
- **B3 (CI bench).** Separate work.
- **Phase 3d (Windows).** Linux-only.

## 7. Staged build plan

Risk-ordered. Each stage either eliminates a load-bearing assumption
or ships incremental value (or both). Stop-anywhere safe: if a
stage's outcome surprises us, we redesign before committing further.

### Stage 0 — Spike: prove FUSE fd passing is possible

**Throwaway code** in `spikes/fuse-fd-passing/`. Smallest possible
program:

1. Process A opens `/dev/fuse`, calls `mount(... "fuse" ... fd=N)`
   to attach it to a test mountpoint.
2. Process A passes the fd to process B via `SCM_RIGHTS` over a
   unix socket pair.
3. Process B runs a hand-rolled minimal FUSE protocol loop:
   `INIT`, `LOOKUP("hello")`, `READ` returning "world".
4. Verify `cat /tmp/test/hello` from process C returns "world".

**Why first.** Three load-bearing questions only this can answer:

- Does `fuser` expose a "session from existing fd" API? (Or do we
  drop to libfuse / hand-roll the protocol layer?)
- Are there kernel-version or capability surprises in this
  devcontainer?
- What's the wire-format overhead?

Stage 4 cannot be designed honestly without Stage 0's answer. If
fuser doesn't expose this, Stage 4's complexity goes up
significantly.

**Stage 0 also has a fallback value:** even if we never reach Stage
4 (pure T1.5 deployment), the spike is the de facto documentation
for how FUSE-in-container actually works at the kernel layer.

**Stop conditions:** if the spike reveals something fundamentally
incompatible (e.g. fuser's session model can't be split, libfuse
FFI is prohibitively complex), pause and redesign T4 path before
Stages 1–3.

### Stage 1 — Multi-projection within one process

Refactor so one `ProjgitFuse` adapter can host multiple
`ProjectionFsProvider`s sharing one `ObjectStore` and one
`Fetcher`. Today's structure is one-projection-per-process and one
mount-per-process; this generalises both.

Surface: a new `MultiProjectionProvider` (or similar) that
multiplexes inode space across child providers via the synthetic
inode bit pattern we already use. CLI gets a `projgit mount-multi`
subcommand or just accepts `--ref a,b,c --mountpoint x,y,z` pairs.

**Why second.** Largest internal refactor in the plan, but works
without daemon or fd-passing. Ships shared-cache value immediately
even as a single-process tool. Forms the substrate the daemon
plugs into in Stage 2.

**Stop conditions:** if inode allocation across multiple
projections collides or `ObjectStore` thread-safety surprises
surface, pause and rethink rather than papering over.

### Stage 2 — Daemon scaffold with control-plane RPC

A new `projgitd` binary plus a `DaemonFetcher` impl on the sidecar
side. Wire format: unix socket carrying length-prefixed messages
(JSON for the prototype; binary later if profiling warrants).

Initially the daemon hosts T1.5-style subdirectory mounts itself
(the daemon does the FUSE mount on a host directory; agents
consume via `-v`). No fd passing yet. **Ships single-tenant T4 to
production-quality.**

Surface:

- `projgitd` binary in a new crate `projgit-daemon`.
- `DaemonFetcher` impl in `projgit-core` (or its own crate).
- A `projgit-client` library or extended `projgit` CLI subcommand
  (`projgit attach <daemon-socket> <projection> <mountpoint>`).
- A protocol module documenting message formats and error codes.

**Why third.** Now we have:
- Shared cache across containers (the §1.6-in-memory win).
- Single in-flight fetch coalescer (A3 closed).
- A daemon that can be supervised, restarted, monitored.
- Single-tenant deployment story end-to-end.

If the project stops here, that's still a major step up from
today.

**Stop conditions:** if the protocol design proves awkward (e.g.
serialising errors across the wire surfaces ambiguity), pause and
iterate before committing client code.

### Stage 3 — Sidecar holds the FUSE fd (failure-mode upgrade)

Move the FUSE mount from the daemon to per-container sidecars. The
daemon becomes pure data plane; sidecars run the protocol loop
locally; if the daemon crashes, sidecars degrade rather than die.

Surface:

- The sidecar process is essentially a `projgit attach …` from
  Stage 2 that *also* opens `/dev/fuse` and does its own mount,
  rather than asking the daemon to mount on its behalf.
- A small per-sidecar inode-level hot cache to avoid daemon
  round-trips on hot `getattr`.
- Documentation of the daemon-failure failure mode (the EAGAIN /
  retry contract).

**Why fourth.** This is the failure-mode upgrade. Stage 2's
daemon-holds-fd model is fragile in production. Stage 3 makes the
daemon restartable without taking down the world.

**Stop conditions:** if sidecar-side caches need to grow large
enough to defeat the §1.6 amortisation goal (i.e. we're
duplicating per-container what the daemon is supposed to own),
pause and rethink the cache split.

### Stage 4 — T4 last mile (per-namespace mount via fd passing)

Add the second `MountSource` impl: sidecar accepts a FUSE fd from
Harbor (via SCM_RIGHTS) and runs the protocol loop against it,
instead of opening `/dev/fuse` itself. Harbor does the mount in
the agent's namespace; agent gets a mount visible only inside its
own container.

Surface:

- A `MountSource` trait with `Owned` (Stage 3's `mount()` itself)
  and `External` (fd from socket) impls.
- Harbor-side helper library / sample for "open fd, mount in
  child-namespace, pass to sidecar."
- Test that exercises mount-namespace isolation explicitly.

**Why fifth.** Brings multi-tenant readiness. Builds on Stages 0
(fd passing proven) and 3 (sidecar architecture). Only this stage
needs the answer to Stage 0's main question.

**Stop conditions:** if multi-tenant requirements never
materialise, Stage 4 stays unbuilt. Single-tenant deployments use
Stage 3.

### Stage 5 — Lifecycle / supervision / production polish

Once the architecture is proven, ship the production wrapper:
systemd unit for the daemon, kubelet recipe for the sidecar
shape, persistent daemon state for fast restart, health-check
endpoints, structured logging via `tracing-subscriber`. Out of
scope for the design; in scope for the deliverable.

## 8. Open questions to resolve in later stages

These are deliberately not answered here. Each will be settled by
the data Stages 0–3 produce, not by speculation now.

- **Protocol shape.** JSON over unix socket vs MessagePack vs a
  custom binary format. Start with JSON; revisit if profiling
  shows it's the bottleneck (almost certainly won't be — bytes
  are on disk).
- **Sidecar cache size.** How much per-inode metadata to cache
  per sidecar before round-trip cost wins out. Stage 2/3.
- **Mount namespace coordination.** For Stage 4, exactly how
  Harbor obtains the agent's mount namespace fd. Probably
  `/proc/<agent_pid>/ns/mnt` with the daemon container sharing
  PID namespace with the agent.
- **Crash recovery state.** What does the daemon persist so it
  can restart fast? Probably just "list of (projection-url, ref,
  cache-dir)" so it can re-resolve OIDs without re-fetching. The
  packs are on disk already.
- **Multi-instance daemons.** Sharded by URL? One per host? One
  per tenant? Out of scope until a real deployment hits a single
  daemon's capacity.
- **Authentication / authorization on the daemon socket.** Today
  unix socket permissions are the boundary. For multi-tenant
  hosts, per-projection capabilities would be needed. Stage 4+.
- **Whether the sidecar talks to the daemon at all on the warm
  path.** If a hot OID is already on disk, the sidecar's
  `ObjectStore` reads it directly and the daemon never sees the
  request. This is good for warm-path latency, but means the
  daemon's "knowledge of who's reading what" is incomplete.
  Probably fine. Worth measuring.

## 9. What this document is not

- A protocol spec. Stage 2's PR will include the protocol
  document.
- A user-facing deployment guide. That's the downstream of Stage
  5; the architectural framing lives here.
- A multi-tenant security spec. Stage 4+, downstream of an actual
  multi-tenant requirement.
- A perf claim. The §1.6-in-memory amortisation claim becomes
  empirically testable only after Stage 2/3; the Phase C bench
  (audit-A3) will measure it directly.
- A commitment to ship every stage. Each stage is independently
  valuable; stopping at Stage 2 or 3 leaves a coherent
  deployment story.
