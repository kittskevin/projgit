# Design: The Workload Shape projgit Is Built For

> Status: **current as of 2026-05-12**. This is the project's
> opinionation document. Companion to
> [`docs/problem-statement.md`](../problem-statement.md) (what use
> case, why nothing else fits) and
> [`docs/design/prefetch.md`](prefetch.md) /
> [`docs/design/fetch-coalescing.md`](fetch-coalescing.md) (specific
> subsystems shaped by this).

## 0. Why this document exists

projgit's pitch in the README is "lazy-fetch git as a read-only
filesystem." That's accurate but underspecified — it sounds like
projgit could be a general-purpose virtual git client. **It can't,
and that is a feature.** Every projection cache, prefetch tier,
fetcher trait choice, and even the decision to be read-only flows
from one specific workload shape. This doc names that shape so:

- A reader can tell from first principles whether projgit will fit
  their problem.
- A contributor can evaluate a proposed change with "does this
  improve the workload shape?" instead of "is this a generally
  good idea?"
- A future maintainer can revisit the workload assumption itself
  if reality changes, rather than re-deriving it from the code.

## 1. The Workload Shape

projgit is built for one workload class. In one sentence:

> **Many short-lived processes pointed at a Git commit, performing
> wide-shallow access with bursty concurrency and predictable
> ordering, that tolerate speculative work in exchange for low
> interactive latency.**

Six concrete properties make up that shape.

### 1.1 Wide-shallow access

Most files in the commit are never touched. The set that *is*
touched is a small, unpredictable fraction of the tree. A reader
typically:

- walks much of the directory structure (cheap metadata-only),
- reads a handful of orientation files (`README.md`,
  `Cargo.toml`, top-level source),
- searches for a few patterns,
- reads the small subset surfaced by search.

Total bytes read are usually well under 1% of total bytes in the
commit. This is the property that justifies lazy fetch. A workload
that reads everything (e.g. full-tree static analysis) would not
benefit from projgit.

### 1.2 Bursty concurrency

Filesystem traffic comes in storms separated by idle gaps. Two
distinct storm types:

- **Metadata storms.** `readdir`, `stat`, and `lookup` calls
  arrive in bursts (directory walks, language-server index
  builds, `find -type f`). These are cheap on the wire — tree
  objects already ship with a partial clone — but expensive in
  syscall count if naïvely served.
- **Read storms.** Concurrent `read` calls arrive in bursts
  (ripgrep, parallel agent tool calls, LSP file indexing). These
  are the expensive ones because each cold miss can hit the
  network.

Idle gaps between storms are real and exploitable: prefetch
workers can finish background work without contending with the
foreground.

### 1.3 Predictable ordering

`readdir → lookup → read` is the canonical sequence. Each step
hints at the next. A `readdir` reveals exactly which OIDs the
kernel is about to `lookup`. A `lookup` reveals exactly which OID
might soon be `read`. This isn't a probabilistic prediction; it's
a structural property of how the FUSE/WinFsp protocol presents
work to us. We get to lean on it hard.

### 1.4 Tolerance for over-fetching

Bytes-not-read are a *cheap* mistake (extra idle network).
Reads-blocked-on-network are an *expensive* mistake (interactive
latency that humans or agents perceive directly). The workload
strongly prefers the former.

This is what makes anticipatory prefetch safe: when in doubt
between "fetch in case it's needed" and "wait until asked," the
former is almost always correct, given a sane budget.

### 1.5 Short-lived sessions

Mounts last minutes to hours, not days. A container that finishes
its eval gets torn down. This means:

- Process-only caches don't need long-tail memory budgets — the
  process exits.
- Persistent state lives in the shared on-disk Git object store,
  not in projgit-specific files.
- Optimisations that pay off only after hours of runtime
  (learned access patterns, adaptive cache sizing) are mostly
  not worth the complexity.

### 1.6 High parallelism with shared storage

A single host runs many concurrent mounts against the same (or
related) commits. The shared on-disk Git object store amortises
cold-fetch cost across all of them: the *first* mount that touches
a blob pays the network cost; every subsequent mount sees a warm
hit. This is the property that makes "100 containers per host"
operationally viable and makes cold-cost optimisation
disproportionately valuable — cold is per-repo-version, not
per-mount.

## 2. The Two Cost Shapes And What They Need

The two storm types from §1.2 cost differently and want different
things from projgit.

| Cost shape       | What's expensive   | What helps                                                                 |
| ---------------- | ------------------ | -------------------------------------------------------------------------- |
| Metadata storm   | Syscalls and round-trip count | Tree LRU, header LRU, T1 readdir-time header prefetch, blob-free `readdir`. |
| Read storm       | Network round trips | Body batching, anticipatory body prefetch (with budget), shared object store. |

Metadata is mostly handled today (the LRUs and T1 prefetch were
built specifically for this). The body story is incomplete — the
benchmark's cold-cat regression is exactly the read-storm cost
showing through. See
[`docs/design/fetch-coalescing.md`](fetch-coalescing.md).

## 3. How Every Subsystem Maps To The Shape

This section is the design discipline. Every existing or planned
subsystem traces back to a property in §1.

| Subsystem                                   | Workload property it serves                                | File                                   |
| ------------------------------------------- | ---------------------------------------------------------- | -------------------------------------- |
| Lazy fetch (partial-clone promisor)         | Wide-shallow access (§1.1)                                 | [`fetchers.md`](fetchers.md)           |
| One CAS, many mounts                        | High parallelism + shared storage (§1.6)                   | [`../problem-statement.md`](../problem-statement.md) §3 |
| Tree LRU                                    | Metadata storms repeat directory listings (§1.2)           | `crates/projgit-core/src/tree_cache.rs` |
| Header LRU                                  | Metadata storms repeat header lookups (§1.2)               | `crates/projgit-core/src/header_cache.rs` |
| Small-blob LRU                              | Repeated reads of small orientation files (§1.1, §1.2)     | `crates/projgit-core/src/blob_cache.rs` |
| T1 readdir-time header prefetch             | Predictable ordering (§1.3) + metadata storms (§1.2)       | [`prefetch.md`](prefetch.md)           |
| Long-lived `git cat-file --batch-check`     | Bursty metadata storms amortise the child cost (§1.2)      | `crates/projgit-core/src/fetcher/git_cli.rs` |
| Blob-free `readdir`                         | Wide-shallow access (§1.1) — never hydrate to compute size | `crates/projgit-core/src/projection_fs.rs` |
| Read-only MVP                               | Workload doesn't need writes; cuts scope ~50%              | [`../initial-plan.md`](../initial-plan.md) §10 |
| Single-commit projection                    | Workload binds to one commit per mount (§1.5)              | `crates/projgit-core/src/projection.rs` |
| Network-gated tests + bench                 | Honest measurement of the shape we claim to serve          | [`../bench/baseline.md`](../bench/baseline.md) |
| Fetch coalescing *(planned)*                      | Read storms (§1.2) + tolerance for over-fetching (§1.4)    | [`fetch-coalescing.md`](fetch-coalescing.md)     |
| Anticipatory body prefetch *(planned)*      | Predictable ordering (§1.3) + tolerance (§1.4)             | [`fetch-coalescing.md`](fetch-coalescing.md)     |
| GVFS as first-class second backend *(planned)* | Multi-backend bench credibility, server-side batching (§1.6) | [`fetchers.md`](fetchers.md), [`fetch-coalescing.md`](fetch-coalescing.md) |

If a planned subsystem doesn't trace cleanly to a property in §1,
it probably isn't projgit's job.

## 4. Two Properties Of The Architecture That Make Us Bold

These aren't workload properties; they're properties of how we
chose to build the system. They're what let us make
opinion-driven optimisations without fear.

### 4.1 Fail-soft

Every optimisation is *opt-in to actual reads.* The on-demand
`fetch_object` path is the source of truth for correctness;
prefetch and batching are accelerators on top. Concretely:

- We can drop a T1 prefetch task under channel pressure — the
  `lookup` will fetch correctly when it runs.
- We can fail an anticipatory body batch — the `read` will fault
  on demand.
- We can disable fetch coalescing entirely with a feature flag — every
  individual fault still works.

The worst case for any optimisation we ship is "we wasted some
bandwidth" or "we did a bit of extra work." Never "we returned
wrong bytes" or "we deadlocked." This safety floor is what makes
us comfortable speculating in §1.4.

### 4.2 No commit graph

projgit operates within a single commit OID. We never walk
history, never negotiate refs, never reason about shallow
boundaries, never compute commit parents. An entire class of git
costs (the most expensive class for many tools) is gone by
deliberate scope cut. This is what lets `ObjectStore` stay as
small as it is, and it's why our cold path is bounded by tree +
blob fetches and nothing else.

If the workload ever wanted history, projgit would be the wrong
tool. We are honest about that.

## 5. Workloads projgit Is *Not* For

Saying what we're not for is half of saying what we are for.

- **Long-lived developer workstations on a single repo.** Just
  clone. The amortisation argument doesn't apply when there's
  one process and time is cheap.
- **Heavy write workloads.** projgit is read-only. Add a write
  layer (overlayfs, custom) on top if you need one; that's a
  separate design.
- **Full-tree static analysis.** A workload that reads every
  byte gets no benefit from lazy fetch and pays the cold cost
  once for everything. A pre-warmed clone wins.
- **Binary or multimedia repos.** Big blobs blow our budgets;
  the small-blob LRU caps and anticipatory body budgets exist
  exactly because we assume "most blobs are small text/source."
- **Multi-repo orchestration.** projgit is one mount per
  projection. If your workload spans many repos with cross-repo
  semantics, you want something else (or you want projgit
  composed under something else).
- **Adversarial or hostile inputs.** projgit assumes a
  cooperative remote and a cooperative reader. Defensive hardening
  beyond what gix gives us is out of scope.
- **History exploration.** No commit graph. `git log`-style
  queries depend on `.git/` synthesis (deferred); projgit's
  identity is "the tree at one commit," not "all reachable
  history."

If any of those describe your workload, the failure mode is
"Scalar (or stock git, or sparse-checkout, or EdenFS) was a
better choice." That's a valid outcome and it's not our project
failing.

## 6. The Design Discipline That Falls Out

When proposing a change to projgit, walk it through these
checks in order:

1. **Which property in §1 does it serve?** If none, it's
   probably not projgit's job. (Ship it elsewhere.)
2. **Does it preserve fail-soft (§4.1)?** If a failure of the
   new feature can corrupt reads or block on a deadlock, redesign
   so the new feature is purely accelerative.
3. **Does it preserve the single-commit / no-history scope (§4.2)?**
   If it requires walking history or reasoning across commits,
   it's a different project (or it lives in `.git/` synthesis,
   which is a separate design conversation).
4. **Can it be measured?** The bench
   ([`docs/bench/baseline.md`](../bench/baseline.md)) is the
   honest answer to "did this help?" New optimisations should
   move bench numbers; new features should at least not regress
   them.
5. **Is the failure mode legible?** A reader hitting an edge
   case should be able to consult §5 above and recognise their
   workload as one we don't claim.

This is not a heavy process. Most decisions take seconds with this
list in mind. Its purpose is to keep projgit *small and opinionated*
rather than slowly drifting into "a generic virtual git client" —
which is a project nobody benefits from.

## 7. What This Document Is Not

- A spec for any one subsystem. Each subsystem has its own design
  doc (linked in §3).
- A timeline. Phase ordering lives in
  [`../handoff.md`](../handoff.md).
- A scaling guide. The §1.6 amortisation property is qualitative;
  exact concurrency limits depend on the deployment and aren't
  fixed by projgit.
- A guarantee that the workload assumptions are right forever.
  If they stop being right, this document is what you re-derive
  the architecture from.
