# Session handoff — 2026-07-18: writable worktrees (Phase 2), end to end

> Scope: this session took **Phase 2 (writable worktrees)** from a
> design gate all the way to a **usable CLI dev loop** — spike → design
> update → 7 build stages → follow-ups → CLI `--writable` → remote+branch
> wiring so `edit → commit → push` works with no manual setup.
> Design lives in
> [`../design/writable-worktrees.md`](../design/writable-worktrees.md);
> the stage plan (as-built) in
> [`../implementation/writable-worktrees-plan.md`](../implementation/writable-worktrees-plan.md).
> This doc is the running narrative so a resume doesn't re-derive it.
>
> **All 28 Phase-2 commits are on `feat/writable-worktrees`, pushed to
> `origin`.** `main` is unchanged (= `origin/main`, `980d2c6`). HEAD =
> `b772694`. The branch is **feature-complete and green** — persistence,
> checkout integration, stock-git observation, opt-in fsmonitor, writable
> sidecar, validated partial-clone push, and blob GC all landed after the
> mid-session point this doc was first written. **Ready to merge — open a
> PR to `main`.**

## Branch state (read first)

- **Working branch:** `feat/writable-worktrees` (pushed, tracking
  `origin/feat/writable-worktrees`). Clean tree, nothing unpushed.
- **`main`:** reset to `origin/main` (`980d2c6`) with `git branch -f`
  (non-destructive — the commits live on the feature branch). Do **not**
  assume Phase-2 work is on `main`.
- **Convention going forward:** keep committing to
  `feat/writable-worktrees` with conventional-commit messages; run
  `cargo clippy --workspace --all-targets -- -D warnings` +
  `cargo test --workspace` + the ignored FUSE tests before each commit;
  mark the plan doc; push after each commit.

## Session arc

1. **Passed the no-fork gate.** Built a throwaway spike
   (`spikes/writable-nofork/`) proving stock git can drive a *virtual*
   worktree with **no fork** — clean first `status`, edit detection,
   fsmonitor scan reduction. Verdict GREEN (its `RESULTS.md`); design
   §12 updated.
2. **Turned the spike into four bounded requirements** (R1–R4) and an
   ordered stage plan (`writable-worktrees-plan.md`).
3. **Built Stages 1–7** (writable index → overlay → invalidation →
   fsmonitor → sparse cone → commit → checkout-under-live-mount), each
   with a real-FUSE integration test.
4. **Productionized** as `projgit mount --writable`.
5. **Wired the dev loop**: named branch + inherited remote so
   `git push` works out of the box.
6. **Made it persistent**: reused on-disk scratch (committed work) + an
   on-disk crash journal for the upper (uncommitted edits). Nothing is
   lost on unmount.
7. **Integrated checkout**: `projgit checkout` + a `reference-transaction`
   hook so **stock** git ops re-project the live mount over a control
   socket; every swap reconciles the upper.
8. **Streamed fsmonitor over the socket** and added a **writable sidecar**
   (`--writable` + `--daemon-socket`).
9. **Hardened partial clones**: validated `edit → commit → push` against a
   real `blob:none` GitHub clone (fixed 2 bugs) and pre-populated an FSMN
   index so the first `status` doesn't mass-hydrate the tree.
10. **Bounded the upper**: GC of stale content-store blobs on compaction.

## Commits (chronological, all on `feat/writable-worktrees`, pushed)

| commit | what |
|---|---|
| `e924d15` | docs(bench): record cache+transform tier read-only results |
| `2e5c4b9` | spike(writable-nofork): stock git drives a virtual worktree — GREEN |
| `031aa10` | docs(design): writable-worktrees §12 — no-fork spike passed |
| `c37b7e5` | docs(plan): add writable-worktrees Phase 2 implementation plan |
| `c8c3bc2` | feat(core): writable-mode index synthesis (Stage 1 / R1) |
| `2993564` | feat(fuse): writable worktree overlay (Stage 2) |
| `cdedbe3` | feat(fuse): off-thread FUSE cache invalidation on write (Stage 3 / R4) |
| `7ce1e44` | feat(fuse): FSMonitor write-log from the writable overlay (Stage 4 / R3) |
| `845db4e` | feat(fuse): sparse-cone projection filtering (Stage 5 / R2) |
| `66878c4` | test(fuse): verify edit→add→commit cycle on the overlay (Stage 6) |
| `7c54bfb` | docs(plan): capture Stage 7 design (checkout-under-live-mount) |
| `79b1076` | feat(fuse): swap-baseline under a live mount (Stage 7 core) |
| `134ee5e` | feat(fuse): path-keyed overlay — edits survive checkout (Stage 7 follow-up) |
| `79c631a` | feat(core): sparse writable index — SKIP_WORKTREE out-of-cone (Stage 5 follow-up) |
| `01cdd97` | feat(cli): `projgit mount --writable` — usable writable worktree |
| `1affe59` | feat(cli): writable mount — branch + remote wiring for push |
| `18fe468` | docs(handoff): session handoff for writable worktrees (this doc) |
| `ae61806` | feat(cli): persist committed work across unmount (reused scratch) |
| `f82497e` | feat(fuse): persist the uncommitted upper via a crash journal |
| `392c2b3` | feat: checkout-under-live-mount — HEAD watcher + `projgit checkout` |
| `b9161c1` | feat(fuse): reconcile the upper on every baseline swap |
| `7cea4f0` | feat(cli): synchronous `projgit checkout` over a control socket |
| `71737ee` | feat(cli): observe stock git ops via a reference-transaction hook |
| `55e55e8` | feat: FSMonitor over the mount control socket (`--fsmonitor`) |
| `f242a69` | feat(cli): writable sidecar — `--writable` + `--daemon-socket` |
| `f2847eb` | fix(core,cli): partial-clone writable correctness (validated vs GitHub) |
| `beb0dd1` | feat(core,cli): pre-populated FSMN index — no first-status hydration |
| `b772694` | feat(fuse): GC stale blobs from the upper journal store |

## What got built (by requirement)

- **R1 — writable index synthesis** (`projgit-core/src/dotgit.rs`).
  `build_writable_index_bytes(store, commit)` and
  `build_writable_index_bytes_sparse(store, commit, cone)`: real
  size-from-header + stable commit-time mtime, **no `ASSUME_VALID`**,
  paired with `WRITABLE_CORE_CONFIG` (`core.checkStat=minimal`). Sparse
  variant sets `EXTENDED|SKIP_WORKTREE` on out-of-cone entries. The
  read-only `a1_plus_overlay` (ASSUME_VALID) path is untouched.
- **R2 — sparse cone** (`WritableFs` + the sparse index above). Cone-mode
  visibility in `lookup`/`readdir` keeps git's sparse index collapsed.
- **R3 — FSMonitor write-log** (`WritableFs` + `MountConfig::fsmonitor_file`).
  Cumulative modified-path set behind **monotonic nanosecond tokens**
  (git rejects small-integer tokens); a `core.fsmonitor` hook (query
  protocol v2) streams it so git skips scanning.
- **R4 — FUSE invalidation** (`WritableFs`). Off-thread worker holds the
  post-mount `Notifier` and drains `inval_inode`/`inval_entry`
  (calling the notifier from inside a handler **deadlocks the kernel**);
  TTL restored to 1s.

- **The overlay** (`projgit-fuse/src/writable.rs`, ~800 lines):
  `WritableFs<F: FsProvider>` is **path-keyed** — `edits: HashMap<String,
  Edit>` + `whiteouts: HashSet<String>` over an immutable LOWER
  projection, with a per-baseline inode cache. Materialize-on-write.
  `mount_writable_background[_with_handle]`. `WritableHandle::swap_baseline`
  swaps the LOWER under a live mount (a `checkout`) and **local edits
  survive** (EdenFS semantics) because they're path-keyed.

- **CLI** (`projgit-cli/src/main.rs`): `projgit mount --writable <src>
  <mnt>` builds a real on-disk scratch git dir
  (`/tmp/projgit-wt-<hash>.git`), `objects/info/alternates` → the shared
  store, writable seeded index, `core.worktree` = mount; the mount's
  `.git` is a `gitdir:` **link file** to it. Now starts on a **named
  branch** (symbolic HEAD → `refs/heads/<branch>`) and inherits the
  clone's `remote.origin.url` + branch upstream, so `git push` works.

## What got built after the dev loop (persistence → integration → hardening)

The 12 commits after `1affe59` took the single-session dev loop to a
persistent, integrated, partial-clone-correct feature:

- **Persistence across unmount** (`ae61806`, `f82497e`). The scratch git
  dir moved to `<cache>/worktrees/` and is **reused** (not recreated);
  its `objects` is a symlink into the shared CAS, so *committed* work is
  durable. The upper (uncommitted edits + whiteouts) is backed by an
  **on-disk crash journal** (`projgit-fuse/src/upper_journal.rs`):
  append-only fsync'd `journal` + content-addressed `blobs/`. On mount it
  replays, **reconciles** against the re-pinned baseline (drops
  now-committed edits), and compacts. Committed *and* uncommitted work
  survive with no user action.
- **Checkout under a live mount** (`392c2b3`, `b9161c1`). `projgit
  checkout <ref>` re-projects the running mount with no worktree rewrite
  (read-tree + HEAD move → `swap_baseline`); path-keyed edits survive;
  every swap reconciles the upper so stock and virtual checkout converge.
- **Stock-git observation via hooks** (`71737ee`, `7cea4f0`). A
  `reference-transaction` hook makes **stock** `git commit/checkout/reset`
  drive the swap synchronously over a per-mount **control socket**
  (`projgit-control.sock`); the HEAD poll watcher is now only a fallback.
- **FSMonitor over the socket** (`55e55e8`). Opt-in `--fsmonitor`
  installs a `core.fsmonitor` hook that streams the modified-path set
  from the mount over the control socket, so git skips scanning.
- **Writable sidecar** (`f242a69`). `--writable` composes with
  `--daemon-socket`: the scratch attaches to a running `projgitd`'s
  object store (`prepare_writable` shared by standalone + sidecar).
- **Partial-clone correctness + no first-status hydration** (`f2847eb`,
  `beb0dd1`). Validated `edit → commit → push` over a real `blob:none`
  clone against GitHub (2 bugs fixed: size-fallback for absent blobs,
  promisor config propagation). `--fsmonitor` **pre-populates an FSMN
  index extension** marking every entry valid, so the first `status`
  doesn't mass-hydrate the tree (the GVFS `core.virtualfilesystem`
  problem) — proven: missing-blob count unchanged across the first scan.
- **Upper blob GC** (`b772694`). Compaction garbage-collects blobs no
  longer referenced by the live journal, so the content store stays
  bounded.

## Proof (tests — all green)

- `cargo clippy --workspace --all-targets -- -D warnings` clean; full
  `cargo test --workspace` green.
- Core: `projgit-core/tests/dotgit_index.rs` — 9 tests incl.
  `writable_index_*` + `sparse_index_sets_skip_worktree_outside_cone`.
- FUSE (`#[ignore]`, need `/dev/fuse`): `projgit-fuse/tests/writable_mount.rs`
  — 4 tests: edit/add/create/commit, fsmonitor write-log, sparse-cone
  hides out-of-cone, swap-baseline + edit-survival.
- CLI (`#[ignore]`, real subprocess): `projgit-cli/tests/writable_mount_cli.rs`
  — **12 tests**: edit/add/commit, commit-and-push-to-branch, persist
  committed + persist uncommitted across remount, `projgit checkout`
  (live + synchronous over the socket), stock-commit reconcile via hook,
  fsmonitor over socket, partial-clone edit/commit, fsmonitor-avoids-
  partial-clone-hydration, and journal blob GC.
- Daemon sidecar (`#[ignore]`, real `projgitd`):
  `projgit-daemon/tests/xprocess_mount_smoke.rs::xprocess_writable_sidecar_edit_commit`.
- Run the ignored ones with `-- --ignored` (e.g.
  `cargo test -p projgit-cli --test writable_mount_cli -- --ignored`).

## The dev loop that now works

```bash
projgit mount --writable https://github.com/you/repo mnt &   # add --fsmonitor for big/partial clones
cd mnt
$EDITOR src/foo.rs            # edit a tracked (still-virtual) file
git add -A && git commit -m "..."   # stock git; the hook reconciles the upper
git push                      # → origin/<branch>, no manual setup
projgit checkout other-branch # re-projects the live mount, no worktree rewrite
# unmount + remount the same path later: committed + uncommitted work is restored
```

## Status of the earlier "honest limitations" (four fixed, one by-design)

The mid-session handoff listed five caveats. Four are now **fixed**; one
is **out of scope by design**:

1. ~~No persistence across unmount~~ — **FIXED** (`ae61806`, `f82497e`).
   Committed *and* uncommitted work survive unmount (reused scratch +
   on-disk crash journal). Nothing is lost without `git push`.
2. ~~`--writable` exclusive with `--daemon-socket`~~ — **FIXED**
   (`f242a69`). Writable sidecar composes with a running `projgitd`.
   (`--writable` still rejects `--subtree` / `--no-dotgit` by design.)
3. ~~Stock checkout doesn't drive `swap_baseline`~~ — **FIXED**
   (`71737ee`, `7cea4f0`). A `reference-transaction` hook drives the swap
   from stock git; `projgit checkout` does it synchronously over the
   control socket.
4. ~~FSMonitor is a write-log file, not a socket~~ — **FIXED**
   (`55e55e8`). `--fsmonitor` streams the modified set over the control
   socket via a `core.fsmonitor` hook.
5. **Commit re-reads the worktree** (perf, not correctness) — still true,
   but **blob GC** (`b772694`) subsumed most of the "derive trees from
   the materialized set" motivation; deferred as marginal.

**The one remaining Phase-2 boundary (out of scope by design):** making
**stock** `git checkout` stay *fully virtual* for unmodified files needs
`SKIP_WORKTREE` management (racy index writes) or a thin
`core.virtualfilesystem`-style git integration. Today stock checkout
works correctly but eagerly materializes the diff; `projgit checkout`
stays virtual.

## Gotchas / lessons (carried from this session)

- **`SKIP_WORKTREE` needs `EXTENDED` too**, or gix serializes a V2 index
  and silently drops the flag. Set `EXTENDED|SKIP_WORKTREE`.
- **git rejects small-integer fsmonitor tokens** (stores `0`, drops all
  deltas). Use nanosecond-timestamp-shaped tokens.
- **Never call `Notifier` from inside a FUSE handler** — it can deadlock
  the kernel. Enqueue to an off-thread worker.
- **Path-key the upper, not inode-key it.** Inode-keyed edits get
  orphaned by a baseline swap; path-keyed edits survive checkout for free
  (and it's less code — collapsed several maps).
- **`.git` can be a FILE** (`gitdir: <path>`) — that's how the mount
  points stock git at the external writable git dir while the worktree is
  the FUSE mount.
- `CARGO_TARGET_DIR = /workspaces/projgit/target` (global); binary at
  `target/{debug,release}/projgit`. Rebuild before a manual mount bench —
  a stale binary silently runs old code.

## Next-up queue

Every item from the mid-session queue (persistence, checkout integration,
sidecar, commit perf / blob GC, partial-clone push) is **done**. Phase 2
is feature-complete. The realistic options going forward:

1. **Merge to `main`** *(this milestone)*. The branch (28 commits, HEAD
   `b772694`) is green: clippy clean; workspace + 12 CLI + 4 FUSE +
   sidecar writable tests pass. Open a PR `feat/writable-worktrees` →
   `main`.
2. **Stock-checkout-stays-fully-virtual** *(hard/forky, optional)* — the
   one remaining boundary above; needs `SKIP_WORKTREE` juggling or a thin
   git integration.
3. **New phase** — e.g. the `projgit-winfsp` crate (Windows; currently a
   stub — see `../design/winfsp-implementation-plan.md`), or a perf/bench
   pass on the writable path.

## State at handoff

- Branch `feat/writable-worktrees` pushed & clean; HEAD `b772694`; `main`
  = `origin/main` (`980d2c6`), 28 commits ahead.
- **Phase 2 is feature-complete**: virtual reads, real edits, stock-git
  `add`/`commit`/`push`, persistence (committed + uncommitted, GC'd),
  `projgit checkout` + stock-checkout observation, opt-in fsmonitor with
  no partial-clone hydration, and a writable daemon sidecar — all
  no-fork. Validated live against GitHub (throwaway branch, deleted).
- The only remaining Phase-2 item is the out-of-scope-by-design
  stock-checkout-stays-virtual boundary.
- Authoritative plan: `../implementation/writable-worktrees-plan.md`.
- **Ready to merge — open the PR to `main`.**
