# Design: WinFsp Backend & Licensing (`projgit-winfsp`)

> Status: **Shipped (`crates/projgit-winfsp`)**. Implements Windows filesystem
> mounting for `projgit mount` and `projgit mount-multi` using self-contained,
> `MIT OR Apache-2.0` WinFsp C ABI bindings dynamically loaded from
> `winfsp-x64.dll` under the **WinFsp FLOSS Exception**.

## 1. Licensing & The WinFsp FLOSS Exception

Third-party Rust wrapper crates such as `winfsp` / `winfsp-sys` (`winfsp-rs`)
are licensed under pure **GPL-3.0** with no linking exception, which would force
any binary linking them to be distributed under GPL-3.0.

By contrast, **WinFsp itself (`winfsp-x64.dll`)** is licensed by Bill
Zissimopoulos under **GPLv3 with the WinFsp FLOSS Exception**
([`License.txt`](https://github.com/winfsp/winfsp/blob/master/License.txt)):

> As a special exception to the terms and conditions of version 3.0 of the GPL,
> the Copyright Holder grants You the right to Distribute Covered Software work
> tied to a Derivative Work that is distributed under an Open Source license,
> without the Derivative Work becoming subject to the terms and conditions of
> the GPL, provided that all of the following criteria are met:
>
> 1. You obey the GPL in all respects for the Covered Software and the custom
>    portions you have written for the Covered Software, except for the
>    identifiable sections of the Derivative Work which are independent and not
>    derived from the Covered Software, and which can reasonably be considered
>    independent and separate works in themselves,
> 2. You distribute all identifiable sections of the Derivative Work under one or
>    more FSF Free Software or OSI Open Source licenses,
> 3. You display the following notice in the Derivative Work's user-interface and
>    user-facing documentation:
>    *"WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos"*,
>    together with a URL/link to the Covered Software's distribution web site,
>    and
> 4. You do not link or distribute the Derivative Work with other software that
>    is not distributed under an FSF Free Software or OSI Open Source license.

Because `projgit-winfsp` uses its own clean-room FFI definitions (`src/ffi.rs`)
for the WinFsp C ABI (`<winfsp/fsctl.h>` / `<winfsp/winfsp.h>`) and dynamically
loads unmodified `winfsp-x64.dll` at runtime:

1. **Workspace license remains `MIT OR Apache-2.0`** across every crate in the
   repository (`Cargo.toml`, `LICENSE-MIT`, `LICENSE-APACHE`).
2. **Attribution notice (`projgit_winfsp::WINFSP_FLOSS_NOTICE`)** is displayed in
   the CLI (`projgit --help` and `projgit mount --help`) and in `README.md`:
   > WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
   > (<https://github.com/winfsp/winfsp>)
3. **Downstream note**: binaries linking `winfsp-x64.dll` under the WinFsp FLOSS
   Exception must remain under an FSF/OSI license and may not be combined with
   closed-source proprietary software without a commercial WinFsp license.

## 2. Root Cause of the Historical Phase 3c Spike Failure

In the earlier Phase 3c spike (`spikes/winfsp-helloworld`), `FspFileSystemCreate`,
`FspFileSystemSetMountPoint`, and `FspFileSystemStartDispatcher` all returned
`STATUS_SUCCESS`, yet `dir Z:\` immediately failed with `Incorrect function`
(`STATUS_INVALID_DEVICE_REQUEST`) without invoking a single Rust callback.

Inspecting WinFsp's `src/dll/fs.c` and `<winfsp/fsctl.h>` revealed two exact root
causes (neither of which required `FspServiceLoop`):

1. **`FspFileSystemCreate` gates `Operations[FspFsctlTransactCreateKind]` on
   four callbacks simultaneously**:
   In `src/dll/fs.c`, `FspFileSystemCreate` only wires up `FspFileSystemOpCreate`
   (which services every `IRP_MJ_CREATE` / file or directory open) if
   `GetSecurityByName`, `Create` (or `CreateEx`), `Open`, **and** `Overwrite`
   (or `OverwriteEx`) are **all non-NULL** in `FSP_FILE_SYSTEM_INTERFACE`. Even
   when `VolumeParams.ReadOnlyVolume = 1`, leaving `Create` and `Overwrite` as
   `None` caused `FspFileSystemCreate` to leave
   `FileSystem->Operations[FspFsctlTransactCreateKind] == NULL`, so the WinFsp
   user-mode dispatcher rejected every `IRP_MJ_CREATE` with
   `STATUS_INVALID_DEVICE_REQUEST` (`Incorrect function`) before calling into
   Rust. Registering read-only `Create` and `Overwrite` callbacks that return
   `STATUS_MEDIA_WRITE_PROTECTED` (`0xC00000A2`) satisfies `FspFileSystemCreate`
   and routes all read opens to `Open`.
2. **`STATUS_OBJECT_NAME_NOT_FOUND` constant value**:
   The spike had defined `STATUS_OBJECT_NAME_NOT_FOUND` as `0xC000000F`
   (`STATUS_NO_SUCH_FILE`) instead of `0xC0000034`
   (`STATUS_OBJECT_NAME_NOT_FOUND`).

## 3. Shipped `crates/projgit-winfsp` Architecture

### 3.1 Audited C ABI & Dynamic Loader (`src/ffi.rs`)

- **Zero build-time SDK / `libclang` requirement**: Instead of running `bindgen`
  or linking against `winfsp-x64.lib` at compile time, `src/ffi.rs` defines the
  exact `#[repr(C)]` WinFsp 2.x structs with compile-time size assertions
  verified against `<winfsp/fsctl.h>` and `<winfsp/winfsp.h>`:
  - `FspFsctlVolumeParams` (`504` bytes)
  - `FspFsctlVolumeInfo` (`88` bytes)
  - `FspFsctlFileInfo` (`72` bytes)
  - `FspFsctlDirInfoHeader` (`104` bytes)
  - `FspFileSystemInterface` (`512` bytes, 64 pointer slots)
  - `FspFileSystem` (`792` bytes, `UserContext` at offset `280`)
- **Runtime DLL discovery (`WinFspApi::load`)**: Resolves `winfsp-x64.dll` via
  `LoadLibraryW` / `GetProcAddress`, checking `PATH`, the WinFsp registry key
  `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir`,
  `%ProgramFiles(x86)%\WinFsp\bin\winfsp-x64.dll`, and
  `%ProgramFiles%\WinFsp\bin\winfsp-x64.dll`.
- **Per-user SDDL `SECURITY_DESCRIPTOR` (`CurrentUserSecurityDescriptor`)**:
  Queries the current process token SID (`GetTokenInformation(..., TokenUser)`)
  and builds a self-relative security descriptor via
  `ConvertStringSecurityDescriptorToSecurityDescriptorW` with SDDL
  `O:<sid>G:<sid>D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;<sid>)(A;;FRFX;;;WD)`.
  Because every projected file and directory reports the mounting user as owner,
  Git for Windows's `safe.directory` ownership check passes out of the box.
- **Existing empty directory mountpoint support (`RawWinFspMount::start`)**:
  WinFsp's `FspFileSystemSetMountPoint` creates a directory reparse point and
  requires the target directory path not to exist yet. To match Linux/macOS FUSE
  ergonomics (where users pass an existing empty directory to `projgit mount`),
  `RawWinFspMount::start` detects an existing empty directory, removes it just
  before `FspFileSystemSetMountPoint`, and restores the empty directory when the
  mount is dropped.

### 3.2 Native Windows Symlink Reparse Points (`src/symlinks.rs`)

- Implements `classify_symlink_target` over any `projgit_core::FsProvider` to
  classify mode-`0120000` git symlink entries as `SymlinkKind::File`,
  `SymlinkKind::Directory`, or `SymlinkKind::DanglingAssumeFile` (handling `..`,
  root-escape bounds, chained symlinks, and symlink loops up to 16 hops).
- Builds and parses standard Windows `IO_REPARSE_TAG_SYMLINK` (`0xA000000C`)
  `REPARSE_DATA_BUFFER` payloads (`build_symlink_reparse_buffer` /
  `parse_symlink_reparse_buffer`) with `SYMLINK_FLAG_RELATIVE` (`0x00000001`)
  and `\`-normalized UTF-16LE `SubstituteName` / `PrintName`.
- Supports both `SymlinkMode::NativeReparse` (default; sets
  `FILE_ATTRIBUTE_DIRECTORY` on directory symlinks so Windows APIs and shells
  traverse both file and directory symlinks without requiring
  `SeCreateSymbolicLinkPrivilege`) and `SymlinkMode::PlainFile`.

### 3.3 `FsProvider` Adapter & Direct L2 Host-File Reads (`src/adapter.rs`)

- `ProjgitWinFsp<P>` implements `WinFspBackend` over any `P: FsProvider`.
- **Direct host-file read fast path (`WinFspReadStats::host_file_reads`)**:
  During `Open`, `ProjgitWinFsp` calls `FsProvider::backing_file(ino)`. When the
  inode is backed by a host file on disk — either an uncompressed L2 cached blob
  (`<git-dir>/blobs/<xx>/<oid>`) or an isolated `.git/objects/pack/*.pack`
  `SyntheticEntry::HostFile` — the open handle stores `Arc<std::fs::File>` and
  serves `Read` directly via `std::os::windows::fs::FileExt::seek_read`
  (thread-safe positioned `ReadFile` with an `OVERLAPPED` offset), bypassing
  `FsProvider::read` and buffer allocation on warm reads.

## 4. Verification (`crates/projgit-winfsp/tests/winfsp_mount.rs`)

Verified on Windows 11 with WinFsp 2.1 (`cargo test -p projgit-winfsp` and
`cargo test -p projgit-cli`):

- `winfsp_dll_loads_and_reports_version` — dynamically loads `winfsp-x64.dll`
  and verifies `FspVersion`.
- `in_memory_mount_serves_files_dirs_and_native_symlinks` — mounts an
  `InMemoryFsProvider`, verifies root/nested file reads, directory listings,
  read-only write rejection, `fs::symlink_metadata` / `fs::read_link`, and
  transparent traversal of both file and directory symlinks.
- `plain_file_symlink_mode_serves_symlink_as_regular_file` — verifies
  `SymlinkMode::PlainFile` fallback.
- `mount_onto_existing_empty_directory_succeeds_and_restores_dir_on_unmount` —
  verifies Linux/macOS-style mounting onto a pre-existing empty directory.
- `real_git_projection_mounts_serves_l2_host_files_and_passes_git_cli` — mounts
  a real `ProjectionFsProvider` + `a1_plus_isolated_overlay`, verifies L2
  uncompressed blob `host_file_reads >= 1`, and runs `git rev-parse HEAD`,
  `git log -n 1 --oneline`, and `git --no-optional-locks status --short` inside
  the mounted WinFsp projection without `safe.directory` overrides.

