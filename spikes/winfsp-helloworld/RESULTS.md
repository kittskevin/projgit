# Phase 3c Spike — Results

> **Outcome: PARTIAL.** The bindgen-driven FFI approach to WinFsp
> compiles and links cleanly, the WinFsp DLL loads via delay-load,
> and our filesystem object successfully establishes a mountpoint
> (verified with `fsptool-x64.exe lsvol`). However, **no IRPs reach
> our user-mode dispatcher** when the mount is probed
> (e.g., `dir Z:\` returns `Incorrect function.`). Phase 3d must
> resolve the dispatch routing issue before the bindings can be
> promoted into the production `projgit-winfsp` crate.

## Spike question

> Can we hand-author Rust FFI bindings to WinFsp (so `projgit-winfsp`
> stays MIT/Apache without depending on the GPL-3.0 `winfsp-rs`
> crate), and use them to mount and serve a single read-only file?

## What worked

1. **Bindgen integration.** Adding `bindgen` as a `[build-dependencies]`
   entry against a single `wrapper.h` that includes `<windows.h>`
   plus `<winfsp/winfsp.h>` produces a `bindings.rs` containing every
   WinFsp type, function pointer, and constant we need. Roughly 5300
   lines of generated FFI; builds in ~2s after the first run.
2. **Allowlisting.** `allowlist_function("Fsp.*")` /
   `allowlist_type("FSP_.*")` keeps the surface focused on WinFsp
   itself and skips the (huge) Win32 surface bindgen would otherwise
   pull in.
3. **`<windows.h>` + `PNTSTATUS` typedef.** Bindgen-against-WinFsp
   needs `<windows.h>` included first. `PNTSTATUS` is missing from
   plain `<windows.h>` (it lives in `<ntstatus.h>` / `<bcrypt.h>`).
   We work around this with a minimal manual typedef in `wrapper.h`:
   ```c
   typedef LONG NTSTATUS;
   typedef NTSTATUS *PNTSTATUS;
   ```
4. **Linker / delay-load.** Three `cargo:rustc-link-arg` lines in
   `build.rs` are enough:
   - `cargo:rustc-link-search=native=C:\Program Files (x86)\WinFsp\lib`
   - `cargo:rustc-link-lib=dylib=winfsp-x64`
   - `cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll`
   - `cargo:rustc-link-arg=delayimp.lib`
5. **Type-safe callbacks.** Bindgen renders WinFsp's typedefs
   (`PWSTR = *mut WCHAR`, `NTSTATUS = LONG`, `SIZE_T = ULONG_PTR`,
   etc.) as distinct Rust type aliases. Writing callbacks with the
   `ffi::` typedefs (instead of plain `*mut u16` / `i32`) lets the
   compiler coerce them to the bindgen-generated function-pointer
   types in `FSP_FILE_SYSTEM_INTERFACE`.
6. **Bitfield setters.** Bindgen generates `set_<Name>(val: UINT32)`
   methods for each bitfield in `FSP_FSCTL_VOLUME_PARAMS`. They
   compile correctly and produce the expected layout.
7. **Mount lifecycle.** All three init calls return `STATUS_SUCCESS`:
   - `FspFileSystemCreate`
   - `FspFileSystemSetMountPoint("Z:")`
   - `FspFileSystemStartDispatcher(fs, 0)`
8. **`fsptool lsvol` confirms the mount.** While the spike runs,
   `fsptool lsvol` reports `Z:  \Device\Volume{...}` — i.e., WinFsp's
   FSD has accepted our volume and bound it to the requested drive
   letter.

## What did not work

`dir Z:\` (and `dir \\?\Z:\` and `dir \\.\Z:\`) all return
`Incorrect function.` (`ERROR_INVALID_FUNCTION = 0x1`,
`STATUS_INVALID_DEVICE_REQUEST = 0xC0000010` at the kernel level).

A file-based trace inside every callback (file `trace.log`, written
synchronously and flushed) confirms that **none of `GetVolumeInfo`,
`GetSecurityByName`, `Open`, `GetFileInfo`, or `ReadDirectory` are
invoked** when the volume is probed. The IRP is rejected somewhere
between the kernel FSD and our user-mode dispatcher.

## Hypotheses (unverified)

In rough order of likelihood:

1. **Service framework requirement.** WinFsp's bundled samples
   (memfs, passthrough) all use `FspServiceCreate` /
   `FspServiceLoop` / `FspServiceStop`, not the bare
   `FspFileSystemStartDispatcher` lifecycle that the WinFsp wiki
   documents as "the simple option." It's possible (likely) that
   the bare path requires additional plumbing the wiki glosses
   over -- e.g., explicit operation guards, a service control
   handler, or a manifest that registers the binary as a WinFsp
   service.
2. **Mount-mode flag.** `FspFileSystemSetMountPoint` may need to
   be replaced by `FspFileSystemSetMountPointEx` with a security
   descriptor argument. The default `SetMountPoint` may be limited
   to certain accessor sessions / privileges in a way that breaks
   `dir`.
3. **Subtle bitfield miscompile.** Bindgen's bitfield codegen on
   MSVC structs is historically fragile. The volume params struct
   has two bitfield blocks (`_bitfield_1` and `_bitfield_2`) totalling
   8 bytes. If bindgen got the bit positions slightly wrong, we
   could be telling the FSD "I do not support unicode on disk" or
   similar, and the FSD would reject all IRPs.
4. **Calling convention.** All callbacks declare `extern "C"`. On
   x64 Windows that resolves to the single Microsoft x64 ABI, so
   it should be correct. But worth ruling out by hex-comparing
   the C memfs callback prologues with our Rust ones.
5. **DevicePath value.** I pass `"WinFsp.Disk"` (matches
   `FSP_FSCTL_DISK_DEVICE_NAME`). Possibly needs to be a wide-string
   literal computed by the macro, not a Rust-side string -- but
   matching by value should be equivalent.

## What this means for Phase 3d

The bindgen approach **stays** -- generation is correct, build
toolchain is solid, type translation works. What changes:

1. **Switch to the `FspService*` lifecycle.** Read the memfs source
   carefully and replicate it in Rust. The samples are the source
   of truth, not the wiki.
2. **Verify struct layouts byte-by-byte before debugging logic.**
   Add a startup check that asserts
   `sizeof(FSP_FSCTL_VOLUME_PARAMS) == <expected from C build>` and
   probes a few critical bitfield positions by setting one bit at a
   time and reading back the underlying `_bitfield_*` bytes. If
   anything differs from C, that's our bug.
3. **Plan for one round of upstream-binding study.** Even though we
   don't *link* against `winfsp-rs`, reading its startup code (under
   GPL-3.0; reading is fine, copying isn't) is fast intel on
   "what does it actually take to get a working WinFsp dispatcher in
   Rust."
4. **Do this work in `projgit-winfsp`, not under `spikes/`.** The
   spike has served its diagnostic purpose. Phase 3d's first commit
   should be a fresh scaffold of `projgit-winfsp` with a working
   mount, then we layer on the projection-backed `FsProvider` and
   the symlink classifier.

## Reproducibility

```powershell
# 1. WinFsp installed via `winget install WinFsp.WinFsp` and then
#    re-installed with ADDLOCAL=ALL via msiexec for the developer
#    feature (provides headers, lib, samples).
# 2. LLVM installed via `winget install LLVM.LLVM` (provides libclang).
# 3. WinFsp DLL must be on PATH at run time:
$env:PATH = 'C:\Program Files (x86)\WinFsp\bin;' + $env:PATH
# 4. bindgen needs to find libclang:
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'

cd spikes/winfsp-helloworld
cargo build
.\target\debug\spike-winfsp-helloworld.exe Z:
# in another shell:
& 'C:\Program Files (x86)\WinFsp\bin\fsptool-x64.exe' lsvol
# expected: Z:  \Device\Volume{...}
& cmd /c 'dir Z:\'
# expected (someday): one entry, hello.txt, 43 bytes
# actual today:       "Incorrect function."
```

## Date

Run on 2026-05-08.
