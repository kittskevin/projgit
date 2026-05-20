# Session handoff — 2026-05-20: projgitd Stages 0–2

> Scope: just the work done in one focused session on 2026-05-20.
> Project-wide state lives in
> [`../implementation/handoff.md`](../implementation/handoff.md);
> this file captures what happened in this session specifically,
> so a future resume (or a new reader skimming history) doesn't
> have to re-derive context from commit messages.
>
> First entry in `docs/handoffs/`. The convention going forward
> (suggested): one file per substantive session, named
> `YYYY-MM-DD-<short-topic>.md`. Keep them in chronological order;
> don't merge or rewrite past entries.

## Session arc

Picked up after the May 18 work (container deployment design,
`--allow-other` flag, non-root smoke test all done). The big arc
today: **projgitd Stages 0, 1, 2 all shipped.** That's the entire
daemon-architecture backbone, from "no daemon, just one
process-per-mount" to "long-lived daemon with unix-socket control
plane serving multi-projection FUSE mounts."

## Commits landed (chronological, all pushed to `origin/main`)

| commit | what |
|---|---|
| `b9bc804` | spike(fuse-fd-passing): prove external-fd FUSE serving — GREEN |
| `747d3c1` | feat(cli): mount-multi subcommand + Stage 1 integration test |
| `03ee694` | docs(projgitd): Stage 1 done; update plan + handoff |
| `f33601e` | feat(daemon): projgit-daemon crate + protocol + scaffold (Stage 2a) |
| `8291a05` | feat(daemon): Mount/Umount RPCs + ActiveRepo + cache-stat status (Stage 2b) |
| `cb98d5a` | feat(cli): projgit attach client subcommand (Stage 2c) |
| `da9630c` | docs(projgitd): Stage 2 done; update plan, handoff, audit |

HEAD = `da9630c` at end-of-session, in sync with `origin/main`,
working tree clean.

## What each stage proved / shipped

- **Stage 0 (spike)** — `fuser::Session::from_fd` works. A process
  that did NOT open `/dev/fuse` and did NOT call `mount(2)` can
  fully serve the FUSE protocol on the resulting fd received via
  SCM_RIGHTS. The opener can exit cleanly; mount stays in the
  kernel namespace. **Stage 4 (T4 last-mile) is green-lit.**
  Throwaway code lives in
  [`../../spikes/fuse-fd-passing/`](../../spikes/fuse-fd-passing/README.md).
- **Stage 1 (`projgit mount-multi`)** — one process hosts N
  projections sharing one `ObjectStore` + `Fetcher` + caches.
  Chose **Path B (many mounts, shared store)** over Path A
  (dispatcher + inode rewrite) because the substrate was already
  multi-projection-ready and Path B matches Stages 3–4 naturally.
  Zero changes needed in `projgit-core`/`projgit-fuse`. The
  §1.6-in-memory amortisation claim now works in process
  (verified by `mount_multi.rs` integration test: mount B's read
  of an OID populated by mount A produces a shared `blob_cache`
  hit).
- **Stage 2 (`projgitd` + `projgit attach`)** — the same shared
  state, now over a unix-socket control plane. New crate
  `projgit-daemon`; new CLI subcommand `projgit attach
  {ping,status,shutdown,mount,umount}`. V1 is **one source per
  daemon** (first Mount fixes the source; `source_mismatch` on
  divergence). **Audit A1 (no daemon) and A3 (cross-process
  single-flight) are now architecturally closed**; Phase C bench
  becomes runnable (spawn two `projgit attach mount` clients,
  time cold reads).

## Tests added this session

| file | what it asserts | gating |
|---|---|---|
| `spikes/fuse-fd-passing/` (manual) | fd passing + `Session::from_fd` round-trip | manual |
| [`crates/projgit-fuse/tests/mount_multi.rs`](../../crates/projgit-fuse/tests/mount_multi.rs) | 2 mounts share `blob_cache` | `#[ignore]` (FUSE+git) |
| [`crates/projgit-daemon/tests/server_smoke.rs`](../../crates/projgit-daemon/tests/server_smoke.rs) | ping/status/shutdown lifecycle | always-on |
| [`crates/projgit-daemon/tests/mount_smoke.rs`](../../crates/projgit-daemon/tests/mount_smoke.rs) | full mount/umount via daemon | `#[ignore]` (FUSE+git) |
| [`crates/projgit-cli/tests/attach_smoke.rs`](../../crates/projgit-cli/tests/attach_smoke.rs) | CLI subprocess against in-thread daemon | both (one always-on, one `#[ignore]`) |
| daemon unit tests | protocol roundtrip, framing, dispatch | always-on |

`cargo test --workspace` stays green. Clippy clean on
`projgit-daemon` (one minor `sort_by` → `sort_by_key` nit fixed
during 2b).

## Gotchas hit + worked around

Saved to repo-scoped memory (not in git) for future sessions; also
captured here so this handoff stands on its own.

1. **VS Code stale-buffer overwrites** — `replace_string_in_file`
   on `docs/implementation/handoff.md` keeps reverting the
   §7 → §8 reference fix when the IDE auto-saves a stale buffer
   over disk. Workaround: do all handoff edits via shell-only
   (sed / python) and commit *immediately*.
2. **Curly vs ASCII apostrophe mismatches** — Python `\'` becomes
   ASCII `'`, but earlier `replace_string_in_file` calls wrote
   curly U+2019 to disk. `assert old in src` then silently aborts
   the whole script (Python only writes at `p.write_text(src)` at
   the end, so an assertion failure loses *all* earlier
   replacements). Cost ~10 minutes of debugging in the Stage 2b
   transition. The fix: read actual on-disk bytes first, then
   write the script.
3. **`ctrlc::set_handler` is process-wide / set-once** — putting
   it in the daemon's library `run()` made parallel tests fail
   silently (2nd+ test's `set_handler` returns `Err`). Moved to
   `main.rs` (the binary). Library only honours `Shutdown` over
   the wire; signal handling self-connects + sends `Shutdown`.
4. **`Session::run()` is `pub(crate)`** in fuser. Public way to
   drive the protocol loop is `Session::spawn() →
   BackgroundSession::join()`. Found while wiring the Stage 0
   spike.
5. **`#![forbid(unsafe_code)]` in projgit-cli** — almost shipped
   an `unsafe {{ libc::geteuid() }}` for the socket-path fallback.
   Caught at build; switched to `nix::unistd::geteuid()`. `nix`
   added as a dep to projgit-cli.

## Plan / design doc state at end-of-session

- **[`../design/projgitd.md`](../design/projgitd.md)** —
  unchanged today; was already correct (the plan doc had the
  DaemonFetcher-in-Stage-2 wording bug, design doc had it right).
- **[`../implementation/projgitd-plan.md`](../implementation/projgitd-plan.md)** —
  Stages 0/1/2 marked DONE with actual outcomes captured in their
  §X.4 / §X.5 sections.
- **[`../implementation/handoff.md`](../implementation/handoff.md)** —
  Done section has fresh bullets for Stages 0/1/2; "What I'd do
  next" #1 is now Stage 3.
- **Repo-scoped audit memory** (not in git, lives under
  `/memories/repo/audit.md`) — A1 and A3 moved from "open" to
  "closed since audit" with the daemon-Stage-2 details.

## Next up

Per [handoff "What I'd do next"](../implementation/handoff.md) at
end of session:

1. **`projgitd` Stage 3 — sidecar holds the FUSE fd.** Move the
   FUSE mount from the daemon to per-container sidecars. The
   daemon becomes pure data plane; sidecars run the protocol
   loop locally (via `fuser::Session::from_fd` — proven in
   Stage 0). Failure-mode upgrade: daemon crash → brief
   cold-path EAGAIN window vs killing every mount on the host.
   Needs `DaemonFetcher` — the new Fetcher that talks to the
   daemon over the wire (the one the original plan §2.1 text
   mistakenly placed in Stage 2).
2. A2 ref visibility (dotgit ladder, independent of projgitd).
3. Phase C concurrent bench (now genuinely runnable).
4. `projgitd` Stages 4–5 (T4 last mile; production polish).
5–8. CI bench, Windows backend, container recipe doc,
     tracing wiring.

## Open questions to settle when picking up Stage 3

- **Where does `DaemonFetcher` live?** Plan doc said
  `projgit-core::fetcher::daemon`, but that would force
  `projgit-core` to depend on `projgit-daemon` (protocol types).
  Cleaner: put it in `projgit-daemon` as a public type the sidecar
  side imports. Decide before writing code.
- **Sidecar binary or library-only?** The simplest sidecar is
  "projgit-cli with a different fetcher" — extend the existing
  `mount` / `mount-multi` subcommands with `--daemon-socket
  <path>` to switch to `DaemonFetcher`. Avoids a new binary
  entirely.
- **Protocol extension for fd passing?** Stage 3 itself doesn't
  need it (sidecar mounts its own fd; Stage 4 is when Harbor
  passes the fd in). But might be worth designing the protocol
  message now to avoid a breaking change later.

## Verifying it still works (sanity-check commands)

```sh
# default test suite stays green
cargo test --workspace

# Stage 2 end-to-end (needs FUSE + git CLI):
cargo test -p projgit-daemon --test mount_smoke -- --ignored

# CLI smoke against a real daemon:
SOCK=/tmp/projgitd-smoke.sock; MP=/tmp/attach-mp
rm -f "$SOCK"; mkdir -p "$MP"
cargo run --release --bin projgitd -- --socket "$SOCK" &
sleep 1
cargo run --release --bin projgit -- attach --socket "$SOCK" \
    mount /workspaces/projgit --ref main --mountpoint "$MP" --no-dotgit
ls "$MP"  # should serve the workspace
cargo run --release --bin projgit -- attach --socket "$SOCK" status
cargo run --release --bin projgit -- attach --socket "$SOCK" \
    umount --mountpoint "$MP"
cargo run --release --bin projgit -- attach --socket "$SOCK" shutdown
wait; rmdir "$MP"
```
