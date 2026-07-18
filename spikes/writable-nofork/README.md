# Phase 2 spike — no-fork writable virtual worktree

> Throwaway spike for [`docs/design/writable-worktrees.md`](../../docs/design/writable-worktrees.md)
> §10.1 (the gating "no-fork" question). NOT a workspace member; build
> and run it standalone from this directory.

## The question

Can **stock, unmodified git** drive a **virtual worktree** — `git status`
/ `git add` / `git commit` — correctly and fast, with **no
`core.virtualFilesystem` patch** (the hook GVFS forked git to add)? And
where exactly does cheap, side-effect-free `getattr` stop being enough,
so an upstream mechanism (sparse-index, FSMonitor) has to take over?

See [`RESULTS.md`](RESULTS.md) for the answer and the measured evidence.

## What's here

- `src/main.rs` — `vworktree`, a minimal **overlay FUSE filesystem**:
  - **lower layer** = a git commit's tree. Files are *virtual*: their
    bytes are fetched (via `git cat-file --batch`) only when `read()` is
    actually called. Every lower `read()` is counted as a **hydration**.
  - **upper layer** = in-memory materialisation for writes
    (`create`/`write`/`setattr`/`unlink`/`rename`). Edited files
    diverge from the tree; untouched files never hit disk.
  - a hidden control file `.nofork-stats` (resolvable by name, never
    listed in `readdir`, so git never sees it) exposes cumulative
    counters as `key value` lines.
  - a **write log** file (`--fsmonitor-file`) that records every changed
    path with a monotonic token — the stand-in for "projgit's daemon
    *is* the FSMonitor".
- `fsmonitor-hook.sh` — a git `core.fsmonitor` hook (query protocol v2)
  that answers from the write log with **zero filesystem scanning**.
- `run.sh` — the driver: builds a source repo, clones it
  `--no-checkout` (so `.git` has objects but no worktree files), mounts
  the virtual worktree, and runs the milestone experiments measuring
  hydration at each step.

This is deliberately **not** projgit's production FUSE backend (that one
is hard-wired read-only); the spike needs its own write path to exercise
`git add`/`commit`.

## Run it

```sh
# M1..M4 on a small repo (default 300 files):
./run.sh

# M2 scale + sparse-index (full M1..M4 first, then the scale section):
NFILES=20000 DIRS=200 ./run.sh scale
```

Requires: `git >= 2.37`, `/dev/fuse`, `fusermount`, `python3`, `cargo`.

## Reading the output

The one metric that matters is **`reads(hydrate)`** — the number of
`read()` calls served from the lower (git) layer, i.e. real content
hydration. `getattr` shows scanning cost; `writes`/`materialize` show
the write path engaging. A clean virtual worktree that git calls "clean"
with `reads(hydrate)=0` in steady state is the load-bearing result.
