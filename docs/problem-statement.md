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
  `git blame`. The agent can't predict which paths matter(that is what is under test)) 
  access patterns are sparse, and unpredictable, especially for the agent failure modes.
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

…and crucially, this loop runs **in parallel, many times over**. A
single eval suite spawns dozens-to-thousands of containers
concurrently — different commits, different agent variants,
different prompts — all on the same host (or fleet). The
"per-container" costs above multiply by the concurrency factor.
That's what makes "many containers, one shared store, one upstream
connection" a first-class requirement and not a nice-to-have.

**The thing that has to be true for this to work**: the agent perceives
a complete, normal-looking checkout. Files it has never touched must
appear with real metadata in directory listings. `cat` on any file
must return the right bytes. `git log src/auth/foo.py` must return
real history. None of this can require the agent to know in advance
which files matter. We are evaluating if the agent knows what files matter.

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
This is the upstream protocol projgit speaks; the question here is
what the *user-facing* `git clone --filter=blob:none` workflow gives
you, which is less than people often assume.

**Half-fits lazy-fetch.** History blobs (other revisions, `git log -p`,
`git blame` deep enough to need old content) arrive on demand. But the
default post-clone `git checkout HEAD` materialises the entire worktree
at checkout time — it has to, because checkout writes real files. So
worktree disk and `write()` syscalls are still O(repo) per container,
the same as 4.1. The partial-clone savings are on *history*, not on
the working tree.

**Fits "appears complete."** It's a real worktree on disk, so `os.walk`
and friends see real files with real bytes. (This is the part that
*is* like 4.1.)

**Fails on multiplexing.** Each container has its own worktree, its
own `.git/index` (non-trivial on monorepos even with sparse-index),
and its own copy of the partial-clone promisor state. Cross-container
dedup of *objects* is possible via `--reference` to a shared bare repo,
but the per-container worktree and index are unavoidable.

**Combining with sparse-checkout** (the obvious next move) fixes the
worktree-disk problem but reintroduces 4.3's enumerability failure:
files outside the sparse patterns aren't on disk at all.

Importantly, the promisor *protocol* does the right thing — it's a
clean lazy-fetch RPC. That's why projgit speaks it as its upstream
contract. What projgit adds on top is the virtual filesystem that
stock `git checkout` doesn't provide: files visible to `os.walk`
without being materialised, with bytes fetched on `open()`.

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

The closest *prior art* to projgit, with an instructive history.

**The arc.** Microsoft built GVFS (later renamed VFS for Git) for the
Windows + Office monorepos circa 2017. It was a real virtual
filesystem: ProjFS on Windows, hydrating files from a custom GVFS
protocol against a matching server. It worked. Then Microsoft
retreated: they upstreamed the *enabling primitives* (partial-clone,
sparse-index, commit-graph, multi-pack-index, background maintenance)
into stock git, and packaged "the Microsoft-recommended opt-ins" as
**Scalar** — without the virtual filesystem.

**Scalar today** is stock git + helper commands + sane defaults for
big repos. There is no virtual filesystem in mainline git. The VFS
for Git fork still exists but is in maintenance mode and Windows-only
(ProjFS doesn't exist on Linux/macOS).

**Why this matters for us.** On the use case in §1:

- **On Linux** (where eval containers actually run), Scalar reduces
  to partial-clone + sparse-checkout with nicer defaults — i.e. §4.2
  combined with §4.3, with §4.3's enumerability failure intact.
  Scalar gives you nothing extra here.
- **On Windows**, VFS for Git would technically virtualise, but it
  needs the GVFS-protocol server and is single-OS.

The instructive part isn't "Scalar fails our use case" — it's *why*
Microsoft retreated. The virtualisation worked; it was the cost of
maintaining a git fork plus the Windows-only platform reach that
made partial-clone-only the better bet for them. projgit's bet is
that those costs are lower today — gix gives us a clean git library
to build on, and FUSE + WinFsp covers Linux/macOS/Windows — and
that the use case in §1 needs the virtualisation Microsoft walked
away from.

### 4.5 EdenFS + Sapling + Mononoke (Meta)

The closest *architectural* cousin. EdenFS is a virtual filesystem
(FUSE on Linux, NFS on macOS, ProjFS on Windows) that lazy-materialises
a working tree from an underlying object store. Sapling is Meta's
hg-compatible client (open-sourced 2022). Mononoke is the
source-control server. EdenFS is what we'd build if we owned both
ends of the wire — projgit owes it intellectual debt.

**Fits all three §3 properties** when run as the full stack: files
appear available, bytes are lazy, multiple checkouts on one host
share a daemon and a backing store.

**Why it doesn't fit our use case:**

1. **No "stock git server" path.** EdenFS's fast path is Mononoke
   (or local Sapling backed by a converted repo). You can technically
   point it at a git remote, but you lose most of the lazy-fetch
   wins: regular git has no equivalent of Mononoke's batched
   derived-data RPCs, so tree fetches go one-at-a-time over the
   wire. The §5 "works against your existing GitHub without
   deploying anything new" trade-off is the architectural
   disagreement, not a deployment detail.
2. **Operationally heavy.** EdenFS alone is a privileged daemon with
   its own config, on-disk state, and lifecycle. With Mononoke it's
   a full distributed system (stateless app servers + blobstore +
   derived-data databases) designed for Meta-scale operations.
3. **Sapling-shaped data model.** Sapling is hg-compatible at the
   client level; Mononoke's internal changeset model (Bonsai) is
   neither git nor hg. Git interoperability exists but is the
   conversion-shim path, not the native one.

If you have Meta's scale and are willing to run Meta's stack, EdenFS
is the right answer — projgit's "stock git remote" architecture
wouldn't survive at that operating point (one-at-a-time tree fetches,
no derived-data RPCs, no push-down filtering). We're betting that
many teams have the *use case* — wander-anywhere agents over a
monorepo — without the *scale* that justifies running Meta's stack,
and that for those teams "virtual filesystem on the client, stock git
on the server" is the missing point in the design space.

### 4.6 Custom server-side projects (Gitaly, JGit DfsRepository, …)

Worth mentioning so it's clear we considered them: source-control RPC
layers (Gitaly behind GitLab, JGit's DfsRepository behind Gerrit,
GitHub's Spokes) make the *server* faster and more scalable, but the
*client* is still stock `git`. They don't help with any of §3's
properties on the client side. They're orthogonal to projgit, not
alternatives to it.

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
| Daemon | **Optional for single mounts; required for §1's multiplexing** | One process per mount works for development and small-scale use; the `projgitd` daemon (designed but not built, Phase 6) is what delivers the "many containers, one upstream connection" property the §1 use case actually needs. |
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
- **EdenFS** virtualises beautifully but its fast path needs
  Mononoke (a source-control server you'd have to deploy or convert
  to), and its native client is Sapling, not git.
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
