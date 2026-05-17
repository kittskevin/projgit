# Design: `.git/` Synthesis & `RootOverlay`

> Status: **mechanism decided; content deferred**. Companion to
> [../initial-plan.md](../implementation/initial-plan.md) §9.3. Captures the option
> ladder, UX trade-offs, complexity analysis, the architectural
> commitment we are making in MVP, and the criteria for promoting the
> deferred content design into MVP later.

## 1. Problem statement

A `projgit` mount exposes a projection of a git tree at a directory.
Many tools that developers run inside such a directory walk upward
looking for a `.git/` directory and change behavior based on what they
find:

- `rg` / `ag` / `fd` — VCS-aware ignore handling.
- `git` itself — every porcelain and plumbing command.
- `cargo` — records the source-control revision in build artifacts.
- IDEs and editors — branch indicator, blame, file decorations.
- Build / test runners — version stamping, "dirty" detection.
- Linters and formatters with project-root detection.

Without a `.git/`, these tools either fall back to defaults (often
surprising) or refuse to operate. With a *fake* `.git/`, we risk an
"uncanny valley" where read commands work but writes fail in confusing
ways.

There is no single right answer. This document lays out the option
space and the decision we are taking *now* (mechanism), separately from
the decision we are deferring (content).

## 2. Two halves of the decision

The decision separates cleanly:

1. **Architectural \u2014 `RootOverlay`.** Does the projection engine
   support synthetic entries at the projection root that aren't backed
   by a git tree? *Decided: yes, in Phase 1.*
2. **Content \u2014 what goes in `RootOverlay`.** What synthetic entries do
   we ship by default? *Deferred.*

Half (1) is load-bearing on Phase 1's data model and cannot be
retrofitted without touching `lookup`, `readdir`, and the inode
allocator. Half (2) is ~80\u20132000 LOC of glue (depending on variant)
that can land any time without disturbing the engine.

## 3. Goals & non-goals

### Goals

- **Honesty over almost-works.** When operations don't work, they should
  fail with a clear message, not a cryptic git error.
- **Read-only invariant preserved.** Nothing we synthesize may suggest
  the mount is writable as a git repo.
- **Cross-platform.** Whatever we synthesize must behave consistently
  across `fuser` and `winfsp` backends.
- **Optionality.** Make the architectural choice once; defer the content
  choice as long as evidence is missing.
- **Low public surface.** Anything we ship is a compatibility commitment
  for the project's lifetime; don't ship surfaces we can't justify.

### Non-goals

- A faithful, writable git repo simulation (the A3 variant in \u00a74). That
  contradicts the read-only invariant.
- Synthesizing the commit graph or pack files. Tools that need them
  should consume the shared object store directly via documented means
  (e.g. `--git-dir <store>`).
- Mediating tool behavior beyond what `.git/` presence implies.
  Per-tool integrations (`projgit-rg`, IDE plugins) are explicitly out.

## 4. Option ladder

### 4.1 Within "synthesize a `.git/`" \u2014 the A ladder

Each variant is a strict superset of the previous one.

| Variant | Contents of `.git/` | Tools enabled by this rung |
|---|---|---|
| **A0 \u2014 Marker only** | `HEAD` containing the commit OID (detached form) | "Is this a repo?" detection (`rg`, `cargo`, IDE root detection) |
| **A1 \u2014 Minimal repo** | A0 + `config`, `objects/info/alternates` \u2192 shared store, empty `refs/`, empty `packed-refs` | `git rev-parse HEAD`, `git log`, `git cat-file`, `git show`, `git diff <ref>..<ref>` |
| **A2 \u2014 Refs visible** | A1 + symbolic `HEAD` \u2192 `refs/heads/<name>`, plus `refs/heads/<name>` file with the OID, when projection is a `Ref` | A1 + `git branch --show-current`, IDE branch indicator, `git log --all` (sees the one ref) |
| **A3 \u2014 Writable illusion** | A2 + writable `index`, `ORIG_HEAD`, reflog, hooks dir | A2 + `git status`, `git diff` against working tree, IDE file-level VCS decoration |

### 4.2 Within "no `.git/`" \u2014 the B variants

| Variant | Contents at mount root | Tools enabled |
|---|---|---|
| **B \u2014 Pure tree** | Just the projected tree | None VCS-aware |
| **B+ \u2014 Pure tree + sentinel** | Tree + a small JSON file describing the projection | None VCS-aware, but `projgit` tools and curious users have a clean way to identify the mount |

## 5. UX scenario matrix

The scenarios below are deliberately limited to the high-frequency
cases that drive the decision. \u2705 = works as expected; \u26a0\ufe0f = appears
to work but has gotchas; \u274c = does not work.

| Scenario | A0 | A1 | A2 | A3 | B | B+ |
|---|---|---|---|---|---|---|
| `rg "TODO"` respects `.gitignore` | \u2705 | \u2705 | \u2705 | \u2705 | \u274c | \u274c |
| `git rev-parse HEAD` | \u2705 | \u2705 | \u2705 | \u2705 | \u274c | \u274c |
| `git log` | \u274c bad object HEAD | \u2705 | \u2705 | \u2705 | \u274c | \u274c |
| `git branch --show-current` | "HEAD" detached | "HEAD" detached | \u2705 branch name | \u2705 | \u274c | \u274c |
| IDE statusbar shows branch | sometimes | sometimes | \u2705 | \u2705 | \u274c | \u274c |
| `git status` | \u274c | \u26a0\ufe0f noisy ("index missing") | \u26a0\ufe0f | \u2705 | \u274c clear | \u274c clear |
| `git add .` | \u274c confusing | \u274c "permission denied" | \u274c | \u26a0\ufe0f succeeds-then-discarded | \u274c clear | \u274c clear |
| `git push` / `git checkout other` | \u274c confusing | \u274c | \u274c | \u274c | \u274c clear | \u274c clear |
| `cargo build` records VCS info | \u2705 | \u2705 | \u2705 | \u2705 | \u274c "no VCS" | \u274c |
| User identifies "what commit am I looking at?" | \u2705 (`HEAD`) | \u2705 | \u2705 | \u2705 | \u274c | \u2705 (`info.json`) |
| `git clone /mount /elsewhere` | \u274c | \u26a0\ufe0f partial \u2014 ok for hydrated blobs, fails on un-hydrated | \u26a0\ufe0f | \u26a0\ufe0f | \u274c | \u274c |

Two asymmetries dominate:

1. **`rg` and `cargo`** care only about `.git/` *presence*. A0 satisfies them.
2. **`git status` / `git add`** failures are the worst-UX cells in the
   matrix \u2014 looks like a repo, breaks on common verbs. Only A3 fixes
   them; B / B+ avoid them by being honestly not-a-repo.

## 6. Implementation complexity

Rough LOC estimates including tests; assumes `RootOverlay` already
exists.

| Variant | Approx. LOC | New mechanism beyond `RootOverlay` | Cross-platform concerns |
|---|---|---|---|
| A0 | ~50 | none | none |
| A1 | ~250 | virtual subdir tree under `.git/`; alternates path resolution | path normalization on Windows |
| A2 | ~400 | A1 + ref \u2194 commit awareness in projection state | same as A1 |
| A3 | ~2000+ | writable scratch overlay per mount, lock-file emulation, index synthesis | significant; index format is fiddly, lock files behave differently across OSes |
| B | 0 | none | none |
| B+ | ~80 | one synthetic file in `RootOverlay` | none |

Note that **anything except pure B** requires `RootOverlay`. That is
exactly why we are separating the architectural decision (ship the
mechanism) from the content decision (defer).

## 7. Risks per option

### A risks

1. **Uncanny-valley UX.** A1 / A2 *look* like a checkout but break on
   write verbs, with confusing errors. This is the worst kind of
   user-facing failure mode \u2014 almost works.
2. **Lock-file semantics.** `.git/index.lock` is created by virtually
   every git write operation. Read-only synthesis fails the first
   write; the resulting "another git process seems to be running"
   error is misleading.
3. **Alternates leaks.** `objects/info/alternates` contains the
   absolute on-disk path of the shared store. Sharing logs or running
   `git config --list` exposes that path; for shared CI this can be
   sensitive.
4. **HEAD ambiguity for non-`Ref` projections.** `Commit` and `Subtree`
   projections genuinely have no branch \u2014 they're snapshots. A2's
   symbolic `HEAD` doesn't apply. The projection-kind switch lives at
   the user's most-touched file, which is fragile.
5. **`git clone` from the mount.** With alternates, clone *appears* to
   work but silently fails for un-hydrated blobs. We have no clean
   hook to detect or block the clone.
6. **A3 contradicts the read-only invariant.** Once an index is
   writable, users will reasonably expect to write more. A3 is at
   minimum a Phase 6 / read-write-MVP item.

### B / B+ risks

1. **`rg` annoyance is real.** It's the most-cited UX paper-cut.
   Worked-around by `--no-ignore-vcs=false` or `.ignore` files but the
   workaround burden is on the user.
2. **Support load.** "Why doesn't `git log` work?" is a foreseeable
   question; needs an FAQ entry.
3. **CI / build-system metadata divergence.** Build artifacts won't
   carry a git SHA unless the build script reads our sentinel. Some
   users will care.
4. **B+ specifically:** the sentinel format is a public surface that's
   hard to remove later.

## 8. Two leading recommendations

### R1 \u2014 B+ default, A0 opt-in (deferred-leaning)

- Ship B+ in MVP: pure tree plus `.projgit/info.json`.
- Add `projgit mount \u2026 --emit-dotgit=minimal` for users who want A0.
- Promote toward A1/A2 only with evidence.

Strengths: zero uncanny-valley by default; small public surface;
honest failure modes for write verbs.

Weaknesses: `rg` and `cargo` users feel the friction immediately.

### R2 \u2014 A1 default, B+ opt-out (commit-leaning)

- Ship A1 in MVP: detached `HEAD`, `objects/info/alternates`, empty refs.
- Add `projgit mount \u2026 --no-dotgit` for users who want pure tree.
- A2 lands behind a per-projection flag if user demand appears.

Strengths: maximum day-one transparency; rg/cargo/git-log/git-rev-parse
all work.

Weaknesses: accepts uncanny-valley write failures; commits to
alternates-leak risk; A1 is ~250 LOC of code we own forever.

## 9. Decision

> **Update 2026-05-17:** the A1 variant has been promoted from
> deferred to shipped-as-default. The trigger was the project audit
> identifying problem-statement §7 #4 (`git log <path>` works) as the
> most visibly deferred success criterion. §9.3 promotion criterion #2
> ("actual workflow blocker traceable to the missing `.git/`") is the
> rubric this falls under — the workload simply needs git porcelain
> to work for the eval use case to feel real. §9.1 through §9.4 below
> document the *original* mechanism-only commitment; the new
> additions live in §9.5.

**Defer the content decision. Lock the mechanism decision.**

### 9.1 Decided (in MVP)

- **`RootOverlay` ships in Phase 1.** Implemented as a
  `BTreeMap<&str, SyntheticEntry>` consulted by `lookup` and `readdir`
  at the projection root **before** falling through to the real tree.
  MVP overlay is empty.
- **Engine API contract:** synthetic entries take precedence over real
  tree entries with the same name (collision resolution: warn-once,
  hide the real entry). Synthetic entries get inode IDs from a
  reserved namespace (`projection_id` high-bit set) so they never
  collide with tree-derived inodes.

### 9.2 Deferred (future ship-default: R1)

- **Future content default:** R1 \u2014 a sentinel file `.projgit/info.json`
  plus an opt-in `--emit-dotgit=minimal` flag (the A0 variant).
- **Future sentinel sketch** (committed-to as a *direction*, not a
  schema; not implemented in MVP):
  ```jsonc
  // .projgit/info.json
  {
    "schema_version": 1,
    "projection_kind": "ref" | "commit" | "subtree",
    "commit_oid": "<hex>",
    "ref_name": "refs/heads/main",          // present iff kind == "ref"
    "subtree_path": "src/foo",              // present iff kind == "subtree"
    "store_id": "<opaque>",                 // identifies the shared object store
    "mounted_at_iso8601": "2026-05-08T12:34:56Z"
  }
  ```
- The sentinel lives at `.projgit/info.json` (subdirectory) rather than
  `.projgit-info.json` (top-level file) to keep the top of the mount
  visually clean and to leave room for future siblings under
  `.projgit/` without expanding the top-level surface.
- **Future opt-in flag:** `projgit mount \u2026 --emit-dotgit={none|minimal}`,
  default `none`. `minimal` produces an A0 `.git/HEAD` only.

### 9.3 Promotion criteria

Any one of the following moves the deferred R1 design into MVP:

1. **Phase 5 test-write friction.** A planned integration test is
   materially harder to write without a sentinel or `.git/`-marker.
2. **Beta user blocker.** A beta user reports an actual workflow
   blocker traceable to the missing `.git/`. (Single report is enough
   to trigger a *review*; we may still defer if the workaround is
   trivial.)
3. **Schema bikeshed deadlock.** We can't agree on the `info.json`
   schema after one more design pass. Shipping the simplest version
   forces clarity.

### 9.4 Explicitly rejected

- **A3** for any milestone before read-write support. Contradicts the
  read-only invariant and is ~2000+ LOC of git plumbing we don't need.
- **Bind-mounting / exposing the real shared store at `.git/`.**
  Leaks paths and refs across sibling projections; the shared store is
  projection-agnostic by invariant.

### 9.5 Promoted (2026-05-17): A1 as shipped default

After the original mechanism-only commitment, the audit identified
problem-statement §7 #4 (`git log <path>` works) as the most visibly
deferred success criterion. A1 — not R1 — is the lowest variant that
satisfies it, and the in-tree `RootOverlay` mechanism already supported
the nested synthetic directories A1 needs. Promotion landed as:

- **`crate::dotgit::a1_overlay(commit_oid, objects_dir)`** in
  `projgit-core` builds the A1 overlay (HEAD detached at the commit,
  minimal `[core]` config, empty `refs/heads/`, empty `refs/tags/`,
  empty `packed-refs`, and `objects/info/alternates` pointing at the
  shared store's `objects/`).
- **`projgit mount` synthesizes it by default** for `Ref` and `Commit`
  projections. `--no-dotgit` opts out; `Subtree` projections opt out
  automatically because `.git/HEAD` would point at the full commit's
  tree rather than the subtree the user is browsing (the original
  §7 risk #4, surfaced verbatim).
- **FUSE adapter echoes the requesting process's uid/gid as file
  ownership** so git's `safe.directory` check passes without the user
  having to run `git config --global --add safe.directory <mount>`.
  Same mechanism a future WinFsp backend will need (see the WinFsp
  implementation plan's per-user volume ownership note).
- **Network-gated end-to-end test**
  (`crates/projgit-fuse/tests/mount_real_remote.rs ::
  mount_real_remote_with_dotgit_supports_git_log`) partial-clones
  `rust-lang/log`, mounts with the A1 overlay through the real FUSE
  backend, and asserts `git rev-parse HEAD`, `git log -1`, and
  `git log -1 -- src/lib.rs` all succeed from inside the mount with
  no user configuration.

The original §7 A-risks remain on the table (uncanny-valley write
failures, alternates path leak in `git config --list`). They are
acceptable given the read-only contract is the same one the rest of
projgit advertises. The R1 sentinel design and A2 / A3 variants stay
deferred per §9.2 and §9.4.

## 10. `RootOverlay` mechanism \u2014 the architectural commitment

Because this is the only part shipping in MVP, it gets a precise spec.

### 10.1 Data model

```rust
pub struct RootOverlay {
    entries: BTreeMap<String, SyntheticEntry>,
}

pub enum SyntheticEntry {
    File {
        content: SyntheticContent,        // bytes or generator closure
        mode: u32,                        // git-mode-style; usually 0o100644
    },
    Directory {
        children: BTreeMap<String, SyntheticEntry>,
    },
    Symlink {
        target: String,
    },
}

pub enum SyntheticContent {
    Inline(Bytes),
    /// Generated lazily on read; must be deterministic given the projection.
    Generated(Arc<dyn Fn(&ProjectionContext) -> Bytes + Send + Sync>),
}
```

### 10.2 Resolver integration

```
fn resolve(projection, virtual_path):
    let (head, tail) = split_first(virtual_path);
    if let Some(entry) = projection.root_overlay.entries.get(head):
        return resolve_synthetic(entry, tail);
    return resolve_tree(projection.commit, virtual_path);
```

`readdir` at the root concatenates `root_overlay.entries.keys()` with
the real tree's children, deduplicating by name (overlay wins, real
entry hidden, warn-once log).

### 10.3 Inode IDs

Synthetic entries get inodes from a reserved namespace so they never
collide with tree-derived inodes:

```
synthetic inode = (1u64 << 63) | hash64(projection_id, synthetic_path)
real-tree inode = hash64(projection_id, blob_oid, path_hash)   // top bit always 0
```

### 10.4 Properties guaranteed

- **Read-only.** `SyntheticEntry` has no write operations.
- **Stable across reads.** A `Generated` content closure must produce
  the same bytes for a given `ProjectionContext`. Tested with
  property-style assertions.
- **Cross-platform.** `RootOverlay` is OS-agnostic; both `fuser` and
  `winfsp` backends consume it through the `FsProvider` trait.

## 11. Test plan (MVP \u2014 mechanism only)

### 11.1 Unit (cross-platform)

- Empty overlay behaves as a passthrough: `readdir` and `lookup` match
  the underlying tree exactly.
- Non-empty overlay with a single synthetic file:
  - `lookup` returns the synthetic entry.
  - `readdir` includes the synthetic name.
  - Reading returns the inline bytes.
- Overlay collision with a real tree entry:
  - Synthetic wins.
  - Warn-once is emitted (assert via `tracing-subscriber` test layer).
- Overlay synthetic directory + nested file: lookup of nested path
  works.
- Synthetic-inode top-bit invariant: no overlap with tree-derived
  inodes for any combination in a fixture repo.
- `Generated` content called twice yields identical bytes.

### 11.2 Integration

- Mount a fixture repo with an empty overlay and confirm tree contents
  are byte-identical to a `git archive` of the same commit.
- Mount the same fixture repo with a one-entry test overlay
  (`.projgit-test/marker`) and confirm both the marker and the real
  tree are visible together.

(No tests for the deferred sentinel content yet; those land when the
content design is promoted.)

## 12. Test plan (future \u2014 when content is promoted)

Recorded here so we don't re-derive it later.

### 12.1 If R1 (sentinel) is promoted

- `.projgit/info.json` exists, parses as JSON, matches schema.
- Schema version round-trips.
- All projection kinds populate the right fields.
- Sentinel is identical across two simultaneous mounts of the same
  projection.

### 12.2 If `--emit-dotgit=minimal` is promoted

- `cd <mount> && git rev-parse HEAD` returns the projection's commit.
- `cd <mount> && git status` fails with a documented, recognisable
  error message (we own the wording so it doesn't drift).
- `rg` detects the mount as a VCS root.
- `cargo build` records the OID in build metadata.

## 13. Future evolution path

```
            +----------------+
            | MVP: empty     |
            | RootOverlay    |
            +-------+--------+
                    |
        evidence    |
        triggers    v
            +----------------+
            | R1: B+ default |
            |  + sentinel    |
            +-------+--------+
                    |
        rg/cargo    |
        friction    v
            +----------------+
            | A0 opt-in flag |
            +-------+--------+
                    |
        IDE / git   |
        log demand  v
            +----------------+
            | A1 default     |
            +-------+--------+
                    |
        ref-aware   |
        demand      v
            +----------------+
            | A2 (per-Ref)   |
            +----------------+

            (A3 only when read-write lands)
```

Each step is incremental and gated on real evidence rather than
speculation. The `RootOverlay` mechanism in MVP is what makes every
later step a small additive change rather than an engine refactor.

## 14. Open follow-ups (post-MVP)

- **Sentinel schema review.** Before R1 promotion, do one more design
  pass on `info.json`. Consider whether to include hydration progress,
  mount uptime, or fetcher health.
- **`.projgit/` as a namespace.** If we add more synthetic entries
  (lockfile, status snapshot), they go under `.projgit/` to keep the
  top-level mount surface clean.
- **Tool-specific hints.** Some IDEs / build tools accept env vars or
  config to point at an external git dir. Document these alternatives
  for users who want git integration *without* `--emit-dotgit`.
- **`projgit doctor` integration.** When `--emit-dotgit=minimal` is in
  use, `projgit doctor` should test the round-trip
  (`git rev-parse HEAD == projection commit`).
- **Round-trip on commit-on-write.** When read-write lands, the A0
  `HEAD` we synthesize must reflect post-write state and update
  atomically with commits. Ties into the read-write design doc.
