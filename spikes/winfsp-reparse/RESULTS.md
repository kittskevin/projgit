# Phase 0c Spike — Results

> **Outcome: WinFsp reparse-point synthesis WORKS for all consumer
> tools we tested.** The Windows-symlink design committed in
> [docs/design/windows-symlinks.md](../../docs/design/windows-symlinks.md)
> (default `Auto` mode = `Native` reparse points via WinFsp) is viable.
> Two important secondary findings — Developer Mode is required for
> `mklink` from non-admin shells, and modern git enforces volume
> ownership via `safe.directory` — must be addressed in Phase 3.

## Spike question

> Does a WinFsp filesystem that serves a `IO_REPARSE_TAG_SYMLINK`
> reparse point cause Windows consumer tools (cmd.exe, PowerShell,
> Python `os.readlink`, `cl.exe`) to traverse it transparently \u2014
> *without* the calling user holding `SeCreateSymbolicLinkPrivilege`?

(See [docs/initial-plan.md](../../docs/initial-plan.md) Phase 0c.)

## Approach

This spike does **not** ship Rust code. The original plan was to build
a minimal WinFsp filesystem in Rust against the `winfsp` crate; that
crate is **GPL-3.0**, which would force a project-wide license decision
prematurely (see [/memories/repo/projgit.md](/memories/repo/projgit.md)
\u201cOpen architectural TODOs\u201d). Instead we used WinFsp's bundled
`memfs-x64.exe` C++ sample (ships in
`C:\Program Files (x86)\WinFsp\bin\` after installing the developer
feature). Memfs uses the same WinFsp reparse-point APIs we would call
from any Rust binding, so consumer-tool behavior against memfs answers
the same kernel-level question.

## Setup

- **Toolchain:** WinFsp 2.1.25156 (installed via
  `winget install WinFsp.WinFsp`, then re-installed with `ADDLOCAL=ALL`
  via `msiexec` to get the developer features and `memfs-x64.exe`).
- **OS:** Windows 11 with Developer Mode enabled
  (`HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock\AllowDevelopmentWithoutDevLicense = 1`).
- **MSVC:** Visual Studio 2022 Community, `cl.exe` via vcvars64.bat.
- **Python:** Windows Store Python (resolved via `py.exe`).
- **Git:** Git for Windows (modern, with `safe.directory` enforcement).

## Procedure

1. Mount memfs at `M:`:
   ```text
   memfs-x64.exe -i -n 65536 -s 16777216 -m M:
   ```
2. Populate with two real entries and three symlinks:
   ```text
   real-target.txt           (file, 62 bytes)
   real-subdir\inside.txt    (file, 23 bytes)
   real-header.h             (file, C header)
   link-to-file -> real-target.txt    (mklink, file-symlink, relative)
   link-to-dir  -> real-subdir        (mklink /D, dir-symlink, relative)
   link-header.h -> real-header.h     (mklink, file-symlink, relative)
   uses_link.c                        (file, #includes "link-header.h")
   ```
   `mklink` succeeds **without admin** because Developer Mode is on.
3. Run the verification matrix as a non-admin user.

## Verification matrix

### \u2705 cmd.exe

`cmd /c dir M:\` shows:
```
05/08/2026  01:14 AM    <SYMLINKD>     link-to-dir [real-subdir]
05/08/2026  01:14 AM    <SYMLINK>      link-to-file [real-target.txt]
05/08/2026  01:14 AM    <DIR>          real-subdir
05/08/2026  01:14 AM                62 real-target.txt
```
- `<SYMLINK>` and `<SYMLINKD>` markers appear correctly.
- `cmd /c "type link-to-file"` prints the target's content.
- `cmd /c "cd link-to-dir && dir"` traverses into the symlinked
  directory and lists the real contents.

### \u2705 PowerShell

```text
PS> (Get-Item link-to-file).LinkType   # SymbolicLink
PS> (Get-Item link-to-file).Target     # real-target.txt
PS> (Get-Item link-to-dir ).LinkType   # SymbolicLink
PS> (Get-Item link-to-dir ).Target     # real-subdir
PS> Get-Content link-to-file           # ...target's content...
```
All four properties report correctly. `Get-Content` traverses
transparently.

### \u2705 Python

```python
import os
os.path.islink("link-to-file")   # True
os.path.islink("link-to-dir")    # True
os.path.isfile("link-to-file")   # True   (kind honored: file)
os.path.isdir("link-to-dir")     # True   (kind honored: directory)
os.readlink("link-to-file")      # 'real-target.txt'
os.readlink("link-to-dir")       # 'real-subdir'
open("link-to-file").read()      # ...target's content...
os.listdir("link-to-dir")        # ['inside.txt']
```
- `os.path.islink` works on both kinds.
- `os.readlink` returns the **exact relative target string** \u2014 no
  path rewriting, no surprises.
- `os.path.isfile`/`isdir` correctly distinguish file-symlinks from
  dir-symlinks, proving the **file/dir kind in the reparse data is
  honored end-to-end** (the projection-engine classifier in our design
  must produce this kind correctly).

### \u2705 cl.exe (MSVC)

```text
cmd> cl /nologo /Fe:uses_link.exe uses_link.c && uses_link.exe
uses_link.c
12345 compiled via reparse-point header
```
`#include "link-header.h"` resolved through the symlink to
`real-header.h`, compiled, linked, and executed correctly.

### \u2705 git (bonus test)

After working around the `safe.directory` issue (see Findings):
```text
PS> git ls-files -s
120000 4570b42... 0    link-header.h
120000 b62402a... 0    link-to-dir
120000 1184c3f... 0    link-to-file
100644 293e718... 0    real-header.h
100644 30e3cf3... 0    real-target.txt
...
```
**Git correctly stores all three symlinks as mode `120000`** with the
target string as the blob content. This is exactly the round-trip
behavior projgit needs to render git mode-`120000` blobs back to
Windows.

## Findings

### Confirmed (the architectural assumption holds)

1. **Reparse-point synthesis works without `SeCreateSymbolicLinkPrivilege`
   for the reading user.** All four consumer tools traversed
   memfs-served symlinks as a non-admin user. The privilege only
   gates *creation* via `CreateSymbolicLinkW`; once the FS hands the
   kernel reparse data, the kernel honors it for any reader.
2. **The file/dir kind in the reparse data is consistently honored.**
   Tools correctly distinguish `IO_REPARSE_TAG_SYMLINK` with the
   "directory" flag from the "file" form. This validates the per-
   symlink classifier in
   [windows-symlinks.md](../../docs/design/windows-symlinks.md) \u00a77.2.
3. **Relative target strings round-trip verbatim.** Python's
   `os.readlink` returns the exact string we'd have stored in a git
   `120000` blob. No path rewriting needed in the FS layer.

### New facts the design must absorb

4. **Developer Mode (or admin) is required to *create* symlinks via
   the public API**, even on a WinFsp filesystem. This is irrelevant
   for `projgit` itself (we are read-only and never call
   `CreateSymbolicLinkW`), but matters if a user copies files **into**
   a hypothetical writable mount in the future. Document for read-
   write design (post-MVP).
5. **Modern git enforces volume ownership via `safe.directory`.**
   Memfs reports root ownership as `BUILTIN\Administrators`, which
   triggers `fatal: detected dubious ownership in repository`. Two
   implications for `projgit`:
   a. **Phase 3 must synthesize per-user ownership** in the security
      descriptor returned by `get_security_by_name` so the calling
      user owns the projection. Otherwise every user will hit this
      error on first `git status` inside a mount.
   b. **Cross-user mounts** (one user's daemon serving a mount that
      another user reads) become more complex. Defer to a future
      multi-user design.
6. **`mklink` and `New-Item -ItemType SymbolicLink` use different
   privilege paths.** `mklink` (cmd) honors
   `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE` and works under
   Developer Mode; PowerShell's `New-Item` does not (in the version
   tested) and demands admin. Worth documenting for users who try to
   create symlinks inside any future writable mount.

### Implications for the plan

1. **No change to the Phase 3 symlink design.** `Native` mode default
   stands. Algorithm in
   [windows-symlinks.md](../../docs/design/windows-symlinks.md) \u00a77 is
   verified at the kernel boundary.
2. **Add a Phase 3 sub-task: per-user volume / file ownership.**
   `get_security_by_name` and `get_security` must report ownership =
   the calling-user SID, not Administrators. Without this, `git`
   inside a mount fails on first run.
3. **Reaffirm the WinFsp-binding license TODO.** This spike used the
   GPL-3.0 `winfsp-rs` crate **only via the C++ memfs sample** \u2014 no
   Rust code that links against `winfsp-rs` was written or shipped.
   The production decision (write our own FFI bindings vs. plugin
   architecture vs. accept GPL-3.0) is still open. See
   [/memories/repo/projgit.md](/memories/repo/projgit.md).

## Reproducibility

This spike has no committed code. To re-run:

```powershell
# 1. Install WinFsp with developer features
winget install WinFsp.WinFsp
$msi = "$env:TEMP\winfsp.msi"
Invoke-WebRequest 'https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi' -OutFile $msi
Start-Process msiexec.exe -ArgumentList "/i `"$msi`" ADDLOCAL=ALL /qn /norestart" -Wait -Verb RunAs

# 2. Mount memfs
Start-Process 'C:\Program Files (x86)\WinFsp\bin\memfs-x64.exe' -ArgumentList '-i -n 65536 -s 16777216 -m M:' -WindowStyle Hidden

# 3. Populate (Developer Mode required for unprivileged mklink)
cd M:\
"target content" | Set-Content real-target.txt -NoNewline
cmd /c "mklink link-to-file real-target.txt"

# 4. Test
cmd /c "type link-to-file"
py -c "import os; print(os.readlink('link-to-file'))"

# 5. Unmount
Stop-Process -Name memfs-x64 -Force
```

## Date

Run on 2026-05-08.
