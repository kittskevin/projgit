//! Audited FFI bindings and dynamic loader for `winfsp-x64.dll` (and
//! `winfsp-a64.dll` / `winfsp-x86.dll`).
//!
//! # License & WinFsp FLOSS Exception
//!
//! These bindings are written from scratch for `projgit` under
//! `MIT OR Apache-2.0`. At runtime on Windows, this module dynamically loads
//! the WinFsp DLL (`winfsp-x64.dll`), which is licensed by Bill Zissimopoulos
//! under GPLv3 with the **WinFsp FLOSS Exception**:
//!
//! > WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
//! > <https://github.com/winfsp/winfsp>
//!
//! # Safety Isolation
//!
//! This module is the **only** module in `projgit-winfsp` (and across the
//! entire `projgit` workspace) with `#![allow(unsafe_code)]`. All raw C
//! pointers, callback trampolines, and Win32 / WinFsp dynamic symbol lookups
//! are encapsulated behind the safe [`WinFspBackend`] trait and the RAII
//! [`RawWinFspMount`] handle.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

/// NTSTATUS return code type used by WinFsp callbacks and APIs.
pub type NtStatus = i32;

// ---------------------------------------------------------------------------
// Standard NTSTATUS and Win32 constants
// ---------------------------------------------------------------------------

/// `STATUS_SUCCESS` (`0x00000000`).
pub const STATUS_SUCCESS: NtStatus = 0x0000_0000_u32 as i32;
/// `STATUS_REPARSE` (`0x00000104`).
pub const STATUS_REPARSE: NtStatus = 0x0000_0104_u32 as i32;
/// `STATUS_BUFFER_OVERFLOW` (`0x80000005`).
pub const STATUS_BUFFER_OVERFLOW: NtStatus = 0x8000_0005_u32 as i32;
/// `STATUS_UNSUCCESSFUL` (`0xC0000001`).
pub const STATUS_UNSUCCESSFUL: NtStatus = 0xC000_0001_u32 as i32;
/// `STATUS_INVALID_HANDLE` (`0xC0000008`).
pub const STATUS_INVALID_HANDLE: NtStatus = 0xC000_0008_u32 as i32;
/// `STATUS_INVALID_PARAMETER` (`0xC000000D`).
pub const STATUS_INVALID_PARAMETER: NtStatus = 0xC000_000D_u32 as i32;
/// `STATUS_END_OF_FILE` (`0xC0000011`).
pub const STATUS_END_OF_FILE: NtStatus = 0xC000_0011_u32 as i32;
/// `STATUS_BUFFER_TOO_SMALL` (`0xC0000023`).
pub const STATUS_BUFFER_TOO_SMALL: NtStatus = 0xC000_0023_u32 as i32;
/// `STATUS_OBJECT_NAME_INVALID` (`0xC0000033`).
pub const STATUS_OBJECT_NAME_INVALID: NtStatus = 0xC000_0033_u32 as i32;
/// `STATUS_OBJECT_NAME_NOT_FOUND` (`0xC0000034`).
pub const STATUS_OBJECT_NAME_NOT_FOUND: NtStatus = 0xC000_0034_u32 as i32;
/// `STATUS_OBJECT_PATH_NOT_FOUND` (`0xC000003A`).
pub const STATUS_OBJECT_PATH_NOT_FOUND: NtStatus = 0xC000_003A_u32 as i32;
/// `STATUS_MEDIA_WRITE_PROTECTED` (`0xC00000A2`).
pub const STATUS_MEDIA_WRITE_PROTECTED: NtStatus = 0xC000_00A2_u32 as i32;
/// `STATUS_FILE_IS_A_DIRECTORY` (`0xC00000BA`).
pub const STATUS_FILE_IS_A_DIRECTORY: NtStatus = 0xC000_00BA_u32 as i32;
/// `STATUS_NOT_SUPPORTED` (`0xC00000BB`).
pub const STATUS_NOT_SUPPORTED: NtStatus = 0xC000_00BB_u32 as i32;
/// `STATUS_NOT_A_DIRECTORY` (`0xC0000103`).
pub const STATUS_NOT_A_DIRECTORY: NtStatus = 0xC000_0103_u32 as i32;
/// `STATUS_NOT_A_REPARSE_POINT` (`0xC0000275`).
pub const STATUS_NOT_A_REPARSE_POINT: NtStatus = 0xC000_0275_u32 as i32;

/// Win32 `FILE_ATTRIBUTE_READONLY` (`0x00000001`).
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
/// Win32 `FILE_ATTRIBUTE_DIRECTORY` (`0x00000010`).
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// Win32 `FILE_ATTRIBUTE_REPARSE_POINT` (`0x00000400`).
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// NT `CreateOptions` flag: caller requires a directory (`0x00000001`).
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
/// NT `CreateOptions` flag: caller requires a non-directory (`0x00000040`).
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
/// NT `CreateOptions` flag: open the reparse point itself (`0x00200000`).
pub const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// `FSP_FSCTL_VOLUME_PARAMS.flags` bit positions (from `<winfsp/fsctl.h>`).
pub const VOL_FLAG_CASE_PRESERVED_NAMES: u32 = 1 << 1;
pub const VOL_FLAG_UNICODE_ON_DISK: u32 = 1 << 2;
pub const VOL_FLAG_PERSISTENT_ACLS: u32 = 1 << 3;
pub const VOL_FLAG_REPARSE_POINTS: u32 = 1 << 4;
pub const VOL_FLAG_READ_ONLY_VOLUME: u32 = 1 << 9;
pub const VOL_FLAG_POST_CLEANUP_WHEN_MODIFIED_ONLY: u32 = 1 << 10;
pub const VOL_FLAG_PASS_QUERY_DIRECTORY_FILE_NAME: u32 = 1 << 13;
pub const VOL_FLAG_UM_FILE_CONTEXT_IS_USER_CONTEXT2: u32 = 1 << 16;
pub const VOL_FLAG_ALLOW_OPEN_IN_KERNEL_MODE: u32 = 1 << 24;
pub const VOL_FLAG_WSL_FEATURES: u32 = 1 << 26;
pub const VOL_FLAG_SUPPORTS_POSIX_UNLINK_RENAME: u32 = 1 << 29;
pub const VOL_FLAG_POST_DISPOSITION_WHEN_NECESSARY_ONLY: u32 = 1 << 30;

const fn nt_success(status: NtStatus) -> bool {
    status >= 0
}

// ---------------------------------------------------------------------------
// WinFsp C ABI structs (with compile-time layout assertions matching fsctl.h)
// ---------------------------------------------------------------------------

/// `FSP_FSCTL_VOLUME_PARAMS` (`504` bytes, `<winfsp/fsctl.h>`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FspFsctlVolumeParams {
    pub version: u16,
    pub sector_size: u16,
    pub sectors_per_allocation_unit: u16,
    pub max_component_length: u16,
    pub volume_creation_time: u64,
    pub volume_serial_number: u32,
    pub transact_timeout: u32,
    pub irp_timeout: u32,
    pub irp_capacity: u32,
    pub file_info_timeout: u32,
    pub flags: u32,
    pub prefix: [u16; 192],
    pub file_system_name: [u16; 16],
    pub v1_flags: u32,
    pub volume_info_timeout: u32,
    pub dir_info_timeout: u32,
    pub security_timeout: u32,
    pub stream_info_timeout: u32,
    pub ea_timeout: u32,
    pub fsext_control_code: u32,
    pub reserved32: [u32; 1],
    pub reserved64: [u64; 2],
}

const _: () = assert!(std::mem::size_of::<FspFsctlVolumeParams>() == 504);
const _: () = assert!(std::mem::offset_of!(FspFsctlVolumeParams, flags) == 36);
const _: () = assert!(std::mem::offset_of!(FspFsctlVolumeParams, prefix) == 40);
const _: () = assert!(std::mem::offset_of!(FspFsctlVolumeParams, file_system_name) == 424);
const _: () = assert!(std::mem::offset_of!(FspFsctlVolumeParams, v1_flags) == 456);

/// `FSP_FSCTL_VOLUME_INFO` (`88` bytes, `<winfsp/fsctl.h>`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FspFsctlVolumeInfo {
    pub total_size: u64,
    pub free_size: u64,
    pub volume_label_length: u16,
    pub volume_label: [u16; 32],
}

const _: () = assert!(std::mem::size_of::<FspFsctlVolumeInfo>() == 88);

/// `FSP_FSCTL_FILE_INFO` (`72` bytes, `<winfsp/fsctl.h>`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FspFsctlFileInfo {
    pub file_attributes: u32,
    pub reparse_tag: u32,
    pub allocation_size: u64,
    pub file_size: u64,
    pub creation_time: u64,
    pub last_access_time: u64,
    pub last_write_time: u64,
    pub change_time: u64,
    pub index_number: u64,
    pub hard_links: u32,
    pub ea_size: u32,
}

const _: () = assert!(std::mem::size_of::<FspFsctlFileInfo>() == 72);

/// Fixed 104-byte header of `FSP_FSCTL_DIR_INFO` (`<winfsp/fsctl.h>`),
/// followed immediately in memory by `FileNameBuf: [u16]` (UTF-16LE, no NUL).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FspFsctlDirInfoHeader {
    pub size: u16,
    pub _pad0: [u16; 3],
    pub file_info: FspFsctlFileInfo,
    pub padding: [u8; 24],
}

const _: () = assert!(std::mem::size_of::<FspFsctlDirInfoHeader>() == 104);
const _: () = assert!(std::mem::offset_of!(FspFsctlDirInfoHeader, file_info) == 8);
const _: () = assert!(std::mem::offset_of!(FspFsctlDirInfoHeader, padding) == 80);

/// `IO_STATUS_BLOCK` (`16` bytes on 64-bit Windows).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoStatusBlock {
    pub status: NtStatus,
    pub _pad: u32,
    pub information: usize,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<IoStatusBlock>() == 16);

/// `FSP_FILE_SYSTEM` (`792` bytes on 64-bit Windows, `<winfsp/winfsp.h>`).
#[repr(C)]
pub struct FspFileSystem {
    pub version: u16,
    pub user_context: *mut c_void,
    pub volume_name: [u16; 256],
    pub volume_handle: *mut c_void,
    pub enter_operation: *mut c_void,
    pub leave_operation: *mut c_void,
    pub operations: [*mut c_void; 22],
    pub interface: *const FspFileSystemInterface,
    pub dispatcher_thread: *mut c_void,
    pub dispatcher_thread_count: u32,
    pub dispatcher_result: NtStatus,
    pub mount_point: *mut u16,
    pub mount_handle: *mut c_void,
    pub debug_log: u32,
    pub op_guard_strategy: u32,
    pub op_guard_lock: *mut c_void,
    pub um_file_context_is_user_context2: u8,
    pub um_file_context_is_full_context: u8,
    pub flags: u16,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<FspFileSystem>() == 792);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::offset_of!(FspFileSystem, user_context) == 8);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::offset_of!(FspFileSystem, mount_point) == 752);

type GetReparsePointByNameFn = unsafe extern "system" fn(
    fs: *mut FspFileSystem,
    context: *mut c_void,
    file_name: *mut u16,
    is_directory: u8,
    buffer: *mut c_void,
    p_size: *mut usize,
) -> NtStatus;

/// `FSP_FILE_SYSTEM_INTERFACE` (`512` bytes / 64 pointer slots on 64-bit,
/// `<winfsp/winfsp.h>`).
#[repr(C)]
pub struct FspFileSystemInterface {
    pub get_volume_info: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            volume_info: *mut FspFsctlVolumeInfo,
        ) -> NtStatus,
    >,
    pub set_volume_label: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            volume_label: *mut u16,
            volume_info: *mut FspFsctlVolumeInfo,
        ) -> NtStatus,
    >,
    pub get_security_by_name: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_name: *mut u16,
            p_file_attributes: *mut u32,
            security_descriptor: *mut c_void,
            p_security_descriptor_size: *mut usize,
        ) -> NtStatus,
    >,
    pub create: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_name: *mut u16,
            create_options: u32,
            granted_access: u32,
            file_attributes: u32,
            security_descriptor: *mut c_void,
            allocation_size: u64,
            p_file_context: *mut *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub open: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_name: *mut u16,
            create_options: u32,
            granted_access: u32,
            p_file_context: *mut *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub overwrite: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_attributes: u32,
            replace_file_attributes: u8,
            allocation_size: u64,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub cleanup: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
            flags: u32,
        ),
    >,
    pub close: Option<unsafe extern "system" fn(fs: *mut FspFileSystem, file_context: *mut c_void)>,
    pub read: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            buffer: *mut c_void,
            offset: u64,
            length: u32,
            p_bytes_transferred: *mut u32,
        ) -> NtStatus,
    >,
    pub write: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            buffer: *mut c_void,
            offset: u64,
            length: u32,
            write_to_end_of_file: u8,
            constrained_io: u8,
            p_bytes_transferred: *mut u32,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub flush: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub get_file_info: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub set_basic_info: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_attributes: u32,
            creation_time: u64,
            last_access_time: u64,
            last_write_time: u64,
            change_time: u64,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub set_file_size: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            new_size: u64,
            set_allocation_size: u8,
            file_info: *mut FspFsctlFileInfo,
        ) -> NtStatus,
    >,
    pub can_delete: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
        ) -> NtStatus,
    >,
    pub rename: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
            new_file_name: *mut u16,
            replace_if_exists: u8,
        ) -> NtStatus,
    >,
    pub get_security: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            security_descriptor: *mut c_void,
            p_security_descriptor_size: *mut usize,
        ) -> NtStatus,
    >,
    pub set_security: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            security_information: u32,
            modification_descriptor: *mut c_void,
        ) -> NtStatus,
    >,
    pub read_directory: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            pattern: *mut u16,
            marker: *mut u16,
            buffer: *mut c_void,
            length: u32,
            p_bytes_transferred: *mut u32,
        ) -> NtStatus,
    >,
    pub resolve_reparse_points: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_name: *mut u16,
            reparse_point_index: u32,
            resolve_last_path_component: u8,
            p_io_status: *mut IoStatusBlock,
            buffer: *mut c_void,
            p_size: *mut usize,
        ) -> NtStatus,
    >,
    pub get_reparse_point: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
            buffer: *mut c_void,
            p_size: *mut usize,
        ) -> NtStatus,
    >,
    pub set_reparse_point: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
            buffer: *mut c_void,
            size: usize,
        ) -> NtStatus,
    >,
    pub delete_reparse_point: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
            buffer: *mut c_void,
            size: usize,
        ) -> NtStatus,
    >,
    pub get_stream_info: usize,
    pub get_dir_info_by_name: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
            dir_info: *mut FspFsctlDirInfoHeader,
        ) -> NtStatus,
    >,
    pub control: usize,
    pub set_delete: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *mut u16,
            delete_file: u8,
        ) -> NtStatus,
    >,
    pub create_ex: usize,
    pub overwrite_ex: usize,
    pub get_ea: usize,
    pub set_ea: usize,
    pub obsolete0: usize,
    pub dispatcher_stopped: usize,
    pub reserved: [usize; 31],
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<FspFileSystemInterface>() == 512);

// ---------------------------------------------------------------------------
// Win32 imports (kernel32 + advapi32)
// ---------------------------------------------------------------------------

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER_CLASS: u32 = 1;
const TOKEN_PRIMARY_GROUP_CLASS: u32 = 5;
const SDDL_REVISION_1: u32 = 1;

#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[repr(C)]
struct TokenPrimaryGroup {
    primary_group: *mut c_void,
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> isize;
    fn GetProcAddress(h_module: isize, lp_proc_name: *const u8) -> *const c_void;
    fn GetCurrentProcess() -> isize;
    fn CloseHandle(h_object: isize) -> i32;
    fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
    fn GetLastError() -> u32;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        hkey: isize,
        lp_sub_key: *const u16,
        lp_value: *const u16,
        dw_flags: u32,
        pdw_type: *mut u32,
        pv_data: *mut c_void,
        pcb_data: *mut u32,
    ) -> i32;
    fn OpenProcessToken(
        process_handle: isize,
        desired_access: u32,
        token_handle: *mut isize,
    ) -> i32;
    fn GetTokenInformation(
        token_handle: isize,
        token_information_class: u32,
        token_information: *mut c_void,
        token_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
}

// ---------------------------------------------------------------------------
// Dynamic loader for winfsp-*.dll (matching FspLoad in <winfsp/winfsp.h>)
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
const WINFSP_DLL_NAME: &str = "winfsp-x64.dll";
#[cfg(target_arch = "aarch64")]
const WINFSP_DLL_NAME: &str = "winfsp-a64.dll";
#[cfg(target_arch = "x86")]
const WINFSP_DLL_NAME: &str = "winfsp-x86.dll";

/// Dynamically resolved function table from `winfsp-x64.dll`.
pub struct WinFspApi {
    pub fsp_version: unsafe extern "system" fn(p_version: *mut u32) -> NtStatus,
    pub fsp_file_system_create: unsafe extern "system" fn(
        device_path: *const u16,
        volume_params: *const FspFsctlVolumeParams,
        interface: *const FspFileSystemInterface,
        p_file_system: *mut *mut FspFileSystem,
    ) -> NtStatus,
    pub fsp_file_system_delete: unsafe extern "system" fn(file_system: *mut FspFileSystem),
    pub fsp_file_system_set_mount_point: unsafe extern "system" fn(
        file_system: *mut FspFileSystem,
        mount_point: *const u16,
    ) -> NtStatus,
    pub fsp_file_system_remove_mount_point:
        unsafe extern "system" fn(file_system: *mut FspFileSystem),
    pub fsp_file_system_start_dispatcher:
        unsafe extern "system" fn(file_system: *mut FspFileSystem, thread_count: u32) -> NtStatus,
    pub fsp_file_system_stop_dispatcher: unsafe extern "system" fn(file_system: *mut FspFileSystem),
    pub fsp_file_system_acquire_directory_buffer: unsafe extern "system" fn(
        p_dir_buffer: *mut *mut c_void,
        reset: u8,
        p_result: *mut NtStatus,
    ) -> u8,
    pub fsp_file_system_fill_directory_buffer: unsafe extern "system" fn(
        p_dir_buffer: *mut *mut c_void,
        dir_info: *mut FspFsctlDirInfoHeader,
        p_result: *mut NtStatus,
    ) -> u8,
    pub fsp_file_system_release_directory_buffer:
        unsafe extern "system" fn(p_dir_buffer: *mut *mut c_void),
    pub fsp_file_system_read_directory_buffer: unsafe extern "system" fn(
        p_dir_buffer: *mut *mut c_void,
        marker: *const u16,
        buffer: *mut c_void,
        length: u32,
        p_bytes_transferred: *mut u32,
    ),
    pub fsp_file_system_delete_directory_buffer:
        unsafe extern "system" fn(p_dir_buffer: *mut *mut c_void),
    pub fsp_file_system_find_reparse_point: unsafe extern "system" fn(
        file_system: *mut FspFileSystem,
        get_reparse_point_by_name: GetReparsePointByNameFn,
        context: *mut c_void,
        file_name: *mut u16,
        p_reparse_point_index: *mut u32,
    ) -> u8,
    pub fsp_file_system_resolve_reparse_points: unsafe extern "system" fn(
        file_system: *mut FspFileSystem,
        get_reparse_point_by_name: GetReparsePointByNameFn,
        context: *mut c_void,
        file_name: *mut u16,
        reparse_point_index: u32,
        resolve_last_path_component: u8,
        p_io_status: *mut IoStatusBlock,
        buffer: *mut c_void,
        p_size: *mut usize,
    ) -> NtStatus,
}

static WINFSP_API: OnceLock<Result<WinFspApi, String>> = OnceLock::new();

fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn pwstr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    String::from_utf16_lossy(slice)
}

fn query_winfsp_install_dir() -> Option<PathBuf> {
    for subkey in ["Software\\WOW6432Node\\WinFsp", "Software\\WinFsp"] {
        let subkey_w = to_wide_nul(subkey);
        let value_w = to_wide_nul("InstallDir");
        let mut buf = [0u16; 520];
        let mut byte_size = (buf.len() * 2) as u32;
        // SAFETY: `buf` is a valid stack buffer of `byte_size` bytes and
        // `subkey_w` / `value_w` are NUL-terminated UTF-16 strings.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                subkey_w.as_ptr(),
                value_w.as_ptr(),
                RRF_RT_REG_SZ,
                ptr::null_mut(),
                buf.as_mut_ptr().cast::<c_void>(),
                &mut byte_size,
            )
        };
        if rc == 0 && byte_size >= 2 {
            let char_len = (byte_size as usize / 2).saturating_sub(1);
            let s = String::from_utf16_lossy(&buf[..char_len]);
            let trimmed = s.trim_end_matches('\0');
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

unsafe fn load_proc<T: Copy>(module: isize, name: &'static [u8]) -> Result<T, String> {
    debug_assert_eq!(name.last(), Some(&0));
    let sym = GetProcAddress(module, name.as_ptr());
    if sym.is_null() {
        let symbol_str = std::str::from_utf8(&name[..name.len() - 1]).unwrap_or("<unknown>");
        return Err(format!(
            "WinFsp symbol `{symbol_str}` not found in {WINFSP_DLL_NAME}"
        ));
    }
    debug_assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*const c_void>()
    );
    Ok(std::mem::transmute_copy(&sym))
}

impl WinFspApi {
    /// Load `winfsp-x64.dll` (or architecture equivalent) once and return
    /// the resolved symbol table.
    pub fn get() -> Result<&'static Self, String> {
        WINFSP_API
            .get_or_init(|| unsafe {
                let dll_w = to_wide_nul(WINFSP_DLL_NAME);
                let mut module = LoadLibraryW(dll_w.as_ptr());
                if module == 0 {
                    if let Some(install_dir) = query_winfsp_install_dir() {
                        let full = install_dir.join("bin").join(WINFSP_DLL_NAME);
                        let full_w = to_wide_nul(&full.to_string_lossy());
                        module = LoadLibraryW(full_w.as_ptr());
                    }
                }
                if module == 0 {
                    // Fallback to standard Program Files (x86) path.
                    let fallback =
                        PathBuf::from(r"C:\Program Files (x86)\WinFsp\bin").join(WINFSP_DLL_NAME);
                    let fallback_w = to_wide_nul(&fallback.to_string_lossy());
                    module = LoadLibraryW(fallback_w.as_ptr());
                }
                if module == 0 {
                    let err = GetLastError();
                    return Err(format!(
                        "unable to load {WINFSP_DLL_NAME} (Win32 error {err}); \
                         install WinFsp (`winget install WinFsp.WinFsp`)"
                    ));
                }

                Ok(Self {
                    fsp_version: load_proc(module, b"FspVersion\0")?,
                    fsp_file_system_create: load_proc(module, b"FspFileSystemCreate\0")?,
                    fsp_file_system_delete: load_proc(module, b"FspFileSystemDelete\0")?,
                    fsp_file_system_set_mount_point: load_proc(
                        module,
                        b"FspFileSystemSetMountPoint\0",
                    )?,
                    fsp_file_system_remove_mount_point: load_proc(
                        module,
                        b"FspFileSystemRemoveMountPoint\0",
                    )?,
                    fsp_file_system_start_dispatcher: load_proc(
                        module,
                        b"FspFileSystemStartDispatcher\0",
                    )?,
                    fsp_file_system_stop_dispatcher: load_proc(
                        module,
                        b"FspFileSystemStopDispatcher\0",
                    )?,
                    fsp_file_system_acquire_directory_buffer: load_proc(
                        module,
                        b"FspFileSystemAcquireDirectoryBuffer\0",
                    )?,
                    fsp_file_system_fill_directory_buffer: load_proc(
                        module,
                        b"FspFileSystemFillDirectoryBuffer\0",
                    )?,
                    fsp_file_system_release_directory_buffer: load_proc(
                        module,
                        b"FspFileSystemReleaseDirectoryBuffer\0",
                    )?,
                    fsp_file_system_read_directory_buffer: load_proc(
                        module,
                        b"FspFileSystemReadDirectoryBuffer\0",
                    )?,
                    fsp_file_system_delete_directory_buffer: load_proc(
                        module,
                        b"FspFileSystemDeleteDirectoryBuffer\0",
                    )?,
                    fsp_file_system_find_reparse_point: load_proc(
                        module,
                        b"FspFileSystemFindReparsePoint\0",
                    )?,
                    fsp_file_system_resolve_reparse_points: load_proc(
                        module,
                        b"FspFileSystemResolveReparsePoints\0",
                    )?,
                })
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// Returns `true` if `winfsp-x64.dll` (or the architecture-appropriate WinFsp
/// DLL) is installed and loadable on this Windows host.
#[must_use]
pub fn is_winfsp_available() -> bool {
    WinFspApi::get().is_ok()
}

/// Query the loaded WinFsp runtime version `(major, minor)` if available.
pub fn query_winfsp_version() -> Result<(u16, u16), String> {
    let api = WinFspApi::get()?;
    let mut ver: u32 = 0;
    // SAFETY: `ver` is a valid aligned `u32` on the stack.
    let status = unsafe { (api.fsp_version)(&mut ver) };
    if !nt_success(status) {
        return Err(format!("FspVersion failed with NTSTATUS 0x{status:08X}"));
    }
    Ok(((ver >> 16) as u16, (ver & 0xFFFF) as u16))
}

// ---------------------------------------------------------------------------
// Per-user SDDL security descriptor builder (for Git `safe.directory`)
// ---------------------------------------------------------------------------

/// Build a self-relative Windows `SECURITY_DESCRIPTOR` owned by the current
/// process token's user SID and primary group SID, granting full read/execute
/// access (`O:<user-sid>G:<group-sid>D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)`).
///
/// Reporting the calling user as the volume and file owner ensures Git for
/// Windows passes its `safe.directory` ownership check on mounted projections.
pub fn build_current_user_security_descriptor() -> Result<Vec<u8>, String> {
    let sddl = current_process_sddl()
        .unwrap_or_else(|_| "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)".to_owned());
    sddl_to_security_descriptor(&sddl)
}

fn current_process_sddl() -> Result<String, String> {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle (-1) that is always valid.
    let process = unsafe { GetCurrentProcess() };
    let mut token: isize = 0;
    // SAFETY: `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!("OpenProcessToken failed: {}", unsafe {
            GetLastError()
        }));
    }

    let result = (|| {
        let user_sid = query_token_sid_string(token, TOKEN_USER_CLASS)?;
        let group_sid = query_token_sid_string(token, TOKEN_PRIMARY_GROUP_CLASS)?;
        Ok(format!(
            "O:{user_sid}G:{group_sid}D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"
        ))
    })();

    // SAFETY: `token` was opened by `OpenProcessToken`.
    unsafe {
        CloseHandle(token);
    }
    result
}

fn query_token_sid_string(token: isize, info_class: u32) -> Result<String, String> {
    let mut needed: u32 = 0;
    // SAFETY: First call with null buffer queries required size.
    unsafe {
        GetTokenInformation(token, info_class, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err("GetTokenInformation returned 0 buffer size".to_owned());
    }
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` has length `needed`.
    if unsafe {
        GetTokenInformation(
            token,
            info_class,
            buf.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(format!("GetTokenInformation failed: {}", unsafe {
            GetLastError()
        }));
    }

    let sid_ptr = if info_class == TOKEN_USER_CLASS {
        // SAFETY: `TOKEN_USER` starts with `SID_AND_ATTRIBUTES`.
        unsafe { (*buf.as_ptr().cast::<SidAndAttributes>()).sid }
    } else {
        // SAFETY: `TOKEN_PRIMARY_GROUP` starts with `PSID`.
        unsafe { (*buf.as_ptr().cast::<TokenPrimaryGroup>()).primary_group }
    };

    let mut str_ptr: *mut u16 = ptr::null_mut();
    // SAFETY: `sid_ptr` is valid within `buf`.
    if unsafe { ConvertSidToStringSidW(sid_ptr, &mut str_ptr) } == 0 || str_ptr.is_null() {
        return Err(format!("ConvertSidToStringSidW failed: {}", unsafe {
            GetLastError()
        }));
    }
    // SAFETY: `str_ptr` is a NUL-terminated UTF-16 string allocated by LocalAlloc.
    let sid_str = unsafe { pwstr_to_string(str_ptr) };
    // SAFETY: `LocalFree` frees the buffer allocated by `ConvertSidToStringSidW`.
    unsafe {
        LocalFree(str_ptr.cast::<c_void>());
    }
    Ok(sid_str)
}

fn sddl_to_security_descriptor(sddl: &str) -> Result<Vec<u8>, String> {
    let sddl_w = to_wide_nul(sddl);
    let mut sd_ptr: *mut c_void = ptr::null_mut();
    let mut sd_size: u32 = 0;
    // SAFETY: `sddl_w` is NUL-terminated UTF-16; `sd_ptr` and `sd_size` are valid out-pointers.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd_ptr,
            &mut sd_size,
        )
    };
    if ok == 0 || sd_ptr.is_null() || sd_size == 0 {
        return Err(format!(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW failed: {}",
            unsafe { GetLastError() }
        ));
    }
    // SAFETY: `sd_ptr` points to `sd_size` bytes allocated by LocalAlloc.
    let bytes =
        unsafe { std::slice::from_raw_parts(sd_ptr.cast::<u8>(), sd_size as usize).to_vec() };
    unsafe {
        LocalFree(sd_ptr);
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Safe backend trait & callback trampolines
// ---------------------------------------------------------------------------

/// Volume metadata returned by [`WinFspBackend::get_volume_info`].
#[derive(Debug, Clone)]
pub struct VolumeInfoData {
    /// Total virtual volume size in bytes.
    pub total_size: u64,
    /// Free virtual volume size in bytes (`0` for read-only projections).
    pub free_size: u64,
    /// Volume label (truncated to 32 UTF-16 code units).
    pub label: String,
}

/// Result of [`WinFspBackend::get_security_by_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupByNameResult {
    /// Target path exists with the given Windows `FILE_ATTRIBUTE_*` flags.
    Found {
        /// Windows `FILE_ATTRIBUTE_*` mask for the path.
        file_attributes: u32,
    },
    /// An intermediate path component before the final component is a symlink
    /// reparse point; WinFsp should return `STATUS_REPARSE`.
    IntermediateReparse,
    /// Path does not exist (`STATUS_OBJECT_NAME_NOT_FOUND` or
    /// `STATUS_OBJECT_PATH_NOT_FOUND`).
    NotFound(NtStatus),
}

/// Newly opened handle and metadata returned by [`WinFspBackend::open`].
#[derive(Debug, Clone, Copy)]
pub struct OpenedFile {
    /// Adapter-assigned handle identifier passed to subsequent operations.
    pub handle_id: u64,
    /// Initial `FSP_FSCTL_FILE_INFO` for the opened object.
    pub file_info: FspFsctlFileInfo,
}

/// Single directory entry returned by [`WinFspBackend::read_directory`] or
/// [`WinFspBackend::get_dir_info_by_name`].
#[derive(Debug, Clone)]
pub struct DirEntryData {
    /// Entry name in UTF-16LE without a trailing NUL.
    pub name_utf16: Vec<u16>,
    /// Entry metadata.
    pub file_info: FspFsctlFileInfo,
}

/// Safe trait implemented by `ProjgitWinFsp<F>` in `adapter.rs` and bridged
/// to WinFsp's C callback table by this module.
pub trait WinFspBackend: Send + Sync + 'static {
    /// Self-relative `SECURITY_DESCRIPTOR` bytes for all projected entries.
    fn security_descriptor(&self) -> &[u8];
    /// Query volume capacity and label.
    fn get_volume_info(&self) -> VolumeInfoData;
    /// Resolve `path` (Windows `\`-separated path starting with `\`) for
    /// `GetSecurityByName`.
    fn get_security_by_name(&self, path: &str) -> LookupByNameResult;
    /// Open `path` with the given NT `create_options` and `granted_access`.
    fn open(
        &self,
        path: &str,
        create_options: u32,
        granted_access: u32,
    ) -> Result<OpenedFile, NtStatus>;
    /// Close an open handle previously returned by [`Self::open`].
    fn close(&self, handle_id: u64);
    /// Query `FSP_FSCTL_FILE_INFO` for an open handle.
    fn get_file_info(&self, handle_id: u64) -> Result<FspFsctlFileInfo, NtStatus>;
    /// Read up to `out.len()` bytes starting at `offset` from an open file handle.
    fn read(&self, handle_id: u64, offset: u64, out: &mut [u8]) -> Result<usize, NtStatus>;
    /// Read all entries for an open directory handle.
    fn read_directory(&self, handle_id: u64) -> Result<Vec<DirEntryData>, NtStatus>;
    /// Look up a single child entry `name` inside an open directory handle.
    fn get_dir_info_by_name(
        &self,
        parent_handle_id: u64,
        name: &str,
    ) -> Result<DirEntryData, NtStatus>;
    /// Return the `REPARSE_DATA_BUFFER` for a symlink at `path`.
    fn get_reparse_point_by_name(&self, path: &str) -> Result<Vec<u8>, NtStatus>;
    /// Return the `REPARSE_DATA_BUFFER` for an open symlink handle.
    fn get_reparse_point(&self, handle_id: u64) -> Result<Vec<u8>, NtStatus>;
}

struct HostContext {
    backend: std::sync::Arc<dyn WinFspBackend>,
    api: &'static WinFspApi,
}

struct OpenHandleBox {
    handle_id: u64,
    dir_buffer: *mut c_void,
}

unsafe fn host_from_fs<'a>(fs: *mut FspFileSystem) -> &'a HostContext {
    &*((*fs).user_context.cast::<HostContext>())
}

unsafe extern "system" fn cb_get_volume_info(
    fs: *mut FspFileSystem,
    volume_info: *mut FspFsctlVolumeInfo,
) -> NtStatus {
    if fs.is_null() || volume_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let data = host.backend.get_volume_info();
    let out = &mut *volume_info;
    *out = FspFsctlVolumeInfo {
        total_size: data.total_size,
        free_size: data.free_size,
        volume_label_length: 0,
        volume_label: [0u16; 32],
    };
    let units: Vec<u16> = data.label.encode_utf16().collect();
    let n = units.len().min(out.volume_label.len());
    out.volume_label[..n].copy_from_slice(&units[..n]);
    out.volume_label_length = (n * 2) as u16;
    STATUS_SUCCESS
}

unsafe extern "system" fn cb_set_volume_label(
    _fs: *mut FspFileSystem,
    _volume_label: *mut u16,
    _volume_info: *mut FspFsctlVolumeInfo,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_get_reparse_point_by_name(
    fs: *mut FspFileSystem,
    _context: *mut c_void,
    file_name: *mut u16,
    _is_directory: u8,
    buffer: *mut c_void,
    p_size: *mut usize,
) -> NtStatus {
    if fs.is_null() || file_name.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let path = pwstr_to_string(file_name);
    let reparse = match host.backend.get_reparse_point_by_name(&path) {
        Ok(bytes) => bytes,
        Err(status) => return status,
    };
    if !buffer.is_null() {
        if p_size.is_null() || reparse.len() > *p_size {
            return STATUS_BUFFER_TOO_SMALL;
        }
        *p_size = reparse.len();
        ptr::copy_nonoverlapping(reparse.as_ptr(), buffer.cast::<u8>(), reparse.len());
    }
    STATUS_SUCCESS
}

unsafe extern "system" fn cb_get_security_by_name(
    fs: *mut FspFileSystem,
    file_name: *mut u16,
    p_file_attributes: *mut u32,
    security_descriptor: *mut c_void,
    p_security_descriptor_size: *mut usize,
) -> NtStatus {
    if fs.is_null() || file_name.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let path = pwstr_to_string(file_name);
    match host.backend.get_security_by_name(&path) {
        LookupByNameResult::Found { file_attributes } => {
            if !p_file_attributes.is_null() {
                *p_file_attributes = file_attributes;
            }
            if !p_security_descriptor_size.is_null() {
                let sd = host.backend.security_descriptor();
                if sd.len() > *p_security_descriptor_size {
                    *p_security_descriptor_size = sd.len();
                    return STATUS_BUFFER_OVERFLOW;
                }
                *p_security_descriptor_size = sd.len();
                if !security_descriptor.is_null() && !sd.is_empty() {
                    ptr::copy_nonoverlapping(
                        sd.as_ptr(),
                        security_descriptor.cast::<u8>(),
                        sd.len(),
                    );
                }
            }
            STATUS_SUCCESS
        }
        LookupByNameResult::IntermediateReparse => {
            if (host.api.fsp_file_system_find_reparse_point)(
                fs,
                cb_get_reparse_point_by_name,
                ptr::null_mut(),
                file_name,
                p_file_attributes,
            ) != 0
            {
                STATUS_REPARSE
            } else {
                STATUS_OBJECT_NAME_NOT_FOUND
            }
        }
        LookupByNameResult::NotFound(status) => status,
    }
}

unsafe extern "system" fn cb_create(
    _fs: *mut FspFileSystem,
    _file_name: *mut u16,
    _create_options: u32,
    _granted_access: u32,
    _file_attributes: u32,
    _security_descriptor: *mut c_void,
    _allocation_size: u64,
    _p_file_context: *mut *mut c_void,
    _file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_open(
    fs: *mut FspFileSystem,
    file_name: *mut u16,
    create_options: u32,
    granted_access: u32,
    p_file_context: *mut *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    if fs.is_null() || file_name.is_null() || p_file_context.is_null() || file_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let path = pwstr_to_string(file_name);
    match host.backend.open(&path, create_options, granted_access) {
        Ok(opened) => {
            let boxed = Box::new(OpenHandleBox {
                handle_id: opened.handle_id,
                dir_buffer: ptr::null_mut(),
            });
            *p_file_context = Box::into_raw(boxed).cast::<c_void>();
            *file_info = opened.file_info;
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

unsafe extern "system" fn cb_overwrite(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _file_attributes: u32,
    _replace_file_attributes: u8,
    _allocation_size: u64,
    _file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_cleanup(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _flags: u32,
) {
}

unsafe extern "system" fn cb_close(fs: *mut FspFileSystem, file_context: *mut c_void) {
    if fs.is_null() || file_context.is_null() {
        return;
    }
    let host = host_from_fs(fs);
    let mut boxed = Box::from_raw(file_context.cast::<OpenHandleBox>());
    if !boxed.dir_buffer.is_null() {
        (host.api.fsp_file_system_delete_directory_buffer)(&mut boxed.dir_buffer);
    }
    host.backend.close(boxed.handle_id);
}

unsafe extern "system" fn cb_read(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    p_bytes_transferred: *mut u32,
) -> NtStatus {
    if fs.is_null() || file_context.is_null() || buffer.is_null() || p_bytes_transferred.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    *p_bytes_transferred = 0;
    if length == 0 {
        return STATUS_SUCCESS;
    }
    let host = host_from_fs(fs);
    let handle = &*(file_context.cast::<OpenHandleBox>());
    let out = std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize);
    match host.backend.read(handle.handle_id, offset, out) {
        Ok(0) => STATUS_END_OF_FILE,
        Ok(n) => {
            *p_bytes_transferred = n as u32;
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

unsafe extern "system" fn cb_write(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _buffer: *mut c_void,
    _offset: u64,
    _length: u32,
    _write_to_end_of_file: u8,
    _constrained_io: u8,
    _p_bytes_transferred: *mut u32,
    _file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_flush(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    if !file_context.is_null() && !file_info.is_null() && !fs.is_null() {
        let host = host_from_fs(fs);
        let handle = &*(file_context.cast::<OpenHandleBox>());
        if let Ok(info) = host.backend.get_file_info(handle.handle_id) {
            *file_info = info;
        }
    }
    STATUS_SUCCESS
}

unsafe extern "system" fn cb_get_file_info(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    if fs.is_null() || file_context.is_null() || file_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let handle = &*(file_context.cast::<OpenHandleBox>());
    match host.backend.get_file_info(handle.handle_id) {
        Ok(info) => {
            *file_info = info;
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

unsafe extern "system" fn cb_set_basic_info(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_attributes: u32,
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
    file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    // Windows callers occasionally issue a no-op FileBasicInfo with all
    // fields zeroed on read-only handles.
    if file_attributes == 0
        && creation_time == 0
        && last_access_time == 0
        && last_write_time == 0
        && change_time == 0
    {
        return cb_get_file_info(fs, file_context, file_info);
    }
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_set_file_size(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _new_size: u64,
    _set_allocation_size: u8,
    _file_info: *mut FspFsctlFileInfo,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_can_delete(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _file_name: *mut u16,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_rename(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _new_file_name: *mut u16,
    _replace_if_exists: u8,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_get_security(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    security_descriptor: *mut c_void,
    p_security_descriptor_size: *mut usize,
) -> NtStatus {
    if fs.is_null() || file_context.is_null() || p_security_descriptor_size.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let sd = host.backend.security_descriptor();
    if sd.len() > *p_security_descriptor_size {
        *p_security_descriptor_size = sd.len();
        return STATUS_BUFFER_OVERFLOW;
    }
    *p_security_descriptor_size = sd.len();
    if !security_descriptor.is_null() && !sd.is_empty() {
        ptr::copy_nonoverlapping(sd.as_ptr(), security_descriptor.cast::<u8>(), sd.len());
    }
    STATUS_SUCCESS
}

unsafe extern "system" fn cb_set_security(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _security_information: u32,
    _modification_descriptor: *mut c_void,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_read_directory(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _pattern: *mut u16,
    marker: *mut u16,
    buffer: *mut c_void,
    length: u32,
    p_bytes_transferred: *mut u32,
) -> NtStatus {
    if fs.is_null() || file_context.is_null() || buffer.is_null() || p_bytes_transferred.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let handle = &mut *(file_context.cast::<OpenHandleBox>());
    let mut dir_result: NtStatus = STATUS_SUCCESS;

    if (host.api.fsp_file_system_acquire_directory_buffer)(
        &mut handle.dir_buffer,
        u8::from(marker.is_null()),
        &mut dir_result,
    ) != 0
    {
        match host.backend.read_directory(handle.handle_id) {
            Ok(entries) => {
                let mut raw_buf = vec![0u8; std::mem::size_of::<FspFsctlDirInfoHeader>() + 512];
                for entry in entries {
                    let name_byte_len = entry.name_utf16.len() * 2;
                    let total_size = std::mem::size_of::<FspFsctlDirInfoHeader>() + name_byte_len;
                    if total_size > u16::MAX as usize {
                        continue;
                    }
                    if raw_buf.len() < total_size {
                        raw_buf.resize(total_size, 0);
                    } else {
                        raw_buf[..total_size].fill(0);
                    }
                    let hdr = &mut *(raw_buf.as_mut_ptr().cast::<FspFsctlDirInfoHeader>());
                    hdr.size = total_size as u16;
                    hdr.file_info = entry.file_info;
                    let name_dst = raw_buf
                        .as_mut_ptr()
                        .add(std::mem::size_of::<FspFsctlDirInfoHeader>())
                        .cast::<u16>();
                    ptr::copy_nonoverlapping(
                        entry.name_utf16.as_ptr(),
                        name_dst,
                        entry.name_utf16.len(),
                    );
                    if (host.api.fsp_file_system_fill_directory_buffer)(
                        &mut handle.dir_buffer,
                        raw_buf.as_mut_ptr().cast::<FspFsctlDirInfoHeader>(),
                        &mut dir_result,
                    ) == 0
                    {
                        break;
                    }
                }
            }
            Err(status) => {
                dir_result = status;
            }
        }
        (host.api.fsp_file_system_release_directory_buffer)(&mut handle.dir_buffer);
    }

    if !nt_success(dir_result) {
        return dir_result;
    }

    (host.api.fsp_file_system_read_directory_buffer)(
        &mut handle.dir_buffer,
        marker,
        buffer,
        length,
        p_bytes_transferred,
    );
    STATUS_SUCCESS
}

unsafe extern "system" fn cb_get_dir_info_by_name(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_name: *mut u16,
    dir_info: *mut FspFsctlDirInfoHeader,
) -> NtStatus {
    if fs.is_null() || file_context.is_null() || file_name.is_null() || dir_info.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let handle = &*(file_context.cast::<OpenHandleBox>());
    let name = pwstr_to_string(file_name);
    match host.backend.get_dir_info_by_name(handle.handle_id, &name) {
        Ok(entry) => {
            let name_bytes = entry.name_utf16.len() * 2;
            let total_size = std::mem::size_of::<FspFsctlDirInfoHeader>() + name_bytes;
            if total_size > u16::MAX as usize {
                return STATUS_OBJECT_NAME_INVALID;
            }
            let out = &mut *dir_info;
            out.size = total_size as u16;
            out._pad0 = [0; 3];
            out.file_info = entry.file_info;
            out.padding = [0; 24];
            let dst = (dir_info.cast::<u8>())
                .add(std::mem::size_of::<FspFsctlDirInfoHeader>())
                .cast::<u16>();
            ptr::copy_nonoverlapping(entry.name_utf16.as_ptr(), dst, entry.name_utf16.len());
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

unsafe extern "system" fn cb_resolve_reparse_points(
    fs: *mut FspFileSystem,
    file_name: *mut u16,
    reparse_point_index: u32,
    resolve_last_path_component: u8,
    p_io_status: *mut IoStatusBlock,
    buffer: *mut c_void,
    p_size: *mut usize,
) -> NtStatus {
    if fs.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    (host.api.fsp_file_system_resolve_reparse_points)(
        fs,
        cb_get_reparse_point_by_name,
        ptr::null_mut(),
        file_name,
        reparse_point_index,
        resolve_last_path_component,
        p_io_status,
        buffer,
        p_size,
    )
}

unsafe extern "system" fn cb_get_reparse_point(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_name: *mut u16,
    buffer: *mut c_void,
    p_size: *mut usize,
) -> NtStatus {
    if fs.is_null() || file_context.is_null() || buffer.is_null() || p_size.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let host = host_from_fs(fs);
    let handle = &*(file_context.cast::<OpenHandleBox>());
    match host.backend.get_reparse_point(handle.handle_id) {
        Ok(reparse) => {
            if reparse.len() > *p_size {
                return STATUS_BUFFER_TOO_SMALL;
            }
            *p_size = reparse.len();
            ptr::copy_nonoverlapping(reparse.as_ptr(), buffer.cast::<u8>(), reparse.len());
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

unsafe extern "system" fn cb_set_reparse_point(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _buffer: *mut c_void,
    _size: usize,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_delete_reparse_point(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _buffer: *mut c_void,
    _size: usize,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_set_delete(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _file_name: *mut u16,
    _delete_file: u8,
) -> NtStatus {
    STATUS_MEDIA_WRITE_PROTECTED
}

/// `'static` interface table shared by all `RawWinFspMount` instances.
///
/// Storing `FSP_FILE_SYSTEM_INTERFACE` in `'static` memory is critical:
/// `FspFileSystemCreate` saves the raw pointer in `FileSystem->Interface`
/// without copying the struct.
static PROJGIT_WINFSP_INTERFACE: FspFileSystemInterface = FspFileSystemInterface {
    get_volume_info: Some(cb_get_volume_info),
    set_volume_label: Some(cb_set_volume_label),
    get_security_by_name: Some(cb_get_security_by_name),
    create: Some(cb_create),
    open: Some(cb_open),
    overwrite: Some(cb_overwrite),
    cleanup: Some(cb_cleanup),
    close: Some(cb_close),
    read: Some(cb_read),
    write: Some(cb_write),
    flush: Some(cb_flush),
    get_file_info: Some(cb_get_file_info),
    set_basic_info: Some(cb_set_basic_info),
    set_file_size: Some(cb_set_file_size),
    can_delete: Some(cb_can_delete),
    rename: Some(cb_rename),
    get_security: Some(cb_get_security),
    set_security: Some(cb_set_security),
    read_directory: Some(cb_read_directory),
    resolve_reparse_points: Some(cb_resolve_reparse_points),
    get_reparse_point: Some(cb_get_reparse_point),
    set_reparse_point: Some(cb_set_reparse_point),
    delete_reparse_point: Some(cb_delete_reparse_point),
    get_stream_info: 0,
    get_dir_info_by_name: Some(cb_get_dir_info_by_name),
    control: 0,
    set_delete: Some(cb_set_delete),
    create_ex: 0,
    overwrite_ex: 0,
    get_ea: 0,
    set_ea: 0,
    obsolete0: 0,
    dispatcher_stopped: 0,
    reserved: [0; 31],
};

// ---------------------------------------------------------------------------
// RAII mount session wrapper
// ---------------------------------------------------------------------------

/// Active WinFsp dispatcher and mount point handle.
///
/// Dropping this struct stops the WinFsp dispatcher threads, removes the
/// mount point (drive letter or directory reparse point), deletes the
/// `FSP_FILE_SYSTEM` object, and drops the Rust [`WinFspBackend`].
pub struct RawWinFspMount {
    fs: *mut FspFileSystem,
    host: *mut HostContext,
    api: &'static WinFspApi,
    mountpoint: PathBuf,
    recreate_empty_dir_on_drop: bool,
}

// SAFETY: `FSP_FILE_SYSTEM` is thread-safe and designed to be stopped/unmounted
// from a control thread while worker threads service IRPs.
unsafe impl Send for RawWinFspMount {}
unsafe impl Sync for RawWinFspMount {}

impl RawWinFspMount {
    /// Create, mount, and start the WinFsp dispatcher for `backend` at
    /// `mountpoint`.
    pub fn start(
        backend: std::sync::Arc<dyn WinFspBackend>,
        mountpoint: &Path,
        fs_name: &str,
        n_threads: u32,
        reparse_points_enabled: bool,
    ) -> Result<Self, String> {
        let api = WinFspApi::get()?;

        // WinFsp directory mounts create the target junction themselves and
        // fail with STATUS_OBJECT_NAME_COLLISION if an empty directory already
        // exists at `mountpoint`. To match FUSE ergonomics (where callers
        // pre-create an empty mount directory), if `mountpoint` is an existing
        // empty directory (and not a drive root like `Z:` or `Z:\`), remove it
        // before calling `FspFileSystemSetMountPoint` and recreate it on Drop.
        let mp_str = mountpoint.to_string_lossy();
        let is_drive = is_drive_mountpoint(&mp_str);
        let mut recreate_empty_dir_on_drop = false;
        if !is_drive && mountpoint.exists() {
            if !mountpoint.is_dir() {
                return Err(format!(
                    "mountpoint {} exists and is not a directory",
                    mountpoint.display()
                ));
            }
            let mut entries = std::fs::read_dir(mountpoint).map_err(|e| {
                format!("reading mountpoint directory {}: {e}", mountpoint.display())
            })?;
            if entries.next().is_some() {
                return Err(format!(
                    "mountpoint directory {} is not empty",
                    mountpoint.display()
                ));
            }
            std::fs::remove_dir(mountpoint).map_err(|e| {
                format!(
                    "preparing empty directory mountpoint {}: {e}",
                    mountpoint.display()
                )
            })?;
            recreate_empty_dir_on_drop = true;
        }

        let now_filetime = unix_secs_to_filetime(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(1_700_000_000),
            0,
        );

        let mut flags = VOL_FLAG_CASE_PRESERVED_NAMES
            | VOL_FLAG_UNICODE_ON_DISK
            | VOL_FLAG_PERSISTENT_ACLS
            | VOL_FLAG_READ_ONLY_VOLUME
            | VOL_FLAG_POST_CLEANUP_WHEN_MODIFIED_ONLY
            | VOL_FLAG_PASS_QUERY_DIRECTORY_FILE_NAME
            | VOL_FLAG_UM_FILE_CONTEXT_IS_USER_CONTEXT2
            | VOL_FLAG_ALLOW_OPEN_IN_KERNEL_MODE
            | VOL_FLAG_WSL_FEATURES
            | VOL_FLAG_SUPPORTS_POSIX_UNLINK_RENAME
            | VOL_FLAG_POST_DISPOSITION_WHEN_NECESSARY_ONLY;
        if reparse_points_enabled {
            flags |= VOL_FLAG_REPARSE_POINTS;
        }

        let mut params = FspFsctlVolumeParams {
            version: std::mem::size_of::<FspFsctlVolumeParams>() as u16,
            sector_size: 4096,
            sectors_per_allocation_unit: 1,
            max_component_length: 255,
            volume_creation_time: now_filetime,
            volume_serial_number: (now_filetime / 10_000_000) as u32,
            transact_timeout: 0,
            irp_timeout: 0,
            irp_capacity: 0,
            file_info_timeout: 1_000,
            flags,
            prefix: [0; 192],
            file_system_name: [0; 16],
            v1_flags: 0,
            volume_info_timeout: 0,
            dir_info_timeout: 0,
            security_timeout: 0,
            stream_info_timeout: 0,
            ea_timeout: 0,
            fsext_control_code: 0,
            reserved32: [0; 1],
            reserved64: [0; 2],
        };

        let name_str = if fs_name.is_empty() {
            "projgit"
        } else {
            fs_name
        };
        let fs_name_w: Vec<u16> = name_str.encode_utf16().collect();
        let n = fs_name_w.len().min(params.file_system_name.len() - 1);
        params.file_system_name[..n].copy_from_slice(&fs_name_w[..n]);

        let host_box = Box::new(HostContext { backend, api });
        let host_ptr = Box::into_raw(host_box);

        let device_w = to_wide_nul("WinFsp.Disk");
        let mut fs_ptr: *mut FspFileSystem = ptr::null_mut();

        // SAFETY: `device_w`, `params`, and `PROJGIT_WINFSP_INTERFACE` are valid,
        // and `PROJGIT_WINFSP_INTERFACE` has `'static` lifetime.
        let status = unsafe {
            (api.fsp_file_system_create)(
                device_w.as_ptr(),
                &params,
                &PROJGIT_WINFSP_INTERFACE,
                &mut fs_ptr,
            )
        };
        if !nt_success(status) || fs_ptr.is_null() {
            unsafe {
                drop(Box::from_raw(host_ptr));
            }
            if recreate_empty_dir_on_drop {
                let _ = std::fs::create_dir_all(mountpoint);
            }
            return Err(format!(
                "FspFileSystemCreate failed with NTSTATUS 0x{status:08X}"
            ));
        }

        // SAFETY: `fs_ptr` was just created by `FspFileSystemCreate`.
        unsafe {
            (*fs_ptr).user_context = host_ptr.cast::<c_void>();
        }

        let normalized_mp = normalize_mountpoint_arg(&mp_str);
        let mp_w = to_wide_nul(&normalized_mp);
        let mp_arg = if normalized_mp == "*" {
            ptr::null()
        } else {
            mp_w.as_ptr()
        };

        // SAFETY: `fs_ptr` is valid and `mp_arg` is either null (`*`) or a
        // NUL-terminated UTF-16 path.
        let status = unsafe { (api.fsp_file_system_set_mount_point)(fs_ptr, mp_arg) };
        if !nt_success(status) {
            unsafe {
                (api.fsp_file_system_delete)(fs_ptr);
                drop(Box::from_raw(host_ptr));
            }
            if recreate_empty_dir_on_drop {
                let _ = std::fs::create_dir_all(mountpoint);
            }
            return Err(format!(
                "FspFileSystemSetMountPoint({}) failed with NTSTATUS 0x{status:08X}",
                mountpoint.display()
            ));
        }

        // SAFETY: `fs_ptr` has its mount point configured and `user_context` set.
        let status = unsafe { (api.fsp_file_system_start_dispatcher)(fs_ptr, n_threads) };
        if !nt_success(status) {
            unsafe {
                (api.fsp_file_system_remove_mount_point)(fs_ptr);
                (api.fsp_file_system_delete)(fs_ptr);
                drop(Box::from_raw(host_ptr));
            }
            if recreate_empty_dir_on_drop {
                let _ = std::fs::create_dir_all(mountpoint);
            }
            return Err(format!(
                "FspFileSystemStartDispatcher failed with NTSTATUS 0x{status:08X}"
            ));
        }

        // Read back the actual mount point assigned by WinFsp (in case `*` was used).
        let actual_mp = unsafe {
            let mp_ptr = (*fs_ptr).mount_point;
            if mp_ptr.is_null() {
                mountpoint.to_path_buf()
            } else {
                PathBuf::from(pwstr_to_string(mp_ptr))
            }
        };

        Ok(Self {
            fs: fs_ptr,
            host: host_ptr,
            api,
            mountpoint: actual_mp,
            recreate_empty_dir_on_drop,
        })
    }

    /// Mount point path or drive letter where the filesystem is mounted.
    #[must_use]
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for RawWinFspMount {
    fn drop(&mut self) {
        if !self.fs.is_null() {
            // SAFETY: `self.fs` is a live `FspFileSystem` started by `Self::start`.
            unsafe {
                (self.api.fsp_file_system_stop_dispatcher)(self.fs);
                (self.api.fsp_file_system_remove_mount_point)(self.fs);
                (self.api.fsp_file_system_delete)(self.fs);
            }
            self.fs = ptr::null_mut();
        }
        if !self.host.is_null() {
            // SAFETY: `self.host` was allocated by `Box::into_raw` in `Self::start`
            // and is no longer referenced after `FspFileSystemStopDispatcher` and
            // `FspFileSystemDelete` complete.
            unsafe {
                drop(Box::from_raw(self.host));
            }
            self.host = ptr::null_mut();
        }
        if self.recreate_empty_dir_on_drop && !self.mountpoint.exists() {
            let _ = std::fs::create_dir_all(&self.mountpoint);
        }
    }
}

fn is_drive_mountpoint(s: &str) -> bool {
    if s == "*" {
        return true;
    }
    let b = s.as_bytes();
    (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || (b.len() == 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/'))
}

fn normalize_mountpoint_arg(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return s[..2].to_owned();
    }
    s.trim_end_matches(['\\', '/']).to_owned()
}

/// Convert UNIX timestamp (`secs`, `nsecs`) to Windows FILETIME (100ns ticks
/// since 1601-01-01 UTC).
#[must_use]
pub const fn unix_secs_to_filetime(secs: u64, nsecs: u32) -> u64 {
    secs.saturating_mul(10_000_000)
        .saturating_add((nsecs / 100) as u64)
        .saturating_add(116_444_736_000_000_000)
}
