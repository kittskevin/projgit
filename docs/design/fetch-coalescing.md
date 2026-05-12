# Design: Fetch Coalescing

> Status 5/11/2026 Not implemented
>
> Companion to [`docs/design/workload.md`](workload.md) (the workload
> shape this work serves), [`docs/design/fetchers.md`](fetchers.md),
> and [`docs/design/prefetch.md`](prefetch.md).

## 1. The Problem (Measured)

The mount benchmark in
[`crates/projgit-cli/examples/bench_mount.rs`](../../crates/projgit-cli/examples/bench_mount.rs)
shows projgit beating `git ls-tree` on enumeration and `git cat-file`
on warm reads, but losing on cold reads:

| Operation              | projgit cold | projgit warm | git baseline |
| ---------------------- | -----------: | -----------: | -----------: |
| `readdir` of root      |        0.93 |        0.97 |        6.78 |
| recursive walk         |        8.04 |        1.57 |        5.67 |
| `cat` 3 files          |     8,754.7 |        0.48 |     2,904.3 |

(Median of 3, ms, against `rust-lang/log` on AMD Ryzen 7800X3D, WSL2.)

Cold first-read of three uncached files is ~3× slower than `git
cat-file` cold. That gap exists because:

- The `Fetcher` trait body path is single-OID
  ([`crate::Fetcher::fetch_object`](../../crates/projgit-core/src/fetcher.rs)).
  Concurrent kernel reads serialize through it.
- `GitCliFetcher` writes one OID at a time to its long-lived
  `git cat-file --batch-check` child. In a partial clone, each
  missing-object line triggers an independent promisor fetch with
  the upstream. Three OIDs = three serial protocol exchanges.
- The header path is already batched
  (`Fetcher::prefetch_headers` → `git cat-file --batch-check`
  fed many OIDs in one go), so the precedent for batching exists.
  Bodies just never got the same treatment.

**The fix is not "shovel OIDs into the existing batch-check child
faster."** That child is for header probing; even when body bytes
arrive as a side effect of the promisor fetch, the network exchange
is per-OID. The fix is **a body-batch path that causes a single
protocol exchange to ask for many OIDs**:

- `git fetch --filter=blob:none origin <oid1> <oid2> … <oidN>`, or
- the GVFS `POST /gvfs/objects` endpoint (currently deferred).

Both are inherently per-backend, which is what makes this a
trait-level question and not just an implementation tweak.

## 2. Workload Recap

The motivating workloads in priority order.

### 2a. Agent-eval (`os.walk` then targeted `cat`)

The README's design target. A short-lived agent points at a fresh
mount and:

1. Walks the tree to learn the project shape (`readdir`-heavy).
2. Reads a small set of orientation files (`README.md`,
   `Cargo.toml` / `package.json`, `src/lib.rs`, …).
3. Issues focused searches (e.g. ripgrep) over the tree.
4. Opens a small handful of files surfaced by step 3, reads
   them, stops.

Most files in the repo are never touched. Total bytes read are a
small fraction of total bytes in the commit.

### 2b. AI coding agents (Claude Code, Copilot, similar)

Same general shape, with two amplifiers:

- The agent issues many tool calls in parallel. A "scan three
  modules" prompt translates into a burst of concurrent `read_file`
  calls.
- `grep_search` is implemented on ripgrep, which walks the tree
  and reads many files in parallel by default
  (~thread-per-CPU). Single biggest concurrent-read producer.

### 2c. Editor + language server "open project"

Open a workspace in VS Code; rust-analyzer (or pyright, gopls, …)
opens *every* candidate file under `src/` in a burst to build its
index. Very high concurrency. Bench-relevant in the same way as
ripgrep.

### 2d. Single-threaded sequential tools (`cat a b c`, `tar`)

Cheap to ignore at the design level — they hit the cold path one
fault at a time, so reactive coalescing doesn't help. Anticipatory
prefetch is what helps these workloads.

## 3. Why Bodies Are Different From Headers

[`docs/design/prefetch.md`](prefetch.md) §4 already implements
header batching at `readdir` time. Why don't bodies get the same
T1 treatment automatically?

Two real differences:

- **Cost asymmetry.** Headers are tiny (`<sha> <kind> <size>` per
  line). Bodies are 1–10000× bigger. Hydrating bodies eagerly
  burns network on bytes nobody reads.
- **Cardinality asymmetry.** A directory listing has bounded
  width; even big directories have hundreds of entries, not
  millions. Header batches are naturally small. Body batches need
  *both* count and byte budgets to stay safe.

So bodies are a budgeted sibling of headers, not a drop-in
extension.

## 4. The Coalescing-Window Sub-Question

A reactive coalescing window collects on-demand misses arriving
within a small time window (say 1–5 ms) and ships them as one
batch. Worth being precise about who it helps and who it doesn't.

| Workload                                | Helped by reactive window? | Why |
| --------------------------------------- | :------------------------: | --- |
| `cat a b c` (single process, sequential reads) | No | Each `open` blocks on the previous read; only one fault is in flight at a time. |
| `cat a & cat b & cat c` (parallel)      | Yes | Three concurrent faults. Window catches all three. |
| ripgrep / parallel grep                 | Yes | Many concurrent reads from worker threads. |
| Language server initial index           | Yes | Burst of concurrent `open`+`read`. |
| `make -j` / `cargo build`               | Yes | Compiler processes spawn in parallel. |
| Agent issuing parallel tool calls       | Yes | Each tool call → independent thread → independent faults. |
| Sequential `git checkout`-style walk    | No | One fault at a time; same as `cat`. |

Two corollaries:

1. Reactive coalescing helps **multi-process / multi-threaded**
   workloads. It is invisible-but-useless for single-threaded
   sequential tools.
2. Anticipatory prefetch helps **single-threaded** sequential
   tools because it warms files before they're requested.

We probably want both. They cover disjoint workloads.

## 5. Design Axes

These are the questions worth being explicit about. Each has more
than one defensible answer.

### 5.1. When do we batch?

- **Reactive.** Collect on-demand misses in a small time window;
  fire one batch.
- **Anticipatory.** When `readdir` runs we already know which
  blob OIDs the kernel is about to ask for. Hydrate them under a
  budget, ahead of time.
- **Hybrid.** Both. They cover disjoint workloads (see §4).

### 5.2. What's the trigger boundary for anticipatory?

- **Per `readdir`.** Each `readdir` posts a body-prefetch task
  for that directory's blob OIDs. Simple, bounded, fits the
  existing T1 worker shape.
- **Per access pattern.** Detect e.g. "open then walk" and warm
  a wider scope. More magic, harder to reason about.
- **Manifest-driven.** `--prefetch src/auth` from the CLI; user
  tells us what to warm. Already laid out as T3 in
  [`docs/design/prefetch.md`](prefetch.md).

Per-`readdir` is the cheapest first move. Manifest-driven is a
clean follow-up.

### 5.3. Where does coalescing live?

- **Inside each `Fetcher` impl.** Every backend handles its own
  batching. Pros: backend-specific protocol knowledge stays local
  (`git fetch` vs `POST /gvfs/objects` are *not* the same
  operation). Cons: every backend has to implement it.
- **A generic `CoalescingFetcher<F>` wrapper.** Mirrors today's
  `Coalescer`. Pros: one implementation. Cons: a wrapper can
  only schedule single-OID calls into batches; it can't issue a
  protocol-batched request. It helps only if the underlying impl
  already exposes a batch entry point.
- **In the prefetch worker.** Extend the existing T1 worker to
  also accept body-prefetch tasks; it already batches and
  rate-limits.

The right answer is **all three at different layers**:

- Trait grows a body-batch entry point (so backends *can*
  implement it natively).
- The prefetch worker uses it for anticipatory tasks.
- The on-demand path uses a thin wrapper to do reactive
  coalescing across concurrent on-demand misses.

### 5.4. Coalescing window length (latency vs. throughput)

If we do reactive coalescing, the window length is the central
knob.

- **0 ms** (no wait): first miss ships immediately; concurrent
  misses queue but don't help that first call.
- **1–5 ms**: catches concurrent reads from a single command
  burst. Effectively invisible latency to a human; meaningful
  batching for ripgrep / LSP / parallel agents.
- **50+ ms**: better batching for slow access patterns; very
  visible latency for single-`cat` workflows.
- **Adaptive**: start at 0, grow if recent batches "would have
  caught more if I'd waited."

Adaptive is tempting but adds state and is hard to test
deterministically. Start with a flat 1–5 ms; let the bench tell
us. Make it configurable so it can be tuned per deployment.

### 5.5. Anticipatory body prefetch budget

This is the axis where bad choices hurt the system.

Eagerly hydrating bodies at `readdir` time means we may fetch
bytes nobody reads. For a 1 MB blob that nobody touches, that's
wasted network. For a 100 MB blob in a multimedia repo, that's
*a lot* of wasted network.

Defensible policies:

- **Sizes-then-bodies.** Use the existing `prefetch_headers` to
  learn each entry's size first. Only hydrate bodies under a
  per-entry size cap (e.g. 64 KiB, mirroring our existing
  blob-cache per-entry cap) and a per-batch byte cap (e.g.
  4–8 MiB). Honest and bounded.
- **Extension allowlist.** Hydrate bodies for likely-text-source
  extensions (`.rs`, `.toml`, `.md`, `.py`, `.json`, `.yaml`,
  `.txt`, lockfiles, …). Skip binaries.
- **Both, in series.** Headers first, then size+ext-filtered
  bodies.

The policy for projgit is: **bytes-not-read is a cheap mistake
(extra idle network); reads-blocking-on-network is an expensive
mistake (interactive latency).** For agent-eval we accept the
former to minimise the latter.

### 5.6. Failure semantics

`HeaderProbe` already taught us this lesson: a batch of OIDs has
per-OID outcomes. The body equivalent should mirror it:
`FetchProbe::{Hydrated, AlreadyPresent, Missing, Error}` per OID.
One bad OID can't poison the batch.

### 5.7. Tests

Mirror the existing two-tier shape:

- Unit tests against a fake counting batch fetcher (deterministic,
  no network).
- Network-gated test that mounts a real repo and asserts a single
  protocol exchange happened, not N. Without this, the design is
  unfalsifiable. Should live alongside
  [`crates/projgit-fuse/tests/mount_real_remote.rs`](../../crates/projgit-fuse/tests/mount_real_remote.rs).

## 6. Three Candidate Designs

Not mutually exclusive. Numbered for reference, not priority.

### Design A — Reactive coalescing only

- Add `Fetcher::fetch_objects(&[ObjectId])` with a default loop.
- A small reactive coalescer wakes on a 1–5 ms window or N pending
  OIDs, whichever first.
- `GitCliFetcher` overrides `fetch_objects` with
  `git fetch --filter=blob:none origin <oids…>`.
- All FUSE faults flow through the coalescer.

Pros: directly attacks the cold-cat case in the bench. No policy
choices about prefetch. Smallest change.
Cons: doesn't help workflows where misses don't overlap in time
(single-`cat`). Adds latency to single `cat` proportional to the
window length.

### Design B — Anticipatory body prefetch only

- Add `Fetcher::fetch_objects(...)` with the same `git fetch`
  override.
- Extend `PrefetchTask` with `Bodies(Vec<ObjectId>)`.
- After `readdir`, the provider posts a body-prefetch task,
  filtered by size + extension.
- On-demand `fetch_object` stays single-OID.

Pros: huge win on the agent-eval workload (the bench's recursive
walk + `cat` pattern). Bench's cold-cat row improves dramatically
because by the time the kernel reads, bytes are local.
No latency added to single `cat`.
Cons: doesn't help a workload that bypasses `readdir` (e.g. opens
files by absolute path). Wastes network on unread bodies unless the
budget is right.

### Design C — Both, gated

- Trait change as above.
- Reactive coalescing with a default-off "window" (0 ms = legacy
  behaviour). Opt-in via config.
- Anticipatory body prefetch with a default-on size + extension
  budget.
- New `--stats` counters: `body_batches_sent`, `bodies_hydrated`,
  `body_bytes_hydrated`.

Pros: best total behaviour. Each piece is independently
disable-able for triage.
Cons: more code, more configuration surface, more test coverage.

## 7. Recommended Direction

Combining the workload analysis (§2), GVFS as a first-class
backend, and the budget answer:

**Design C, with anticipatory prefetch as the leading change.**

Build order:

1. **Trait extension.**
   `fn fetch_objects(&[ObjectId]) -> Vec<FetchProbe>` with a
   default loop. The foundation for everything else, and the
   hook GVFS needs.
2. **Anticipatory body prefetch in the existing prefetch worker.**
   New `PrefetchTask::Bodies(Vec<ObjectId>)` posted by `readdir`
   after it posts headers, with a size + per-batch budget.
   Directly improves the bench numbers and the agent-walk
   workload.
3. **Native batch overrides.**
   - `GitCliFetcher::fetch_objects` → `git fetch --filter=blob:none
     origin <oids…>`. Pays off step 1 for the default backend.
   - `GvfsFetcher::fetch_objects` → finish the deferred
     `POST /gvfs/objects` endpoint. Turns GVFS into a real
     measured backend, not just a documented option.
4. **Reactive coalescing as a thin wrapper.** Helps ripgrep / LSP
   / parallel agent calls. Easy to ship after step 1 because the
   trait already supports it.

Each step is independently shippable and independently
bench-able.

## 8. GVFS-Specific Notes

GVFS being a first-class second backend changes how we evaluate
this work:

- The `Fetcher` trait is the public boundary for "how projgit
  talks to a remote." Today the body path pretends `git fetch`
  vs `POST /gvfs/objects` is an implementation detail. It isn't.
  The trait change recognises that.
- Closing the deferred `POST /gvfs/objects` endpoint moves GVFS
  from "experimental opt-in we built but never fully validated"
  to "real backend with a measurable shape." Worth doing as part
  of this work, not as a separate later effort.
- The bench should grow a `--fetcher gvfs --gvfs-url` mode so
  someone with a real GVFS server can produce honest comparison
  numbers. Today the bench implicitly times `GitCliFetcher`.

## 9. Risks And Open Questions

- **Promisor protocol mismatches.** `git fetch --filter=blob:none
  origin <oid>` is what the documentation says works for
  partial-clone promisor remotes; we should de-risk this with a
  small spike before committing to the trait override. (See §10.)
- **Server-side rate limits.** A single batched request asking for
  hundreds of OIDs can be rejected by some hosts. The trait should
  return per-OID outcomes so the worker can split-and-retry on
  partial success.
- **Pack proliferation.** Each batch produces a small pack on
  disk. Long-running mounts may accumulate many tiny packs. Same
  open issue as today's per-blob fetches; needs a background
  repack policy eventually. Out of scope for this design.
- **Body-prefetch and the FUSE `read` race.** A `read` arriving
  before the body batch finishes must not deadlock on the batch.
  The design must be: on-demand `read` is allowed to bypass an
  in-flight batch and issue its own (the coalescer dedups within
  itself); the batch may then "discover" the OID is already
  present and skip it.
- **Adaptive window.** Worth experimenting with later, but not
  in the first cut.

## 10. De-risking Spike Before Coding

Before any trait change, run this experiment in the devcontainer
against an already-partial-cloned `rust-lang/log`:

```sh
# pick three blob OIDs known to be missing locally:
oids=$(git ls-tree -r master | head -n 3 | awk '{print $3}')

# baseline: serial promisor fetches via batch-check
time { for o in $oids; do echo "$o" | git cat-file --batch-check; done; }

# proposed: one git fetch with many wants
time git fetch --filter=blob:none origin $oids
```

Outcomes:

- If the multi-OID `git fetch` is dramatically faster, the trait
  change as designed is the right call.
- If the difference is negligible, then either git is already
  batching internally for `cat-file`, or the win lives elsewhere
  (e.g. parallel TLS sessions). Worth knowing before writing
  trait code.

The same spike against a GVFS server (`POST /gvfs/objects` with a
list vs N×`GET /gvfs/objects/{oid}`) confirms the GVFS arm.

## 11. What This Doc Doesn't Decide

- The exact `FetchProbe` shape (mirrors `HeaderProbe`).
- Window length default (will be picked from bench data).
- Per-entry / per-batch byte budgets (will be picked from bench
  data).
- Background-repack policy.
- Async story (still sync for the foreseeable future, per
  [`crates/projgit-core/src/fetcher.rs`](../../crates/projgit-core/src/fetcher.rs#L7)).

These get nailed down in the implementation PRs once §10 has
produced numbers.
