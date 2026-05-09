# projgit — Problem statement

> Status: **drafted 2026-05-09 to capture the use case driving Phase 6+
> work.** Companion to [initial-plan.md](initial-plan.md) (the original
> design) and [handoff.md](handoff.md) (the running status doc).
>
> This document exists because the project's *original* framing —
> "lazy-fetch git projections as cross-platform read-only mounts" — is
> a valid framing but doesn't sharpen which trade-offs matter. We now
> have a concrete use case that does. This doc captures what it is,
> why it justifies the build, and which existing tools come close
> without quite fitting.

## 1. The use case driving design

**Agent-evaluation containers running against a monorepo.**

Concretely:

- A team runs LLM-driven coding agents against a shared codebase.
- Each evaluation spawns one or more containers; each container is
  scoped to a specific commit (could be `HEAD`, a topic branch, or a
  historical revision).
- The agent inside the container needs to **explore the codebase as if
  it were a normal local checkout** — `os.walk()`, `find`, `grep -r`,
  language-server indexing, git tooling like `git log` / `git diff` /
  `git blame`. The agent can't predict which paths matter; access
  patterns are sparse and unpredictable.
- Evals run frequently enough that **per-container clone time and disk
  use are first-order costs**. A monorepo of even moderate size
  (10K–1M files) costs measurable wall-clock and gigabytes-per-eval if
  done with stock `git clone`.

The hot-loop pattern is roughly:

```
spawn container ─→ mount commit ─→ agent runs (sparse access)
                                  ↳ git tooling, diffs, blame
                                  ↳ build / test the touched areas
       ↓
  ~seconds later, repeat with a different commit
```

**The thing that has to be true for this to work**: the agent perceives
a complete, normal-looking checkout. Files it has never touched must
appear with real metadata in directory listings. `cat` on any file
must return the right bytes. `git log src/auth/foo.py` must return
real history. None of this can require the agent to know in advance
which files matter.

## 2. What "appears complete" actually requires

In increasing depth — every level must hold:

1. **`ls -la` shows every file** that exists in the commit, with real
   `size` / `mode` / `mtime`. Hidden subtrees would change behaviour
   for `os.walk` / `find` / build tools.
2. **`open()` + `read()` returns the actual blob bytes** the first
   time and forever after.
3. **The filesystem behaves like a normal filesystem under arbitrary
   tooling** — language servers, search tools, IDEs, build systems,
   git porcelain.

Layers (1) and (2) imply some form of *virtual filesystem* —
genuinely-on-disk files would mean a full checkout, which is what
we're avoiding.

Layer (3) implies enough of `.git/` is present that `git`-aware tools
behave normally. (We address this in
[design/dotgit-synthesis.md](design/dotgit-synthesis.md).)

## 3. Why this combination is hard

Three properties have to hold *simultaneously*, and that's what
narrows the option space:

- **Lazy fetch.** Disk and bandwidth proportional to what's *touched*,
  not to repo size. Otherwise per-eval cost scales with monorepo size.
- **Total enumerability.** All files must be visible to `os.walk()`
  even when their bytes have never been fetched. Otherwise agents
  can't explore the codebase.
- **Cheap multiplexing.** Many containers, each at potentially
  different commits, sharing one upstream connection and one local
  object store. Otherwise per-host disk and outbound bandwidth scale
  with concurrency.

Each of these alone is solved. The combination is what's missing from
the off-the-shelf toolchain.

## 4. Alternatives considered

For each, the honest reason it doesn't fit. None of these are bad
tools — most are excellent at their actual job.

### 4.1 Stock `git clone` per container

Per-eval: clone the repo, check out the commit, run the agent.

**Fits "appears complete" perfectly.** It's just a real working tree.

**Fails on lazy-fetch and multiplexing.** Each clone is full repo
size on disk and over the wire. For a 1M-file monorepo, untenable
per-eval; for a 10K-file repo with hundreds of containers, the disk
amplification is ugly.

`--reference` to a shared bare repo helps with disk but not with the
checkout time (still O(files) `write()` calls per container).

### 4.2 Stock `git clone --filter=blob:none`

Lazy-fetches blobs via the partial-clone "promisor remote" mechanism.
This is what projgit uses today as its upstream contract.

**Fits lazy-fetch.** Bytes arrive on demand.

**Half-fits "appears complete."** Files exist as zero-byte placeholders
until promisor-fetched. `os.walk` sees them; `open()` triggers a
fetch. Close to projgit's behaviour.

**Fails on multiplexing.** Each container has its own `.git/index`
and its own copy of the partial-clone state. Cross-container dedup
only happens at the underlying-pack level if you `--reference` an
external `.git/objects/`, and even then per-container disk overhead
is the size of the index (non-trivial on monorepos).

Importantly, the promisor mechanism does the right thing here, which
is why projgit can lean on it as the upstream API.

### 4.3 Sparse checkout (`git sparse-checkout`)

Common suggestion. **Architecturally cannot meet requirement (1)
above.**

Sparse checkout works by setting `SKIP_WORKTREE` on every file
outside the patterns, which causes `git checkout` to **delete those
files from the working tree**. Files outside the sparse patterns
genuinely don't exist on disk. `os.listdir` doesn't return them;
`os.stat` returns `ENOENT`.

For agents whose access patterns can't be predicted, this fails
immediately: the agent looks for a file, the agent doesn't find it,
the agent infers the file doesn't exist. There is no git config
that fixes this. The capability "files appear available without
being on disk" requires a virtual filesystem; stock git is not one.

### 4.4 GVFS / VFS for Git / Scalar (Microsoft)

Closest off-the-shelf fit. GVFS pioneered the architecture; Microsoft
later upstreamed the primitives (partial-clone, sparse-index,
commit-graph, multi-pack-index, background maintenance) into stock
git, then renamed the "Microsoft-recommended setup" as **Scalar**.

**Scalar today** = stock git with all the right opt-ins enabled +
some helper commands. **No virtual filesystem in mainline.** ProjFS
support exists in Microsoft's fork but is Windows-only and not
upstream.

So Scalar fits 80% of the use case for repos that fit in stock
sparse-checkout's "agent knows what paths it cares about" model,
but doesn't help when the agent needs to wander.

### 4.5 EdenFS + Mononoke (Meta)

Closest *architectural* fit, hands down. EdenFS is a virtual
filesystem (FUSE / NFS / ProjFS) that lazy-materialises a working
tree backed by Mononoke (Meta's purpose-built source-control
server).

**Fits all three properties.** Files appear available, bytes are
lazy, multiple checkouts share one daemon and one backing store.

**Two real problems for our use case:**

1. **Operationally heavy.** EdenFS is co-designed with Mononoke.
   Mononoke is a stateless-app-servers + distributed-blobstore +
   derived-data-databases system, designed for Meta-scale operations.
   Standing it up for a single team's eval pipeline is a different
   project.
2. **Hg-first, monorepo-first design.** EdenFS speaks Mercurial /
   Sapling natively; git support exists but is a second-class shim.
   The data model is hg-shaped (Bonsai changesets internally).

If you have Meta's scale problem, EdenFS is the right answer. If you
have ours, it's an order of magnitude too much infrastructure.

### 4.6 Custom server-side projects (Gitaly, JGit DfsRepository, …)

Source-control RPC layers that scale serving but **don't change the
client experience**. They don't make the working tree virtual; the
client is still stock `git`.

Useful infrastructure for forge operators; doesn't fit our problem.

### 4.7 Just use a fast filesystem and live with full clones

A reasonable position. Many shops do exactly this. Per-container
clone of even a million-file repo, on a fast SSD, with parallel
fetch, can be tens of seconds. If your evals are minutes long, this
is in the noise.

It stops working when:

- The repo is *large* enough that even fast clone is minutes.
- You spawn enough containers that per-host disk becomes a real
  budget.
- Per-eval startup latency dominates total throughput.

Any of those three can push you toward virtualisation. The boring
answer wins until it doesn't.

## 5. The trade-offs we're explicitly choosing

projgit's positioning, sharpened against the alternatives:

| Trade-off | Choice | Why |
|---|---|---|
| Server | **Stock git remote** (anything that supports `--filter=blob:none`) | Works against GitHub / GitLab / SSH / your existing infra without deploying anything new. Loses the surgical-RPC wins of Mononoke. |
| Read/write | **Read-only MVP** | Halves scope. Write path is real engineering; we'll address it via overlayfs (a separate doc) when needed. |
| Storage format | **Stock git odb** (gix-compatible) | Tooling can read our store directly. No custom format to debug. Loses the per-blob-file random-access wins of a custom store. |
| Daemon | **Optional, not required** | One process per mount works today; a `projgitd` daemon is a Phase 6 add-on (designed but not built) that wins when you have many containers per host. |
| FS frontend | **FUSE on Linux/macOS, WinFsp on Windows** | The two backends share a `FsProvider` trait; ProjFS deferred for code-sharing reasons. |
| Synthesised `.git/` | **Mechanism shipped in Phase 1, content deferred** | The `RootOverlay` machinery exists; what we put in `.git/` is the next big design question for this use case (see [design/dotgit-synthesis.md](design/dotgit-synthesis.md)). |
| Prefetch | **Manifest-driven + readdir-batched first; learned patterns later** | Designed in [design/prefetch.md](design/prefetch.md). |

## 6. Why nothing else has these exact trade-offs

A short summary of the empty cell in the matrix:

- **Stock git** doesn't virtualise (sparse-checkout deletes what
  it skips).
- **GVFS / Scalar** retreated from virtualisation in favour of
  upstreaming primitives; the mainline result is "fast partial
  clone," not "virtual working tree."
- **EdenFS** virtualises beautifully but requires Mononoke (a
  source-control server you'd have to deploy) and is hg-shaped
  internally.
- **Forge-internal infra** (Gitaly, GitHub Spokes, etc.) scales the
  *server*, not the client filesystem.
- **`overlayfs` / `unionfs`** alone don't talk to git; they're the
  right primitive for the *write* layer on top of a read-only
  virtual mount, but not the read-side answer.

projgit's bet: there's a useful middle ground — *virtual filesystem
on the client, stock git on the server* — that nobody else is
filling. The Microsoft path proved the client side worked but
abandoned it for upstreaming; the Meta path requires owning the
server. We're building the client without requiring server changes.

If we're wrong about the gap, the failure mode is "Scalar was good
enough." If we're right, the win is "anyone can run agents against
their existing GitHub-hosted monorepo without re-architecting their
SCM."

## 7. Success criteria

For the agent-eval use case specifically, this is what "shipped"
means. None are arbitrary — each maps to a property an agent will
notice or not notice.

- A container can mount a commit in **<100 ms** end-to-end (process
  start → directory walkable).
- `os.walk('/workspace')` returns **every file in the commit** with
  real size/mode/mtime, with first-walk latency bounded by directory
  count × one batched RTT (not file count × RTT).
- `cat <path>` returns correct bytes within **one upstream RTT** on
  cold; under **1 ms** on warm.
- `git log <path>` works inside the mount and returns real history
  within **one upstream RTT** for a typical query.
- Per-container disk overhead **<10 MiB** at steady state.
- A single host can run **≥100 concurrent mounts** against the same
  commit without each adding a separate upstream connection.

The first four are about agent perception. The last two are about
operational viability.

## 8. What this document is not

- A timeline. Phase ordering is in [handoff.md](handoff.md).
- A spec for `projgitd`. The daemon design lives in its own doc when
  we build it.
- A spec for `.git/` synthesis. That's
  [design/dotgit-synthesis.md](design/dotgit-synthesis.md), already
  drafted at the mechanism level.
- A spec for prefetch. That's [design/prefetch.md](design/prefetch.md).
- A justification for adding a write path. The trade-off note above
  is intentionally light; if/when writes become real, it's a separate
  design conversation that revisits the locked decision in
  [initial-plan.md §10](initial-plan.md).

If a future reader is wondering whether some new feature is in scope:
ask whether it makes the agent-eval use case in §1 work better. If
yes, probably in scope. If it's about replacing git or competing with
GitHub's server side, almost certainly out.
