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
> **All 16 Phase-2 commits are on `feat/writable-worktrees`, pushed to
> `origin`.** `main` is unchanged (= `origin/main`, `980d2c6`). HEAD =
> `1affe59`. Merge/PR the feature branch when ready.

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

## Proof (tests — all green)

- `cargo clippy --workspace --all-targets -- -D warnings` clean; full
  `cargo test --workspace` green.
- Core: `projgit-core/tests/dotgit_index.rs` — 9 tests incl.
  `writable_index_*` + `sparse_index_sets_skip_worktree_outside_cone`.
- FUSE (`#[ignore]`, need `/dev/fuse`): `projgit-fuse/tests/writable_mount.rs`
  — 4 tests: edit/add/create/commit, fsmonitor write-log, sparse-cone
  hides out-of-cone, swap-baseline + edit-survival.
- CLI (`#[ignore]`, real subprocess): `projgit-cli/tests/writable_mount_cli.rs`
  — `cli_writable_mount_edit_add_commit` and
  `cli_writable_mount_commit_and_push_to_branch` (bare remote + source:
  mount on `main`, edit, commit, `git push`, bare remote receives content).
- Run the ignored ones with `-- --ignored` (e.g.
  `cargo test -p projgit-cli --test writable_mount_cli -- --ignored`).

## The dev loop that now works

```bash
projgit mount --writable https://github.com/you/repo mnt &
cd mnt
$EDITOR src/foo.rs            # edit a tracked (still-virtual) file
git add -A && git commit -m "..."
git push                      # → origin/<branch>, no manual setup
```

## Honest limitations (read before trusting the demo)

1. **No persistence across unmount — the biggest gap.**
   `setup_writable_gitdir` **recreates the scratch dir fresh each mount**
   (`remove_dir_all`), and the upper is **in-memory**. So both
   *uncommitted* edits and *local-only* commits are lost on unmount unless
   `git push`ed. This is the top follow-up.
2. **`--writable` is exclusive** with `--subtree` / `--no-dotgit` /
   `--daemon-socket` (rejected at parse). No writable sidecar yet.
3. **git-checkout doesn't drive `swap_baseline` yet** — the swap works
   and is tested, but nothing wires stock `git checkout` to it; a
   checkout still materializes the diff.
4. **FSMonitor is a write-log *file*** read by a hook, not a live daemon
   socket. Same write-log; only the transport differs.
5. **Commit is correctness-only** — trees come from stock git re-reading
   the worktree, not derived from the materialized set via `gix`
   cache-tree (a perf follow-up).

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

## Next-up queue (recommended order)

1. **Persistence across unmount** *(highest value)*. Two parts, do (a)
   first: **(a)** don't `remove_dir_all` the scratch dir when it already
   exists — reuse it so committed refs survive a remount; **(b)** move
   the upper to an **on-disk overlay + minimal journal** (design §10.3)
   so *uncommitted* edits survive too. Removes the "edits lost on
   unmount" caveat that undercuts the demo.
2. **git-checkout integration** — drive `WritableHandle::swap_baseline`
   from stock `git checkout` (daemon observes the HEAD change +
   sparse/skip-worktree) instead of materializing the whole diff.
3. **Writable sidecar** — allow `--writable` + `--daemon-socket`
   together; move the FSMonitor write-log onto the daemon socket
   transport (R3 deferred piece).
4. **Commit perf** — derive trees from the materialized set via `gix`
   cache-tree so commit cost ∝ change, not worktree size.
5. **Partial-clone push correctness** — delta push works in tests;
   validate against a real partial clone / GitHub remote
   (`PROJGIT_NETWORK_TESTS=1`).

## State at handoff

- Branch `feat/writable-worktrees` pushed & clean; `main` = `origin/main`.
- Phase 2 is **feature-complete for a single mount session** (edit →
  commit → push). The remaining work is **persistence** and
  **integration polish** (checkout-driven swap, sidecar, commit perf),
  not new core mechanism.
- Plan doc (`writable-worktrees-plan.md`) reflects all of the above as
  shipped; its §2 Stage 7 "Remaining follow-ups" and §3 out-of-scope are
  the authoritative to-do list alongside this handoff's queue.
