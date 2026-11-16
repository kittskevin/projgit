//! `vhost-user-fs` and FUSE + `virtio-fs` (`DAX`) wire protocol frontend for `projgit`.
//!
//! This crate translates standard FUSE kernel wire frames (including `virtio-fs`
//! `FUSE_SETUPMAPPING` opcode 48 and `FUSE_REMOVEMAPPING` opcode 49) into calls
//! on [`projgit_core::fs_provider::FsProvider`]:
//!
//! * Regular tree blobs and `SyntheticEntry::HostFile` entries (such as
//!   `.git/objects/pack/*.pack` in `--isolated-dotgit` mode) return
//!   `Some(Arc<File>)` from [`FsProvider::backing_file`] and are mapped directly
//!   into the [`dax::DaxMapper`] window on `FUSE_SETUPMAPPING`.
//! * In-memory synthetic entries (such as `.git/HEAD` and `.git/index`) return
//!   `Ok(None)` from [`FsProvider::backing_file`]; `ProjgitVirtioFs` replies with
//!   `-ENOSYS` (`-38`) to `FUSE_SETUPMAPPING` so the guest kernel transparently
//!   falls back to `FUSE_READ` over the virtqueue.
//! * All write opcodes return `-EROFS` (`-30`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod dax;
pub mod server;
pub mod wire;

use std::sync::Arc;

use projgit_core::{FileType, FsProvider};

use crate::dax::{DaxMapper, MemoryDaxWindow};
use crate::wire::{
    decode_remove_mappings, encode_readonly_statfs, errno, fs_error_to_errno, FuseAttrOut,
    FuseDirent, FuseEntryOut, FuseInHeader, FuseInitIn, FuseInitOut, FuseOpcode, FuseOpenIn,
    FuseOpenOut, FuseOutHeader, FuseReadIn, FuseSetupMappingIn, DEFAULT_DAX_MAP_ALIGNMENT,
    FOPEN_KEEP_CACHE, FUSE_ASYNC_READ, FUSE_CACHE_SYMLINKS, FUSE_IN_HEADER_LEN,
    FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_MAP_ALIGNMENT,
    FUSE_SETUPMAPPING_FLAG_WRITE,
};

/// Whether the native Unix domain socket `vhost-user-fs` listener is supported on this target OS.
///
/// The FUSE + `DAX` wire protocol engine ([`ProjgitVirtioFs`]) and stream framing
/// ([`server::serve_stream`]) are platform-agnostic and run on all targets.
pub const SUPPORTED: bool = cfg!(unix);

/// Stateless FUSE + `virtio-fs` (`DAX`) protocol dispatcher backed by an [`FsProvider`].
#[derive(Debug)]
pub struct ProjgitVirtioFs<F: FsProvider, M: DaxMapper = MemoryDaxWindow> {
    provider: F,
    mapper: Arc<M>,
    uid: u32,
    gid: u32,
}

impl<F: FsProvider> ProjgitVirtioFs<F, MemoryDaxWindow> {
    /// Construct a new [`ProjgitVirtioFs`] with a default [`MemoryDaxWindow`].
    pub fn new(provider: F) -> Self {
        Self::with_mapper(provider, Arc::new(MemoryDaxWindow::default()))
    }
}

impl<F: FsProvider, M: DaxMapper> ProjgitVirtioFs<F, M> {
    /// Construct a new [`ProjgitVirtioFs`] with a caller-supplied [`DaxMapper`].
    pub fn with_mapper(provider: F, mapper: Arc<M>) -> Self {
        Self {
            provider,
            mapper,
            uid: 1000,
            gid: 1000,
        }
    }

    /// Override the owner `(uid, gid)` reported in `FuseAttr` responses.
    pub fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }

    /// Borrow the underlying [`FsProvider`].
    pub fn provider(&self) -> &F {
        &self.provider
    }

    /// Borrow the configured [`DaxMapper`].
    pub fn dax_mapper(&self) -> &Arc<M> {
        &self.mapper
    }

    /// Decode a raw FUSE request frame (`FuseInHeader` + payload), dispatch it against
    /// the [`FsProvider`] and [`DaxMapper`], and return the encoded FUSE response frame.
    pub fn dispatch(&self, request: &[u8]) -> Vec<u8> {
        let Some(hdr) = FuseInHeader::decode(request) else {
            return FuseOutHeader::reply_err(0, errno::EINVAL);
        };
        if (hdr.len as usize) > request.len() || (hdr.len as usize) < FUSE_IN_HEADER_LEN {
            return FuseOutHeader::reply_err(hdr.unique, errno::EINVAL);
        }
        let payload = &request[FUSE_IN_HEADER_LEN..hdr.len as usize];

        let Some(opcode) = FuseOpcode::from_u32(hdr.opcode) else {
            return FuseOutHeader::reply_err(hdr.unique, errno::ENOSYS);
        };

        if opcode.is_write_operation() {
            return FuseOutHeader::reply_err(hdr.unique, errno::EROFS);
        }

        match opcode {
            FuseOpcode::Init => self.handle_init(hdr.unique, payload),
            FuseOpcode::Destroy
            | FuseOpcode::Forget
            | FuseOpcode::BatchForget
            | FuseOpcode::Release
            | FuseOpcode::Releasedir => FuseOutHeader::reply_ok(hdr.unique, &[]),
            FuseOpcode::Lookup => self.handle_lookup(hdr.unique, hdr.nodeid, payload),
            FuseOpcode::Getattr => self.handle_getattr(hdr.unique, hdr.nodeid),
            FuseOpcode::Readlink => self.handle_readlink(hdr.unique, hdr.nodeid),
            FuseOpcode::Open => self.handle_open(hdr.unique, hdr.nodeid, payload, false),
            FuseOpcode::Opendir => self.handle_open(hdr.unique, hdr.nodeid, payload, true),
            FuseOpcode::Read => self.handle_read(hdr.unique, hdr.nodeid, payload),
            FuseOpcode::Readdir => self.handle_readdir(hdr.unique, hdr.nodeid, payload),
            FuseOpcode::Statfs => FuseOutHeader::reply_ok(hdr.unique, &encode_readonly_statfs()),
            FuseOpcode::SetupMapping => self.handle_setup_mapping(hdr.unique, hdr.nodeid, payload),
            FuseOpcode::RemoveMapping => self.handle_remove_mapping(hdr.unique, payload),
            // Write opcodes are handled above by `opcode.is_write_operation()`.
            _ => FuseOutHeader::reply_err(hdr.unique, errno::EROFS),
        }
    }

    fn handle_init(&self, unique: u64, payload: &[u8]) -> Vec<u8> {
        let Some(init_in) = FuseInitIn::decode(payload) else {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        };
        if init_in.major < FUSE_KERNEL_VERSION {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        }
        let supported_flags = FUSE_ASYNC_READ | FUSE_CACHE_SYMLINKS | FUSE_MAP_ALIGNMENT;
        let negotiated_flags = (init_in.flags & supported_flags) | FUSE_MAP_ALIGNMENT;
        let out = FuseInitOut {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: init_in.max_readahead.max(131_072),
            flags: negotiated_flags,
            max_background: 64,
            congestion_threshold: 48,
            max_write: 131_072,
            time_gran: 1,
            max_pages: 32,
            map_alignment: DEFAULT_DAX_MAP_ALIGNMENT,
            unused: [0; 8],
        };
        FuseOutHeader::reply_ok(unique, &out.encode())
    }

    fn handle_lookup(&self, unique: u64, parent_ino: u64, payload: &[u8]) -> Vec<u8> {
        let name = match payload.iter().position(|&b| b == 0) {
            Some(pos) => &payload[..pos],
            None => payload,
        };
        if name.is_empty() {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        }
        match self.provider.lookup(parent_ino, name) {
            Ok(attr) => {
                let entry_out = FuseEntryOut::from_file_attr(&attr, self.uid, self.gid);
                FuseOutHeader::reply_ok(unique, &entry_out.encode())
            }
            Err(e) => FuseOutHeader::reply_err(unique, fs_error_to_errno(&e)),
        }
    }

    fn handle_getattr(&self, unique: u64, nodeid: u64) -> Vec<u8> {
        match self.provider.getattr(nodeid) {
            Ok(attr) => {
                let attr_out = FuseAttrOut::from_file_attr(&attr, self.uid, self.gid);
                FuseOutHeader::reply_ok(unique, &attr_out.encode())
            }
            Err(e) => FuseOutHeader::reply_err(unique, fs_error_to_errno(&e)),
        }
    }

    fn handle_readlink(&self, unique: u64, nodeid: u64) -> Vec<u8> {
        match self.provider.readlink(nodeid) {
            Ok(target) => FuseOutHeader::reply_ok(unique, &target),
            Err(e) => FuseOutHeader::reply_err(unique, fs_error_to_errno(&e)),
        }
    }

    fn handle_open(&self, unique: u64, nodeid: u64, payload: &[u8], is_dir: bool) -> Vec<u8> {
        if !payload.is_empty() && FuseOpenIn::decode(payload).is_none() {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        }
        match self.provider.getattr(nodeid) {
            Ok(attr) => {
                if is_dir && attr.kind != FileType::Directory {
                    return FuseOutHeader::reply_err(unique, errno::ENOTDIR);
                }
                if !is_dir && attr.kind == FileType::Directory {
                    return FuseOutHeader::reply_err(unique, errno::EISDIR);
                }
                let out = FuseOpenOut {
                    fh: 0,
                    open_flags: FOPEN_KEEP_CACHE,
                    padding: 0,
                };
                FuseOutHeader::reply_ok(unique, &out.encode())
            }
            Err(e) => FuseOutHeader::reply_err(unique, fs_error_to_errno(&e)),
        }
    }

    fn handle_read(&self, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
        let Some(read_in) = FuseReadIn::decode(payload) else {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        };
        match self.provider.read(nodeid, read_in.offset, read_in.size) {
            Ok(bytes) => FuseOutHeader::reply_ok(unique, &bytes),
            Err(e) => FuseOutHeader::reply_err(unique, fs_error_to_errno(&e)),
        }
    }

    fn handle_readdir(&self, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
        let Some(read_in) = FuseReadIn::decode(payload) else {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        };
        let entries = match self.provider.readdir(nodeid, read_in.offset) {
            Ok(list) => list,
            Err(e) => return FuseOutHeader::reply_err(unique, fs_error_to_errno(&e)),
        };

        let max_bytes = read_in.size as usize;
        let mut out = Vec::with_capacity(max_bytes.min(4096));
        for (idx, entry) in entries.into_iter().enumerate() {
            let next_off = read_in.offset + (idx as u64) + 1;
            let dirent = FuseDirent {
                ino: entry.inode,
                off: next_off,
                typ: FuseDirent::dirent_type(entry.kind),
                name: entry.name.into(),
            };
            let needed = FuseDirent::encoded_len(dirent.name.len());
            if out.len() + needed > max_bytes {
                break;
            }
            dirent.encode_into(&mut out);
        }
        FuseOutHeader::reply_ok(unique, &out)
    }

    fn handle_setup_mapping(&self, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
        let Some(setup) = FuseSetupMappingIn::decode(payload) else {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        };
        if (setup.flags & FUSE_SETUPMAPPING_FLAG_WRITE) != 0 {
            return FuseOutHeader::reply_err(unique, errno::EROFS);
        }
        match self.provider.backing_file(nodeid) {
            Ok(Some(file)) => {
                match self.mapper.setup_mapping(
                    nodeid,
                    file,
                    setup.foffset,
                    setup.len,
                    setup.moffset,
                    setup.flags,
                ) {
                    Ok(()) => FuseOutHeader::reply_ok(unique, &[]),
                    Err(code) => FuseOutHeader::reply_err(unique, code),
                }
            }
            // In-memory synthetic files (`.git/HEAD`, `.git/index`, etc.) have no
            // host backing file descriptor; return `-ENOSYS` so the guest kernel
            // falls back to standard `FUSE_READ` over the virtqueue.
            Ok(None) => FuseOutHeader::reply_err(unique, errno::ENOSYS),
            Err(e) => FuseOutHeader::reply_err(unique, fs_error_to_errno(&e)),
        }
    }

    fn handle_remove_mapping(&self, unique: u64, payload: &[u8]) -> Vec<u8> {
        let Some(entries) = decode_remove_mappings(payload) else {
            return FuseOutHeader::reply_err(unique, errno::EINVAL);
        };
        for entry in entries {
            if let Err(code) = self.mapper.remove_mapping(entry.moffset, entry.len) {
                return FuseOutHeader::reply_err(unique, code);
            }
        }
        FuseOutHeader::reply_ok(unique, &[])
    }
}
