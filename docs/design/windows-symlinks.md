# Design: Windows Symlinks in projgit

> Status: **decided**. Companion to [../initial-plan.md](../implementation/initial-plan.md)
> §9.1. Captures the problem space, the option matrix considered, the
> decisions taken, and the implementation contract for the Windows FS
> backend.

## 1. Problem statement

Git stores symlinks as blobs with mode `120000` whose content is the link
target string (e.g. `../include/foo.h`). The POSIX kernel resolves these
lazily and never asks the filesystem to classify the target.

Windows has no equivalent of POSIX symlinks. Its closest mechanisms each
carry constraints that bite a virtual filesystem:

| Mechanism | Privilege | Cross-volume | Target kind |
|---|---|---|---|
| NTFS symbolic link (`IO_REPARSE_TAG_SYMLINK`) | `SeCreateSymbolicLinkPrivilege` **or** Developer Mode | Yes | File **or** dir, **declared at creation** |
| Directory junction (`IO_REPARSE_TAG_MOUNT_POINT`) | None | No | Directory only, absolute path only |
| Hard link | None | No | File only |
| App-Exec link | n/a | n/a | n/a |

Two specific properties of the Windows model force design choices on us:

1. **Eager file-vs-directory classification.** When we create / serve a
   symlink, we must declare whether the target is a file or a directory.
   POSIX never asks.
2. **Privilege-gated creation via the normal API.** Standard users without
   Developer Mode cannot call `CreateSymbolicLinkW` successfully.

## 2. Why this is harder in a VFS than in a checkout

Stock Git on Windows resolves the symlink question once, at checkout time,
via `core.symlinks`. We answer it live, on every directory enumeration,
inside the FS event loop. Two consequences:

### 2.1 Out-of-tree targets

A symlink target may resolve to a path **outside** the projected git tree:

```
src/stdio  -> /usr/include/stdio.h         (absolute, system path)
src/shared -> ../../shared-lib/include     (escapes the repo)
```

For in-tree targets we walk git tree objects to read the target's mode.
For out-of-tree targets the target is not in our object store at all, so
we cannot authoritatively classify it. See discussion of options in §4.2.

### 2.2 WinFsp serves reparse points without user privilege

`SeCreateSymbolicLinkPrivilege` gates the **public** API
(`CreateSymbolicLinkW`). A WinFsp filesystem implements
`GetReparsePointByName` / `GetReparsePoint` and synthesizes a
`REPARSE_DATA_BUFFER` directly. The kernel honors the result without
checking the calling user's privileges, because the filesystem itself
declared the entry. This is the keystone fact that makes `Native` mode
viable as a default.

ProjFS is more restrictive (placeholders must declare the reparse tag at
creation time) and is deferred per the main plan.

## 3. Goals & non-goals

### Goals

- Symlinks in mounted projections **just work** for the common cases
  (`cd link`, `type link`, MSVC `#include "../foo.h"`, Python
  `os.readlink`, `git status` inside the mount).
- No requirement for end users to enable Developer Mode or run as admin.
- Graceful degradation when the optimal path is unavailable.
- Behavior is **observable and overridable** \u2014 users can see what we
  did and force a different policy per-mount.
- Cacheability: symlink classification for a given commit is immutable,
  so we cache it.

### Non-goals (MVP)

- Junction-based hybrid (Option 2 in §4.1). Revisit only if `Native`
  turns out to be blocked on some Windows builds.
- Honoring `core.symlinks=false` from the projected repo's own config
  (the projection layer doesn't read repo config).
- Cross-mount symlinks (a link in mount A pointing into mount B). Treated
  as out-of-tree.
- Round-tripping `Text`-mode files back to git symlinks (read-only MVP).

## 4. Options considered

### 4.1 Storage / presentation strategy

| # | Strategy | Verdict |
|---|---|---|
| 1 | **Native NTFS symlinks via WinFsp reparse-point API.** Synthesize `IO_REPARSE_TAG_SYMLINK` on enumeration and `GetReparsePointByName`. | **Chosen as default.** Most transparent. No end-user privilege needed because the filesystem creates the reparse data. |
| 2 | **Directory junctions for in-tree directory targets, file-symlinks otherwise.** Hybrid using `IO_REPARSE_TAG_MOUNT_POINT`. | Rejected for MVP. Junctions are absolute-path only; bakes the mountpoint into link content and breaks if the mount moves. |
| 3 | **Text-file fallback with marker.** Serve a regular file whose content is the link target string; mark via NTFS Alternate Data Stream `:projgit.symlink`. Mirrors `core.symlinks=false`. | **Chosen as fallback** for `Text` mode and as the degradation target of `Auto`. |
| 4 | **Refuse / hide.** Return `ENOENT` or skip the entry. | Rejected. Surprising and breaks `readdir` invariants. |
| 5 | **WSL passthrough.** Tell users to mount inside WSL2 and access via `\\\\wsl$\\...`. | Documented as a deployment recommendation, not a product feature. |

### 4.2 Out-of-tree target classification

Three sub-options when a symlink points outside the git tree:

| # | Strategy | Verdict |
|---|---|---|
| a | **Walk the host filesystem** to see what the target is. | Rejected. Racy, slow, leaks host state into the projection, and may give different answers on different machines. |
| b | **Default to file-symlink** (matches stock Git's behavior for dangling links) and log a warning. | **Chosen.** Resolves correctly at access time *if* the target later exists; failure mode is identical to a normal POSIX dangling link. |
| c | **Emit as `Text` and warn.** | Rejected as default. Builds that depend on the symlink fail silently in confusing ways. Available indirectly via `--symlinks=text`. |

### 4.3 `Text`-mode marker mechanism

| # | Mechanism | Verdict |
|---|---|---|
| i | **NTFS Alternate Data Stream `:projgit.symlink`.** | **Chosen.** Discoverable via `dir /R`. Intentionally does *not* survive a copy to FAT32/exFAT or off the projgit mount — if the file leaves us, the marker should disappear so it's treated as a plain text file. |
| ii | Extended attribute via WinFsp xattr support. | Rejected. Less discoverable; survival semantics are subtler. |
| iii | Sentinel filename suffix (e.g. `.projgit-symlink`). | Rejected. Changes the visible filename; breaks paths inside the repo. |

## 5. Decisions

The three sub-decisions from §4 lock in as follows:

1. **Default mode = `Auto`.** WinFsp's filesystem-side reparse-point
   creation makes `Native` work for end users without
   `SeCreateSymbolicLinkPrivilege`, so `Auto` tries `Native` first and
   degrades to `Text` only when WinFsp reports that reparse data is
   unavailable for the mount.
2. **Out-of-tree targets emit a file-symlink and log a warning.** Matches
   POSIX dangling-link semantics and what stock git does. Captured by
   `tracing` for `projgit doctor` to surface.
3. **`Text`-mode marker = NTFS Alternate Data Stream** named
   `:projgit.symlink`. Marker disappears when the file is copied off the
   mount, which is the intended semantics.

## 6. Per-mount policy

```
projgit mount <store> <projection-spec> <mountpoint>
       [--symlinks={native|text|auto}]   # default: auto
```

- `native` \u2014 always emit reparse points. **Fail the mount** if WinFsp
  reports reparse data unsupported.
- `text` \u2014 always emit text files with the ADS marker. Most compatible
  with hostile environments (some EDR / antivirus tools strip reparse
  points).
- `auto` (default) \u2014 try `native`, fall back to `text` if WinFsp
  reports the reparse path unavailable. The fallback decision is logged
  once per mount, not per file.

`auto` does **not** flip per-file. Mixing `Native` and `Text` symlinks in
the same mount would produce baffling tooling behavior (`readlink` works
on some, returns text content on others).

## 7. Algorithm

### 7.1 Mount initialization

```
on mount:
  policy := parse --symlinks (default Auto)
  if policy in {Native, Auto}:
      probe WinFsp reparse-point capability
      if unsupported:
          if policy == Native: fail mount with diagnostic
          if policy == Auto:   policy := Text; log warning
  store policy on the Mount
```

### 7.2 Per-symlink classification (Native mode)

Triggered the first time a symlink entry is enumerated for a given
projection.

```
classify(commit, link_path, target_str) -> SymlinkKind:
    cache_key = (commit, link_path)
    if cache.contains(cache_key): return cache[cache_key]

    resolved = resolve_relative(target_str, parent_of(link_path))
    if resolved escapes the projection root:
        log_warning("out-of-tree symlink", link_path, target_str)
        kind := File          # default per Decision (2)
    else:
        entry = TreeNavigator.lookup(commit, resolved)
        match entry:
            None              -> log_warning("dangling in-tree"); kind := File
            File              -> kind := File
            Directory         -> kind := Directory
            Symlink (chained) -> recurse classify (capped at N=8); on
                                 cycle/over-limit -> kind := File + warn

    cache[cache_key] := kind
    return kind
```

The cache is keyed by `(commit_oid, link_path)` and lives in the
projection's in-memory state. Trees are immutable per commit, so cache
entries never need invalidation within a projection's lifetime.

### 7.3 Per-symlink emission

```
on enumerate / GetReparsePointByName:
  if policy == Text:
      emit regular file:
          size    = len(target_str)
          content = target_str
          attach NTFS ADS ":projgit.symlink" containing a small JSON header
              { "version": 1, "target": "<target_str>" }
  if policy == Native:
      kind = classify(commit, link_path, target_str)
      emit reparse point:
          tag   = IO_REPARSE_TAG_SYMLINK
          flags = SYMLINK_FLAG_RELATIVE if target is relative else 0
          if kind == Directory: set SYMLINK_FLAG_DIRECTORY equivalent
          target = target_str (verbatim, no path translation)
```

Notes:

- We **do not rewrite** the target string. A symlink to `../foo` stays
  `../foo`; the kernel resolves it inside the mount, where `../foo` is
  meaningful.
- `SymlinkKind::File` is the fallback for anything ambiguous \u2014 dangling,
  out-of-tree, classification failure, cycle. Matches stock Git.

### 7.4 Diagnostic surface

- `tracing` events at `WARN` level for each guessed classification, with
  fields `{projection, commit, link_path, target_str, reason}`.
- `projgit doctor` aggregates these into a per-mount report:
  `"<mount>: 47 symlinks total; 12 classified as File by fallback (out-of-tree); 0 dangling in-tree"`.
- A `--symlink-report` flag on `projgit mount` to dump the classification
  table for offline review.

## 8. Test plan

Tests live alongside Phase 0c (spike) and Phase 3 (frontend) work.

### 8.1 Unit (cross-platform)

- Classifier with a synthetic in-memory tree:
  - in-tree -> file
  - in-tree -> dir
  - in-tree -> symlink chain (resolved)
  - in-tree -> symlink cycle (capped, falls back to File)
  - out-of-tree absolute (`/usr/include/foo.h`) -> File + warning
  - out-of-tree relative escape (`../../x`) -> File + warning
  - dangling in-tree -> File + warning

### 8.2 Integration (Windows only, feature-gated in CI)

A fixture repo `tests/fixtures/symlink-zoo/` containing one of every
case above. For each policy:

- `--symlinks=native` (when WinFsp supports reparse data):
  - `cmd.exe`: `cd link-to-dir`, `type link-to-file`
  - PowerShell: `(Get-Item link-to-file).Target` returns the target
    string
  - Python: `os.readlink(link)` returns the target string
  - MSVC: `cl.exe /I . /c uses_link.c` compiles when the symlink points
    to a header
- `--symlinks=text`:
  - `Get-Content link-to-file` returns the target string
  - ADS check: `Get-Item -Stream projgit.symlink link-to-file` exists with
    a parseable JSON header
- `--symlinks=auto`:
  - Same as `native` on a capable system; logs a fallback message and
    behaves like `text` when the reparse path is disabled (simulated by
    a feature flag in the WinFsp backend)

### 8.3 Phase 0c validation

The Phase 0c validation used WinFsp's bundled `memfs-x64.exe` sample to expose
a symlink reparse point. Results are preserved in
[winfsp-implementation-plan.md](winfsp-implementation-plan.md). Pass criteria:

1. `cmd.exe /c dir` shows the entry as `<SYMLINK>` or `<SYMLINKD>`.
2. `cmd.exe /c type link.txt` reads through to the target.
3. `python -c "import os; print(os.readlink('link.txt'))"` prints the
   target string verbatim.
4. `cl.exe /Zs link.h` (or equivalent) treats the link as a header.

If (1)\u2013(4) all pass, `Native` mode is viable as the default.

## 9. Open follow-ups (post-MVP)

- **Junction-based hybrid (Option 2).** Worth revisiting only if a real
  Windows configuration is found where reparse-point synthesis fails but
  junctions still work.
- **Per-path policy via `.projgitattributes`.** A repo-level escape hatch
  letting projects pin behavior for tricky paths
  (`docs/legacy-symlinks/* projgit-symlink=text`). Out of MVP because the
  attribute file would have to be loaded from inside the projection
  itself \u2014 a layering wrinkle.
- **ProjFS port.** When ProjFS replaces WinFsp on Windows, revisit
  whether placeholders' reparse-tag declaration changes the algorithm
  in §7.
- **Round-trip support.** When read-write lands, decide whether `Text`
  files with the ADS marker should be re-encoded as mode-`120000` blobs
  on commit-on-write.
