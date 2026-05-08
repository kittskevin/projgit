//! Phase 3c spike — hello-world WinFsp filesystem via bindgen-generated FFI.
//!
//! Mounts a read-only filesystem at the requested drive letter
//! containing a single file `hello.txt` with hardcoded contents.
//!
//! Goal: prove that our hand-rolled (well, bindgen-generated) FFI to
//! `winfsp-x64.dll` is mechanically correct end-to-end. Production
//! `projgit-winfsp` (Phase 3d) builds on whatever we learn here.
//!
//! Usage:
//!   cargo run --release -- M:        # mount at M:
//!   then in another shell: type M:\hello.txt
//!   Ctrl-C this process to unmount.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]

// Bindgen-generated bindings live here.
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use widestring::U16CString;

use ffi::{
    FSP_FILE_SYSTEM, FSP_FSCTL_FILE_INFO, FSP_FSCTL_VOLUME_INFO, NTSTATUS,
    PSECURITY_DESCRIPTOR, PUINT32, PVOID, PWSTR, SIZE_T, UINT32, UINT64,
};

// -- the one and only "file" we serve -----------------------------------------

const HELLO_NAME: &str = "hello.txt";
const HELLO_CONTENT: &[u8] = b"hello from a hand-rolled WinFsp filesystem\n";

/// Trace one line into trace.log next to the binary. We bypass
/// stderr / stdout because Start-Process buffers them; this gives
/// us direct visibility into which callbacks Windows invokes.
fn trace(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("trace.log")
    {
        let _ = writeln!(f, "{}", msg);
        let _ = f.flush();
    }
}
// A made-up but stable file id we hand back through PFileContext.
// Anything non-null is fine; we use this to distinguish "the root" (1)
// from "the file" (2) when callbacks come back to us.
const ROOT_CONTEXT: usize = 1;
const FILE_CONTEXT: usize = 2;

// Fake but consistent file times for both the root dir and the file.
// 2026-01-01 UTC, in Windows FILETIME (100ns intervals since 1601).
const FAKE_TIME: u64 = 133_790_976_000_000_000;

// -- helpers ------------------------------------------------------------------

/// Convert a wide PWSTR (null-terminated) to a Rust String for matching.
unsafe fn pwstr_to_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: caller guarantees `p` is null-terminated.
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(p as *const u16, len);
    String::from_utf16_lossy(slice)
}

/// Fill an FSP_FSCTL_FILE_INFO for either the root directory or `hello.txt`.
unsafe fn fill_file_info(info: *mut FSP_FSCTL_FILE_INFO, is_dir: bool) {
    if info.is_null() {
        return;
    }
    let info = &mut *info;
    *info = std::mem::zeroed();

    // FILE_ATTRIBUTE_DIRECTORY (0x10) for the root, FILE_ATTRIBUTE_READONLY (0x01) for the file.
    info.FileAttributes = if is_dir { 0x10 } else { 0x01 };
    info.ReparseTag = 0;
    info.AllocationSize = if is_dir { 0 } else { HELLO_CONTENT.len() as UINT64 };
    info.FileSize = if is_dir { 0 } else { HELLO_CONTENT.len() as UINT64 };
    info.CreationTime = FAKE_TIME;
    info.LastAccessTime = FAKE_TIME;
    info.LastWriteTime = FAKE_TIME;
    info.ChangeTime = FAKE_TIME;
    info.IndexNumber = if is_dir { ROOT_CONTEXT as UINT64 } else { FILE_CONTEXT as UINT64 };
    info.HardLinks = 0;
    info.EaSize = 0;
}

// -- callbacks ----------------------------------------------------------------

unsafe extern "C" fn cb_get_volume_info(
    _fs: *mut FSP_FILE_SYSTEM,
    out: *mut FSP_FSCTL_VOLUME_INFO,
) -> NTSTATUS {
    trace("[cb_get_volume_info]");
    let vi = &mut *out;
    vi.TotalSize = HELLO_CONTENT.len() as UINT64;
    vi.FreeSize = 0;
    let label = U16CString::from_str("ProjgitHello").unwrap();
    let label_bytes = label.as_slice();
    let n = label_bytes.len().min(vi.VolumeLabel.len());
    for i in 0..n {
        vi.VolumeLabel[i] = label_bytes[i] as ffi::WCHAR;
    }
    vi.VolumeLabelLength = (n * 2) as u16; // bytes, not chars
    0 // STATUS_SUCCESS
}

unsafe extern "C" fn cb_get_security_by_name(
    _fs: *mut FSP_FILE_SYSTEM,
    file_name: PWSTR,
    p_file_attributes: PUINT32,
    _security_descriptor: PSECURITY_DESCRIPTOR,
    p_security_descriptor_size: *mut SIZE_T,
) -> NTSTATUS {
    let name = pwstr_to_string(file_name);
    let sd_size_in = if p_security_descriptor_size.is_null() { 0 } else { *p_security_descriptor_size };
    trace(&format!("[cb_get_security_by_name] name={name:?} sd_size_in={sd_size_in}"));
    let (is_dir, found) = match name.as_str() {
        "\\" => (true, true),
        "\\hello.txt" => (false, true),
        _ => (false, false),
    };
    if !found {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    }
    if !p_file_attributes.is_null() {
        *p_file_attributes = if is_dir { 0x10 } else { 0x01 };
    }
    if !p_security_descriptor_size.is_null() {
        // We don't supply a security descriptor in this spike; report 0
        // size and STATUS_SUCCESS. WinFsp synthesises a default for us.
        *p_security_descriptor_size = 0;
    }
    0
}

unsafe extern "C" fn cb_open(
    _fs: *mut FSP_FILE_SYSTEM,
    file_name: PWSTR,
    _create_options: UINT32,
    _granted_access: UINT32,
    p_file_context: *mut PVOID,
    file_info: *mut FSP_FSCTL_FILE_INFO,
) -> NTSTATUS {
    let name = pwstr_to_string(file_name);
    trace(&format!("[cb_open] name={name:?}"));
    let (ctx, is_dir) = match name.as_str() {
        "\\" => (ROOT_CONTEXT, true),
        "\\hello.txt" => (FILE_CONTEXT, false),
        _ => return STATUS_OBJECT_NAME_NOT_FOUND,
    };
    *p_file_context = ctx as PVOID;
    fill_file_info(file_info, is_dir);
    0
}

unsafe extern "C" fn cb_close(_fs: *mut FSP_FILE_SYSTEM, _file_context: PVOID) {
    // Stateless — nothing to free.
}

unsafe extern "C" fn cb_cleanup(
    _fs: *mut FSP_FILE_SYSTEM,
    _file_context: PVOID,
    _file_name: PWSTR,
    _flags: ffi::ULONG,
) {
    // Read-only filesystem: nothing to clean up.
}

unsafe extern "C" fn cb_get_file_info(
    _fs: *mut FSP_FILE_SYSTEM,
    file_context: PVOID,
    file_info: *mut FSP_FSCTL_FILE_INFO,
) -> NTSTATUS {
    trace(&format!("[cb_get_file_info] ctx={}", file_context as usize));
    let is_dir = (file_context as usize) == ROOT_CONTEXT;
    fill_file_info(file_info, is_dir);
    0
}

unsafe extern "C" fn cb_read(
    _fs: *mut FSP_FILE_SYSTEM,
    file_context: PVOID,
    buffer: PVOID,
    offset: UINT64,
    length: UINT32,
    p_bytes_transferred: PUINT32,
) -> NTSTATUS {
    if (file_context as usize) != FILE_CONTEXT {
        return STATUS_INVALID_PARAMETER;
    }
    if offset >= HELLO_CONTENT.len() as UINT64 {
        return STATUS_END_OF_FILE;
    }
    let start = offset as usize;
    let end = (start + length as usize).min(HELLO_CONTENT.len());
    let n = end - start;
    std::ptr::copy_nonoverlapping(
        HELLO_CONTENT.as_ptr().add(start),
        buffer as *mut u8,
        n,
    );
    *p_bytes_transferred = n as UINT32;
    0
}

unsafe extern "C" fn cb_read_directory(
    fs: *mut FSP_FILE_SYSTEM,
    file_context: PVOID,
    _pattern: PWSTR,
    marker: PWSTR,
    buffer: PVOID,
    length: ffi::ULONG,
    p_bytes_transferred: *mut ffi::ULONG,
) -> NTSTATUS {
    trace(&format!(
        "[cb_read_directory] ctx={} marker={:?} length={}",
        file_context as usize,
        pwstr_to_string(marker),
        length
    ));
    if (file_context as usize) != ROOT_CONTEXT {
        return STATUS_INVALID_PARAMETER;
    }

    // Iterate our entries: only one — "hello.txt".
    // WinFsp uses `marker` for resumption: skip until *after* the marker.
    let entries: [(&str, bool); 1] = [(HELLO_NAME, false)];

    let marker_str = pwstr_to_string(marker);
    let mut started = marker_str.is_empty();

    for (name, is_dir) in entries.iter() {
        if !started {
            if marker_str == *name {
                started = true;
            }
            continue;
        }

        // Build an FSP_FSCTL_DIR_INFO on the stack with room for the wide name.
        // Layout:
        //   UINT16 Size;
        //   FSP_FSCTL_FILE_INFO FileInfo;
        //   UINT64 NextOffset;
        //   WCHAR FileNameBuf[];   // flexible
        // We use a fixed-size buffer of MAX_PATH chars.
        const NAME_MAX: usize = 260;
        let wname = U16CString::from_str(*name).unwrap();
        let wname_slice = wname.as_slice(); // no NUL
        if wname_slice.len() > NAME_MAX {
            continue;
        }

        let mut storage: [u8;
            std::mem::size_of::<ffi::FSP_FSCTL_DIR_INFO>() + NAME_MAX * 2] =
            [0; std::mem::size_of::<ffi::FSP_FSCTL_DIR_INFO>() + NAME_MAX * 2];
        let total_size =
            std::mem::size_of::<ffi::FSP_FSCTL_DIR_INFO>() + wname_slice.len() * 2;
        let dir_info = storage.as_mut_ptr() as *mut ffi::FSP_FSCTL_DIR_INFO;
        (*dir_info).Size = total_size as u16;
        fill_file_info(&mut (*dir_info).FileInfo, *is_dir);
        // Copy name into the trailing buffer that immediately follows
        // the struct's named fields (FileNameBuf is a flexible array
        // of length 0 in C; bindgen represents it as a 0-length array).
        let name_ptr = (storage.as_mut_ptr() as usize
            + std::mem::offset_of!(ffi::FSP_FSCTL_DIR_INFO, FileNameBuf))
            as *mut u16;
        std::ptr::copy_nonoverlapping(wname_slice.as_ptr(), name_ptr, wname_slice.len());

        // Hand it to WinFsp's helper that fills the reply buffer.
        let ok = ffi::FspFileSystemAddDirInfo(
            dir_info,
            buffer,
            length,
            p_bytes_transferred,
        );
        if ok == 0 {
            // Buffer full -- caller will resume with this entry's name as marker.
            return 0;
        }
    }

    // Signal end-of-listing: AddDirInfo with NULL.
    ffi::FspFileSystemAddDirInfo(ptr::null_mut(), buffer, length, p_bytes_transferred);
    let _ = fs;
    0
}

// -- NTSTATUS values we use; copied here to avoid pulling in ntdef.h ----------

const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_000F_u32 as i32;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;
const STATUS_END_OF_FILE: i32 = 0xC000_0011_u32 as i32;

// -- main ---------------------------------------------------------------------

static SHOULD_STOP: AtomicBool = AtomicBool::new(false);

fn main() -> Result<(), String> {
    let mountpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "M:".to_owned());

    // Truncate trace.log so each run starts clean.
    let _ = std::fs::write("trace.log", "");
    trace("=== main started ===");
    trace(&format!("mountpoint = {mountpoint}"));

    println!("== Phase 3c spike: WinFsp hello-world ==");
    println!("Mountpoint: {mountpoint}");

    // Send WinFsp's internal debug log to stderr.
    unsafe {
        let stderr = windows_sys::Win32::System::Console::GetStdHandle(
            windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
        );
        ffi::FspDebugLogSetHandle(stderr as ffi::HANDLE);
    }

    // Build the volume params struct.
    let mut params: ffi::FSP_FSCTL_VOLUME_PARAMS = unsafe { std::mem::zeroed() };
    params.Version = std::mem::size_of::<ffi::FSP_FSCTL_VOLUME_PARAMS>() as u16;
    params.SectorSize = 4096;
    params.SectorsPerAllocationUnit = 1;
    params.VolumeCreationTime = FAKE_TIME;
    params.VolumeSerialNumber = 0xC0FFEEAA;
    params.FileInfoTimeout = 1000;
    // Bitfield setters: match what memfs does (don't set ReadOnlyVolume
    // explicitly; the read-only behaviour comes from not implementing
    // any write callbacks).
    params.set_CasePreservedNames(1);
    params.set_UnicodeOnDisk(1);
    params.set_PersistentAcls(1);
    params.set_PostCleanupWhenModifiedOnly(1);
    params.set_AllowOpenInKernelMode(1);

    // FileSystem name appears in mtab-like tools. Use an explicit
    // wchar-by-wchar copy to avoid any [u16] vs [WCHAR] surprises.
    let fs_name = U16CString::from_str("-PROJGIT").unwrap();
    let fs_name_slice = fs_name.as_slice();
    let n = fs_name_slice.len().min(params.FileSystemName.len() - 1);
    for i in 0..n {
        params.FileSystemName[i] = fs_name_slice[i] as ffi::WCHAR;
    }

    // Build the interface table.
    let mut iface: ffi::FSP_FILE_SYSTEM_INTERFACE = unsafe { std::mem::zeroed() };
    iface.GetVolumeInfo = Some(cb_get_volume_info);
    iface.GetSecurityByName = Some(cb_get_security_by_name);
    iface.Open = Some(cb_open);
    iface.Cleanup = Some(cb_cleanup);
    iface.Close = Some(cb_close);
    iface.GetFileInfo = Some(cb_get_file_info);
    iface.Read = Some(cb_read);
    iface.ReadDirectory = Some(cb_read_directory);

    // Create the file system object.
    let mut fs: *mut ffi::FSP_FILE_SYSTEM = ptr::null_mut();
    let dev_path = U16CString::from_str("WinFsp.Disk").unwrap();
    let status = unsafe {
        ffi::FspFileSystemCreate(
            dev_path.into_raw() as *mut u16,
            &params,
            &iface,
            &mut fs,
        )
    };
    trace(&format!("FspFileSystemCreate -> 0x{status:08X}"));
    if status != 0 {
        return Err(format!("FspFileSystemCreate failed: NTSTATUS=0x{status:08X}"));
    }

    // Mount.
    let mp = U16CString::from_str(&mountpoint).unwrap();
    let status = unsafe { ffi::FspFileSystemSetMountPoint(fs, mp.into_raw() as *mut u16) };
    trace(&format!("FspFileSystemSetMountPoint -> 0x{status:08X}"));
    if status != 0 {
        unsafe { ffi::FspFileSystemDelete(fs) };
        return Err(format!("FspFileSystemSetMountPoint failed: NTSTATUS=0x{status:08X}"));
    }

    // Start dispatcher (one thread is plenty for hello-world).
    let status = unsafe { ffi::FspFileSystemStartDispatcher(fs, 0) };
    trace(&format!("FspFileSystemStartDispatcher -> 0x{status:08X}"));
    if status != 0 {
        unsafe {
            ffi::FspFileSystemRemoveMountPoint(fs);
            ffi::FspFileSystemDelete(fs);
        }
        return Err(format!("FspFileSystemStartDispatcher failed: NTSTATUS=0x{status:08X}"));
    }

    println!("Mounted. Try:  type {mountpoint}\\hello.txt");
    println!("Press Ctrl-C to unmount.");

    // Catch Ctrl-C to unmount cleanly.
    ctrlc_handler();
    while !SHOULD_STOP.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    println!("Unmounting...");
    unsafe {
        ffi::FspFileSystemStopDispatcher(fs);
        ffi::FspFileSystemRemoveMountPoint(fs);
        ffi::FspFileSystemDelete(fs);
    }
    println!("Done.");
    Ok(())
}

fn ctrlc_handler() {
    // Minimal: poll for Ctrl-C via the windows-sys SetConsoleCtrlHandler API
    // would be cleaner, but for a spike the crate `ctrlc` would add a dep.
    // Instead, we let the user kill the process; on process exit Windows
    // tears down the mount automatically. We still register a signal handler
    // for graceful unmount via Ctrl-C through std::sync::mpsc-based polling
    // would be overkill here -- accept abrupt exit.
    extern "C" fn handle(sig: i32) {
        // SIGINT only; flag the loop to exit.
        if sig == 2 {
            SHOULD_STOP.store(true, Ordering::SeqCst);
        }
    }
    unsafe {
        // libc signal exists on Windows for SIGINT.
        let _ = libc_signal(2, handle as *const () as usize);
    }
}

// Tiny wrapper around msvcrt's signal() so we don't need the libc crate.
extern "C" {
    #[link_name = "signal"]
    fn msvcrt_signal(sig: i32, handler: usize) -> usize;
}
unsafe fn libc_signal(sig: i32, handler: usize) -> usize {
    msvcrt_signal(sig, handler)
}
