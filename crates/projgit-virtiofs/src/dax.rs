//! `virtio-fs` DAX shared-memory window mapping trait and safe backing-file window engine.
//!
//! When a guest kernel mounts a `virtio-fs` filesystem with `-o dax`, file reads
//! bypass the `FUSE_READ` virtqueue path and instead send `FUSE_SETUPMAPPING`
//! (opcode 48) and `FUSE_REMOVEMAPPING` (opcode 49). The host maps the backing
//! file into the VMM's shared memory window at `moffset`, allowing the guest to
//! access the host page cache directly with zero guest-to-host copies per read.

use std::collections::BTreeMap;
use std::fs::File;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::wire::{errno, FUSE_SETUPMAPPING_FLAG_WRITE};

/// Default simulated DAX window size (8 GiB).
pub const DEFAULT_DAX_WINDOW_SIZE: u64 = 8 * 1024 * 1024 * 1024;

/// A single active DAX window mapping established by `FUSE_SETUPMAPPING`.
#[derive(Debug, Clone)]
pub struct DaxMapping {
    /// Offset in bytes within the shared DAX window (`shm` region 0).
    pub moffset: u64,
    /// Length of the mapped region in bytes.
    pub len: u64,
    /// Offset in bytes within [`Self::file`].
    pub foffset: u64,
    /// Inode number that owns this mapping.
    pub ino: u64,
    /// Mapping flags from `FuseSetupMappingIn::flags`.
    pub flags: u64,
    /// Shared handle to the uncompressed host backing file.
    pub file: Arc<File>,
}

/// Trait implemented by VMM / shared-memory backends that service
/// `FUSE_SETUPMAPPING` and `FUSE_REMOVEMAPPING`.
pub trait DaxMapper: Send + Sync {
    /// Map `len` bytes of `file` starting at `foffset` into the DAX window at `moffset`.
    ///
    /// Returns `Ok(())` on success, or a positive POSIX errno (such as `EINVAL` or `EROFS`)
    /// on failure.
    fn setup_mapping(
        &self,
        ino: u64,
        file: Arc<File>,
        foffset: u64,
        len: u64,
        moffset: u64,
        flags: u64,
    ) -> Result<(), i32>;

    /// Unmap `len` bytes starting at `moffset` from the DAX window.
    ///
    /// Returns `Ok(())` on success, or a positive POSIX errno on failure.
    fn remove_mapping(&self, moffset: u64, len: u64) -> Result<(), i32>;
}

/// Safe, cross-platform DAX window tracker (`MemoryDaxWindow` / `RecordedDaxMapper`).
///
/// Tracks active `(moffset, len, foffset, ino, Arc<File>)` mappings installed via
/// `FUSE_SETUPMAPPING` and services direct window reads (`read_window`) by issuing
/// positioned reads (`read_at` on Unix, `seek_read` on Windows) against the mapped
/// `Arc<File>` without mutating any shared file cursor.
#[derive(Debug)]
pub struct MemoryDaxWindow {
    window_size: u64,
    mappings: RwLock<BTreeMap<u64, DaxMapping>>,
}

/// Alias for [`MemoryDaxWindow`] for callers that inspect recorded DAX mappings in tests.
pub type RecordedDaxMapper = MemoryDaxWindow;

impl Default for MemoryDaxWindow {
    fn default() -> Self {
        Self::new(DEFAULT_DAX_WINDOW_SIZE)
    }
}

impl MemoryDaxWindow {
    /// Construct a new DAX window tracker with capacity `window_size` bytes.
    pub fn new(window_size: u64) -> Self {
        Self {
            window_size,
            mappings: RwLock::new(BTreeMap::new()),
        }
    }

    /// Configured total size of the DAX window in bytes.
    pub fn window_size(&self) -> u64 {
        self.window_size
    }

    /// Number of currently active mappings in the DAX window.
    pub fn mapping_count(&self) -> usize {
        self.mappings.read().len()
    }

    /// Snapshot all active mappings ordered by `moffset`.
    pub fn mappings(&self) -> Vec<DaxMapping> {
        self.mappings.read().values().cloned().collect()
    }

    /// Look up the mapping that starts at or covers `moffset`.
    pub fn mapping_at(&self, moffset: u64) -> Option<DaxMapping> {
        let guard = self.mappings.read();
        let (_, mapping) = guard.range(..=moffset).next_back()?;
        let end = mapping.moffset.checked_add(mapping.len)?;
        if moffset < end {
            Some(mapping.clone())
        } else {
            None
        }
    }

    /// Read `len` bytes directly from the DAX window starting at `moffset`.
    ///
    /// Translates `moffset` through the active [`DaxMapping`] into the underlying
    /// `Arc<File>` offset (`foffset + (moffset - mapping.moffset)`) and performs a
    /// positioned OS read (`read_at` / `seek_read`).
    pub fn read_window(&self, moffset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let req_end = moffset.checked_add(len as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "DAX window read overflow")
        })?;
        let mapping = self.mapping_at(moffset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("no active DAX mapping at moffset {moffset:#x}"),
            )
        })?;
        let map_end = mapping.moffset.saturating_add(mapping.len);
        if req_end > map_end {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "DAX read [{moffset:#x}..{req_end:#x}) exceeds mapping [{:#x}..{map_end:#x})",
                    mapping.moffset
                ),
            ));
        }
        let delta = moffset - mapping.moffset;
        let file_offset = mapping.foffset.checked_add(delta).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "file offset overflow")
        })?;
        read_file_at(&mapping.file, file_offset, len)
    }
}

impl DaxMapper for MemoryDaxWindow {
    fn setup_mapping(
        &self,
        ino: u64,
        file: Arc<File>,
        foffset: u64,
        len: u64,
        moffset: u64,
        flags: u64,
    ) -> Result<(), i32> {
        if (flags & FUSE_SETUPMAPPING_FLAG_WRITE) != 0 {
            return Err(errno::EROFS);
        }
        if len == 0 {
            return Err(errno::EINVAL);
        }
        let end = moffset.checked_add(len).ok_or(errno::EINVAL)?;
        let _file_end = foffset.checked_add(len).ok_or(errno::EINVAL)?;
        if end > self.window_size {
            return Err(errno::EINVAL);
        }

        let mut guard = self.mappings.write();
        // Remove any existing mappings that overlap `[moffset, end)`, matching
        // Linux virtiofsd behavior when the guest kernel reclaims/reuses DAX slots.
        let overlapping: Vec<u64> = guard
            .iter()
            .filter_map(|(&k, m)| {
                let m_end = m.moffset.saturating_add(m.len);
                if m.moffset < end && moffset < m_end {
                    Some(k)
                } else {
                    None
                }
            })
            .collect();
        for k in overlapping {
            guard.remove(&k);
        }

        guard.insert(
            moffset,
            DaxMapping {
                moffset,
                len,
                foffset,
                ino,
                flags,
                file,
            },
        );
        Ok(())
    }

    fn remove_mapping(&self, moffset: u64, len: u64) -> Result<(), i32> {
        if len == 0 {
            return Err(errno::EINVAL);
        }
        let end = moffset.checked_add(len).ok_or(errno::EINVAL)?;
        if end > self.window_size {
            return Err(errno::EINVAL);
        }

        let mut guard = self.mappings.write();
        let to_remove: Vec<u64> = guard
            .iter()
            .filter_map(|(&k, m)| {
                let m_end = m.moffset.saturating_add(m.len);
                if m.moffset < end && moffset < m_end {
                    Some(k)
                } else {
                    None
                }
            })
            .collect();
        for k in to_remove {
            guard.remove(&k);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn read_file_at(file: &File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    let mut read_total = 0usize;
    while read_total < len {
        match file.read_at(&mut buf[read_total..], offset + read_total as u64) {
            Ok(0) => break,
            Ok(n) => read_total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(read_total);
    Ok(buf)
}

#[cfg(windows)]
fn read_file_at(file: &File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::windows::fs::FileExt;
    let mut buf = vec![0u8; len];
    let mut read_total = 0usize;
    while read_total < len {
        match file.seek_read(&mut buf[read_total..], offset + read_total as u64) {
            Ok(0) => break,
            Ok(n) => read_total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(read_total);
    Ok(buf)
}

#[cfg(not(any(unix, windows)))]
fn read_file_at(mut file: &File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let mut read_total = 0usize;
    while read_total < len {
        match file.read(&mut buf[read_total..]) {
            Ok(0) => break,
            Ok(n) => read_total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(read_total);
    Ok(buf)
}
