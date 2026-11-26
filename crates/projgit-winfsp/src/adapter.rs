//! WinFsp adapter over [`projgit_core::FsProvider`].
//!
//! Translates WinFsp filesystem operations (`GetVolumeInfo`,
//! `GetSecurityByName`, `Open`, `Close`, `GetFileInfo`, `Read`,
//! `ReadDirectory`, `GetDirInfoByName`, `GetReparsePointByName`,
//! `GetReparsePoint`) into [`FsProvider`] calls.
//!
//! When [`FsProvider::open`] returns a host backing path (an uncompressed L2
//! blob in `<git-dir>/blobs/<xx>/<oid>` or a host-backed `.git/objects/pack/*`
//! file), [`ProjgitWinFsp`] opens a native `std::fs::File` handle and serves
//! `Read` requests via `FileExt::seek_read` (`ReadFile` with `OVERLAPPED`
//! offset), bypassing userspace buffer allocations on the read path.

use std::collections::HashMap;
use std::fs::File;
use std::os::windows::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use projgit_core::fs_provider::{Attr, FileType, FsError, FsProvider, ROOT_INODE};

use crate::ffi::{
    build_current_user_security_descriptor, unix_secs_to_filetime, DirEntryData, FspFsctlFileInfo,
    LookupByNameResult, NtStatus, OpenedFile, VolumeInfoData, WinFspBackend,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT, STATUS_END_OF_FILE,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_INVALID_HANDLE, STATUS_NOT_A_DIRECTORY,
    STATUS_NOT_A_REPARSE_POINT, STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_UNSUCCESSFUL,
};
use crate::symlinks::{
    build_symlink_reparse_buffer, classify_symlink_target, SymlinkKind, SymlinkMode,
    IO_REPARSE_TAG_SYMLINK,
};

const ALLOCATION_UNIT: u64 = 4096;
const VIRTUAL_VOLUME_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024; // 64 GiB virtual capacity

/// Counters tracking direct host-file reads vs [`FsProvider::read`] fallback
/// calls in [`ProjgitWinFsp`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WinFspReadStats {
    /// Number of `Read` requests served directly from a host backing file
    /// (`<git-dir>/blobs/<xx>/<oid>` or `.git/objects/pack/*`) via
    /// `FileExt::seek_read`.
    pub host_file_reads: u64,
    /// Number of `Read` requests served via [`FsProvider::read`].
    pub provider_reads: u64,
}

#[derive(Debug, Clone)]
struct CachedSymlink {
    kind: SymlinkKind,
    target: Vec<u8>,
    reparse_buffer: Option<Vec<u8>>,
}

enum HandleKind {
    Directory {
        parent_ino: u64,
        parent_chain: Vec<u64>,
    },
    RegularFile {
        backing_file: Option<Arc<File>>,
    },
    Symlink {
        target: Vec<u8>,
        reparse_buffer: Option<Vec<u8>>,
    },
}

struct HandleState {
    ino: u64,
    file_info: FspFsctlFileInfo,
    kind: HandleKind,
}

enum PathResolution {
    Resolved {
        ino: u64,
        attr: Attr,
        parent_chain: Vec<u64>,
    },
    IntermediateReparse,
    NotFound(NtStatus),
}

/// WinFsp adapter wrapping an [`FsProvider`].
pub struct ProjgitWinFsp<P> {
    provider: P,
    security_descriptor: Vec<u8>,
    symlink_mode: SymlinkMode,
    volume_label: String,
    next_handle_id: AtomicU64,
    handles: Mutex<HashMap<u64, HandleState>>,
    symlink_cache: Mutex<HashMap<u64, CachedSymlink>>,
    host_file_reads: AtomicU64,
    provider_reads: AtomicU64,
}

impl<P: FsProvider> ProjgitWinFsp<P> {
    /// Construct a new [`ProjgitWinFsp`] adapter around `provider`.
    pub fn new(
        provider: P,
        symlink_mode: SymlinkMode,
        volume_label: impl Into<String>,
    ) -> Result<Self, String> {
        let security_descriptor = build_current_user_security_descriptor()?;
        Ok(Self {
            provider,
            security_descriptor,
            symlink_mode,
            volume_label: volume_label.into(),
            next_handle_id: AtomicU64::new(1),
            handles: Mutex::new(HashMap::new()),
            symlink_cache: Mutex::new(HashMap::new()),
            host_file_reads: AtomicU64::new(0),
            provider_reads: AtomicU64::new(0),
        })
    }

    /// Borrow the underlying [`FsProvider`].
    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Snapshot of direct host-file vs provider read counters.
    #[must_use]
    pub fn read_stats(&self) -> WinFspReadStats {
        WinFspReadStats {
            host_file_reads: self.host_file_reads.load(Ordering::Relaxed),
            provider_reads: self.provider_reads.load(Ordering::Relaxed),
        }
    }

    fn inspect_symlink(&self, ino: u64, parent_chain: &[u64]) -> Result<CachedSymlink, NtStatus> {
        if let Ok(guard) = self.symlink_cache.lock() {
            if let Some(cached) = guard.get(&ino) {
                return Ok(cached.clone());
            }
        }
        let target_bstr = self
            .provider
            .readlink(ino)
            .map_err(|e| fs_error_to_ntstatus(&e))?;
        let target = target_bstr.to_vec();
        let kind = classify_symlink_target(&self.provider, parent_chain, &target);
        let reparse_buffer = build_symlink_reparse_buffer(&target).ok();
        let entry = CachedSymlink {
            kind,
            target,
            reparse_buffer,
        };
        if let Ok(mut guard) = self.symlink_cache.lock() {
            guard.insert(ino, entry.clone());
        }
        Ok(entry)
    }

    fn attr_to_file_info(&self, attr: &Attr, parent_chain: &[u64]) -> FspFsctlFileInfo {
        let (file_attributes, reparse_tag, file_size) = match attr.kind {
            FileType::Directory => (FILE_ATTRIBUTE_DIRECTORY, 0, 0),
            FileType::RegularFile => (FILE_ATTRIBUTE_READONLY, 0, attr.size),
            FileType::Symlink => match self.symlink_mode {
                SymlinkMode::NativeReparse => {
                    let is_dir = self
                        .inspect_symlink(attr.inode, parent_chain)
                        .map(|s| s.kind.is_directory())
                        .unwrap_or(false);
                    let mut attrs = FILE_ATTRIBUTE_REPARSE_POINT;
                    if is_dir {
                        attrs |= FILE_ATTRIBUTE_DIRECTORY;
                    } else {
                        attrs |= FILE_ATTRIBUTE_READONLY;
                    }
                    (attrs, IO_REPARSE_TAG_SYMLINK, 0)
                }
                SymlinkMode::PlainFile => {
                    let len = self
                        .inspect_symlink(attr.inode, parent_chain)
                        .map(|s| s.target.len() as u64)
                        .unwrap_or(attr.size);
                    (FILE_ATTRIBUTE_READONLY, 0, len)
                }
            },
        };

        let allocation_size = if file_size == 0 {
            0
        } else {
            file_size.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT
        };
        let (secs, nsecs) = attr
            .mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        let ft = unix_secs_to_filetime(secs, nsecs);

        FspFsctlFileInfo {
            file_attributes,
            reparse_tag,
            allocation_size,
            file_size,
            creation_time: ft,
            last_access_time: ft,
            last_write_time: ft,
            change_time: ft,
            index_number: attr.inode,
            hard_links: 0,
            ea_size: 0,
        }
    }

    fn resolve_path(&self, path: &str) -> PathResolution {
        // Reject NTFS Alternate Data Stream probes (`file.txt:stream`).
        if path.contains(':') {
            return PathResolution::NotFound(STATUS_OBJECT_NAME_INVALID);
        }

        let components: Vec<&str> = path.split(['\\', '/']).filter(|c| !c.is_empty()).collect();

        if components.is_empty() {
            return match self.provider.getattr(ROOT_INODE) {
                Ok(attr) => PathResolution::Resolved {
                    ino: ROOT_INODE,
                    attr,
                    parent_chain: vec![ROOT_INODE],
                },
                Err(e) => PathResolution::NotFound(fs_error_to_ntstatus(&e)),
            };
        }

        let mut parent_chain = vec![ROOT_INODE];
        for (idx, &comp) in components.iter().enumerate() {
            let is_last = idx + 1 == components.len();
            let current_dir = *parent_chain.last().unwrap_or(&ROOT_INODE);

            match self.provider.lookup(current_dir, comp.as_bytes()) {
                Ok(attr) => {
                    if is_last {
                        return PathResolution::Resolved {
                            ino: attr.inode,
                            attr,
                            parent_chain,
                        };
                    }
                    match attr.kind {
                        FileType::Directory => {
                            parent_chain.push(attr.inode);
                        }
                        FileType::Symlink if self.symlink_mode == SymlinkMode::NativeReparse => {
                            return PathResolution::IntermediateReparse;
                        }
                        FileType::RegularFile | FileType::Symlink => {
                            return PathResolution::NotFound(STATUS_OBJECT_PATH_NOT_FOUND);
                        }
                    }
                }
                Err(FsError::NotFound) => {
                    return if is_last {
                        PathResolution::NotFound(STATUS_OBJECT_NAME_NOT_FOUND)
                    } else {
                        PathResolution::NotFound(STATUS_OBJECT_PATH_NOT_FOUND)
                    };
                }
                Err(e) => return PathResolution::NotFound(fs_error_to_ntstatus(&e)),
            }
        }

        PathResolution::NotFound(STATUS_OBJECT_NAME_NOT_FOUND)
    }
}

/// Map a [`projgit_core::fs_provider::FsError`] to its corresponding Windows
/// `NTSTATUS`.
#[must_use]
pub const fn fs_error_to_ntstatus(err: &FsError) -> NtStatus {
    match err {
        FsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        FsError::NotADirectory => STATUS_NOT_A_DIRECTORY,
        FsError::NotAFile => STATUS_FILE_IS_A_DIRECTORY,
        FsError::NotASymlink => STATUS_NOT_A_REPARSE_POINT,
        FsError::Unsupported => STATUS_NOT_SUPPORTED,
        FsError::Io(_) => STATUS_UNSUCCESSFUL,
    }
}

impl<P: FsProvider + 'static> WinFspBackend for ProjgitWinFsp<P> {
    fn security_descriptor(&self) -> &[u8] {
        &self.security_descriptor
    }

    fn get_volume_info(&self) -> VolumeInfoData {
        VolumeInfoData {
            total_size: VIRTUAL_VOLUME_TOTAL_BYTES,
            free_size: 0,
            label: self.volume_label.clone(),
        }
    }

    fn get_security_by_name(&self, path: &str) -> LookupByNameResult {
        match self.resolve_path(path) {
            PathResolution::Resolved {
                attr, parent_chain, ..
            } => {
                let info = self.attr_to_file_info(&attr, &parent_chain);
                LookupByNameResult::Found {
                    file_attributes: info.file_attributes,
                }
            }
            PathResolution::IntermediateReparse => LookupByNameResult::IntermediateReparse,
            PathResolution::NotFound(status) => LookupByNameResult::NotFound(status),
        }
    }

    fn open(
        &self,
        path: &str,
        create_options: u32,
        _granted_access: u32,
    ) -> Result<OpenedFile, NtStatus> {
        let (ino, attr, parent_chain) = match self.resolve_path(path) {
            PathResolution::Resolved {
                ino,
                attr,
                parent_chain,
            } => (ino, attr, parent_chain),
            PathResolution::IntermediateReparse => return Err(STATUS_OBJECT_PATH_NOT_FOUND),
            PathResolution::NotFound(status) => return Err(status),
        };

        let file_info = self.attr_to_file_info(&attr, &parent_chain);
        let is_dir_like = (file_info.file_attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
        let open_reparse = (create_options & FILE_OPEN_REPARSE_POINT) != 0;

        if (create_options & FILE_DIRECTORY_FILE) != 0 && !is_dir_like {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        if (create_options & FILE_NON_DIRECTORY_FILE) != 0
            && attr.kind == FileType::Directory
            && !open_reparse
        {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }

        let kind = match attr.kind {
            FileType::Directory => {
                let parent_ino = *parent_chain.last().unwrap_or(&ROOT_INODE);
                let mut child_chain = parent_chain;
                if ino != ROOT_INODE {
                    child_chain.push(ino);
                }
                HandleKind::Directory {
                    parent_ino,
                    parent_chain: child_chain,
                }
            }
            FileType::RegularFile => {
                let backing_file = self.provider.backing_file(ino).ok().flatten();
                HandleKind::RegularFile { backing_file }
            }
            FileType::Symlink => {
                let cached = self.inspect_symlink(ino, &parent_chain)?;
                HandleKind::Symlink {
                    target: cached.target,
                    reparse_buffer: cached.reparse_buffer,
                }
            }
        };

        let handle_id = self.next_handle_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.handles.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        guard.insert(
            handle_id,
            HandleState {
                ino,
                file_info,
                kind,
            },
        );

        Ok(OpenedFile {
            handle_id,
            file_info,
        })
    }

    fn close(&self, handle_id: u64) {
        if let Ok(mut g) = self.handles.lock() {
            g.remove(&handle_id);
        }
    }

    fn get_file_info(&self, handle_id: u64) -> Result<FspFsctlFileInfo, NtStatus> {
        let guard = self.handles.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let state = guard.get(&handle_id).ok_or(STATUS_INVALID_HANDLE)?;
        Ok(state.file_info)
    }

    fn read(&self, handle_id: u64, offset: u64, out: &mut [u8]) -> Result<usize, NtStatus> {
        let (ino, file_size, symlink_target, backing) = {
            let guard = self.handles.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
            let state = guard.get(&handle_id).ok_or(STATUS_INVALID_HANDLE)?;
            match &state.kind {
                HandleKind::RegularFile { backing_file } => (
                    state.ino,
                    state.file_info.file_size,
                    None,
                    backing_file.clone(),
                ),
                HandleKind::Symlink { target, .. } => {
                    (state.ino, target.len() as u64, Some(target.clone()), None)
                }
                HandleKind::Directory { .. } => return Err(STATUS_FILE_IS_A_DIRECTORY),
            }
        };

        if offset >= file_size {
            return Err(STATUS_END_OF_FILE);
        }

        let max_len = (file_size - offset).min(out.len() as u64) as usize;
        if max_len == 0 {
            return Err(STATUS_END_OF_FILE);
        }

        if let Some(target_bytes) = symlink_target {
            let start = offset as usize;
            let end = (start + max_len).min(target_bytes.len());
            let n = end.saturating_sub(start);
            if n == 0 {
                return Err(STATUS_END_OF_FILE);
            }
            out[..n].copy_from_slice(&target_bytes[start..end]);
            return Ok(n);
        }

        if let Some(file) = backing {
            if let Ok(n) = file.seek_read(&mut out[..max_len], offset) {
                self.host_file_reads.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    return Err(STATUS_END_OF_FILE);
                }
                return Ok(n);
            }
        }

        self.provider_reads.fetch_add(1, Ordering::Relaxed);
        let data = self
            .provider
            .read(ino, offset, max_len as u32)
            .map_err(|e| fs_error_to_ntstatus(&e))?;
        if data.is_empty() {
            return Err(STATUS_END_OF_FILE);
        }
        let n = data.len().min(out.len());
        out[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    fn read_directory(&self, handle_id: u64) -> Result<Vec<DirEntryData>, NtStatus> {
        let (dir_ino, dir_file_info, parent_ino, parent_chain) = {
            let guard = self.handles.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
            let state = guard.get(&handle_id).ok_or(STATUS_INVALID_HANDLE)?;
            match &state.kind {
                HandleKind::Directory {
                    parent_ino,
                    parent_chain,
                } => (
                    state.ino,
                    state.file_info,
                    *parent_ino,
                    parent_chain.clone(),
                ),
                _ => return Err(STATUS_NOT_A_DIRECTORY),
            }
        };

        let raw_entries = self
            .provider
            .readdirplus(dir_ino, 0)
            .map_err(|e| fs_error_to_ntstatus(&e))?;

        let mut out = Vec::with_capacity(raw_entries.len() + 2);
        if dir_ino != ROOT_INODE {
            out.push(DirEntryData {
                name_utf16: vec![u16::from(b'.')],
                file_info: dir_file_info,
            });
            let parent_attr = self
                .provider
                .getattr(parent_ino)
                .map_err(|e| fs_error_to_ntstatus(&e))?;
            let parent_slice = if parent_chain.len() > 1 {
                &parent_chain[..parent_chain.len() - 1]
            } else {
                &parent_chain[..]
            };
            out.push(DirEntryData {
                name_utf16: vec![u16::from(b'.'), u16::from(b'.')],
                file_info: self.attr_to_file_info(&parent_attr, parent_slice),
            });
        }

        for entry in raw_entries {
            let name_str = String::from_utf8_lossy(entry.name.as_slice());
            if name_str == "." || name_str == ".." {
                continue;
            }
            out.push(DirEntryData {
                name_utf16: name_str.encode_utf16().collect(),
                file_info: self.attr_to_file_info(&entry.attr, &parent_chain),
            });
        }

        Ok(out)
    }

    fn get_dir_info_by_name(
        &self,
        parent_handle_id: u64,
        name: &str,
    ) -> Result<DirEntryData, NtStatus> {
        if name.is_empty() || name.contains(['\\', '/', ':']) {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        let (dir_ino, parent_chain) = {
            let guard = self.handles.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
            let state = guard.get(&parent_handle_id).ok_or(STATUS_INVALID_HANDLE)?;
            match &state.kind {
                HandleKind::Directory { parent_chain, .. } => (state.ino, parent_chain.clone()),
                _ => return Err(STATUS_NOT_A_DIRECTORY),
            }
        };

        let attr = self
            .provider
            .lookup(dir_ino, name.as_bytes())
            .map_err(|e| fs_error_to_ntstatus(&e))?;
        let file_info = self.attr_to_file_info(&attr, &parent_chain);
        Ok(DirEntryData {
            name_utf16: name.encode_utf16().collect(),
            file_info,
        })
    }

    fn get_reparse_point_by_name(&self, path: &str) -> Result<Vec<u8>, NtStatus> {
        if self.symlink_mode != SymlinkMode::NativeReparse {
            return Err(STATUS_NOT_A_REPARSE_POINT);
        }
        match self.resolve_path(path) {
            PathResolution::Resolved {
                ino,
                attr,
                parent_chain,
            } => {
                if attr.kind != FileType::Symlink {
                    return Err(STATUS_NOT_A_REPARSE_POINT);
                }
                let cached = self.inspect_symlink(ino, &parent_chain)?;
                cached.reparse_buffer.ok_or(STATUS_NOT_A_REPARSE_POINT)
            }
            PathResolution::IntermediateReparse => Err(STATUS_NOT_A_REPARSE_POINT),
            PathResolution::NotFound(status) => Err(status),
        }
    }

    fn get_reparse_point(&self, handle_id: u64) -> Result<Vec<u8>, NtStatus> {
        if self.symlink_mode != SymlinkMode::NativeReparse {
            return Err(STATUS_NOT_A_REPARSE_POINT);
        }
        let guard = self.handles.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let state = guard.get(&handle_id).ok_or(STATUS_INVALID_HANDLE)?;
        match &state.kind {
            HandleKind::Symlink {
                reparse_buffer: Some(buf),
                ..
            } => Ok(buf.clone()),
            _ => Err(STATUS_NOT_A_REPARSE_POINT),
        }
    }
}
