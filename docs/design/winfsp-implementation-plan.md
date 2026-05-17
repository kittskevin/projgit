# Design: WinFsp Implementation Plan

> Status: **deferred implementation plan**. This document replaces the tracked
> WinFsp spike crates. It preserves the useful findings from the Phase 0c and
> Phase 3c Windows work without keeping unfinished prototype code in the public
> repo surface.

## 1. Current Decision

Windows mounting is deferred. The working product surface is currently the
Linux/macOS FUSE backend. The `projgit-winfsp` crate remains a documented stub
so the workspace shape still reflects the intended backend split, but it should
not be advertised as functional.

When Windows work resumes, implement it in `crates/projgit-winfsp/`, not in a
new spike crate.

## 2. What The Removed Spikes Proved

### 2.1 Reparse-Point Behavior Works

The Phase 0c spike used WinFsp's bundled `memfs-x64.exe` sample to validate
kernel and tool behavior for symlinks. It did not ship Rust code.

Confirmed on Windows 11 with WinFsp 2.1.25156:

- WinFsp-served `IO_REPARSE_TAG_SYMLINK` entries are traversed by `cmd.exe`,
  PowerShell, Python, and MSVC without the reading user holding
  `SeCreateSymbolicLinkPrivilege`.
- File and directory symlink kind is honored end to end.
- Relative target strings round-trip verbatim through `os.readlink`.
- Git stores those links as mode `120000` with the target string as blob
  content.

Important follow-up:

- Modern Git for Windows enforces `safe.directory`. A projgit WinFsp backend
  must report the calling user as owner in `get_security_by_name` and
  `get_security`, not `BUILTIN\\Administrators`.

This confirms the default policy in [windows-symlinks.md](windows-symlinks.md):
`Auto` should prefer native reparse points and fall back to text-marker mode only
when native support is unavailable.

### 2.2 Hand-Rolled FFI Is Viable But Dispatch Is Blocked

The Phase 3c hello-world spike asked whether projgit could bind WinFsp directly
with `bindgen` and avoid the GPL-3.0 `winfsp-rs` crate. The answer was partial.

Worked:

- `bindgen` against a narrow `wrapper.h` containing `<windows.h>` and
  `<winfsp/winfsp.h>`.
- Allowlisted WinFsp symbols via `allowlist_function("Fsp.*")` and
  `allowlist_type("FSP_.*")`.
- Manual `PNTSTATUS` typedef in `wrapper.h`.
- Link and delay-load against `winfsp-x64.dll`.
- Bindgen bitfield setters for `FSP_FSCTL_VOLUME_PARAMS`.
- `FspFileSystemCreate`, `FspFileSystemSetMountPoint`, and
  `FspFileSystemStartDispatcher` all returned success.
- `fsptool-x64.exe lsvol` showed the drive mounted.

Did not work:

- `dir Z:\` returned `Incorrect function`.
- File-based tracing showed no callbacks were invoked.
- The IRP was rejected before reaching the user-mode dispatcher.

Most likely blocker:

- The production backend should use the `FspServiceCreate` /
  `FspServiceLoop` / `FspServiceStop` lifecycle used by WinFsp's C samples
  instead of the bare `FspFileSystemStartDispatcher` path.

## 3. FFI Recipe To Recreate

### 3.1 Tooling

Windows host requirements:

```powershell
winget install WinFsp.WinFsp
winget install LLVM.LLVM
```

Install WinFsp developer features with `ADDLOCAL=ALL` so headers, import libs,
and samples are present.

Runtime environment:

```powershell
$env:PATH = 'C:\Program Files (x86)\WinFsp\bin;' + $env:PATH
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
```

### 3.2 wrapper.h Shape

```c
#include <windows.h>

typedef LONG NTSTATUS;
typedef NTSTATUS *PNTSTATUS;

#include <winfsp/winfsp.h>
```

### 3.3 build.rs Shape

The production backend should generate bindings in `build.rs` with a narrow
allowlist:

```rust
bindgen::Builder::default()
    .header("wrapper.h")
    .allowlist_function("Fsp.*")
    .allowlist_type("FSP_.*")
    .allowlist_var("FSP_.*")
    .generate()?;
```

Linker configuration:

```rust
println!("cargo:rustc-link-search=native=C:\\Program Files (x86)\\WinFsp\\lib");
println!("cargo:rustc-link-lib=dylib=winfsp-x64");
println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
println!("cargo:rustc-link-arg=delayimp.lib");
```

Before debugging filesystem behavior, add startup assertions for struct sizes
and bitfield positions. A tiny C helper that prints `sizeof` and byte offsets is
worth the time.

## 4. Implementation Track

### 4.1 Unblock The Hello-World Mount

Goal: `dir Z:\` invokes Rust callbacks and shows one read-only file.

Tasks:

1. Read WinFsp's C `memfs` and `passthrough` samples. Treat them as the source
   of truth for service startup and callback registration.
2. Port the `FspService*` lifecycle into `crates/projgit-winfsp`.
3. Keep the filesystem trivial: root directory plus `hello.txt`.
4. Add file-based tracing before every callback return.
5. Validate with `fsptool-x64.exe lsvol`, `cmd /c dir Z:\`, and
   `type Z:\hello.txt`.

Exit criteria:

- The drive mounts.
- Directory enumeration reaches Rust callback code.
- Reading one file works.
- Dropping or stopping the service unmounts cleanly.

### 4.2 Adapt `FsProvider`

Once hello-world dispatch works, replace the hard-coded tree with an adapter
over `projgit_core::FsProvider`.

Operations to map first:

- `GetVolumeInfo`
- `GetSecurityByName`
- `Open`
- `GetFileInfo`
- `ReadDirectory`
- `Read`
- `GetReparsePointByName` / readlink equivalent

Error mapping should mirror `projgit-fuse`: `NotFound` to not-found status,
`NotADirectory` to not-directory status, and unexpected I/O to a generic I/O
failure.

### 4.3 Implement Symlinks

Use the classifier in [windows-symlinks.md](windows-symlinks.md):

```rust
enum SymlinkKind {
    File,
    Directory,
    DanglingAssumeFile,
}
```

Classifier inputs:

- Commit OID.
- Link path.
- Raw target string from the mode-`120000` blob.

Rules:

- Relative in-tree target resolving to a tree -> directory symlink.
- Relative in-tree target resolving to a blob -> file symlink.
- Cycles, escapes, missing targets, or unsupported paths -> file symlink plus a
  diagnostic.
- Cache by `(commit_oid, link_path, target_string)`.

### 4.4 Ownership And Git Compatibility

Before calling Windows support usable, implement per-user security descriptors.
Git for Windows should not report `fatal: detected dubious ownership in
repository` when run inside a future synthesized `.git/` mount.

Validation:

```powershell
git -C Z:\ rev-parse --show-toplevel
git -C Z:\ status
```

The exact commands may evolve once `.git/` synthesis lands.

## 5. Test Matrix

Minimum manual matrix for Phase 3d:

```text
cmd.exe:      dir, type, cd into directory symlink
PowerShell:  Get-Item .LinkType, Get-Content
Python:      os.path.islink, os.readlink, open, os.listdir
MSVC:        cl.exe compiling through a symlinked header
Git:         safe.directory behavior once .git synthesis exists
```

Automated tests should start with pure Rust unit tests for path classification
and callback error mapping. Full WinFsp integration tests can be Windows-only and
ignored by default until CI has the required driver installed.

## 6. Resume Checklist

1. Recreate the FFI binding scaffold in `crates/projgit-winfsp`.
2. Port WinFsp sample service lifecycle, not the bare dispatcher path.
3. Prove hello-world callbacks receive IRPs.
4. Add struct-layout assertions.
5. Implement the `FsProvider` adapter.
6. Implement symlink classifier and reparse-point output.
7. Implement per-user ownership.
8. Wire `projgit mount` on Windows.
9. Update [../handoff.md](../implementation/handoff.md) and [../../README.md](../../README.md)
   only after the backend works end to end.
