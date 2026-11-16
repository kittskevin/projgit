//! Safe little-endian FUSE + `virtio-fs` (`DAX`) wire protocol types and codecs.
//!
//! All structures in this module follow the Linux `<linux/fuse.h>` kernel ABI
//! (protocol version 7.38) and encode/decode via bounds-checked byte slices
//! with zero `unsafe` code.

use projgit_core::{Attr, FileType, FsError};

/// Major version of the FUSE kernel protocol implemented by this crate.
pub const FUSE_KERNEL_VERSION: u32 = 7;

/// Minor version of the FUSE kernel protocol implemented by this crate.
///
/// Protocol 7.38 includes `FUSE_SETUPMAPPING` (48), `FUSE_REMOVEMAPPING` (49),
/// and `FUSE_MAP_ALIGNMENT`.
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 38;

/// Byte length of [`FuseInHeader`] on the wire.
pub const FUSE_IN_HEADER_LEN: usize = 40;

/// Byte length of [`FuseOutHeader`] on the wire.
pub const FUSE_OUT_HEADER_LEN: usize = 16;

/// Minimum byte length of [`FuseInitIn`] on the wire.
pub const FUSE_INIT_IN_MIN_LEN: usize = 16;

/// Byte length of [`FuseInitOut`] on the wire (FUSE 7.31+ 64-byte layout).
pub const FUSE_INIT_OUT_LEN: usize = 64;

/// Byte length of [`FuseAttr`] on the wire.
pub const FUSE_ATTR_LEN: usize = 88;

/// Byte length of [`FuseEntryOut`] on the wire.
pub const FUSE_ENTRY_OUT_LEN: usize = 128;

/// Byte length of [`FuseAttrOut`] on the wire.
pub const FUSE_ATTR_OUT_LEN: usize = 104;

/// Byte length of [`FuseOpenIn`] on the wire.
pub const FUSE_OPEN_IN_LEN: usize = 8;

/// Byte length of [`FuseOpenOut`] on the wire.
pub const FUSE_OPEN_OUT_LEN: usize = 16;

/// Minimum byte length of [`FuseReadIn`] on the wire (24 bytes; full 7.9+ is 40 bytes).
pub const FUSE_READ_IN_MIN_LEN: usize = 24;

/// Full byte length of [`FuseReadIn`] on the wire.
pub const FUSE_READ_IN_LEN: usize = 40;

/// Fixed header length of a [`FuseDirent`] record before the variable-length name.
pub const FUSE_DIRENT_HEADER_LEN: usize = 24;

/// Byte length of [`FuseSetupMappingIn`] on the wire.
pub const FUSE_SETUPMAPPING_IN_LEN: usize = 40;

/// Byte length of [`FuseRemoveMappingIn`] on the wire.
pub const FUSE_REMOVEMAPPING_IN_LEN: usize = 4;

/// Byte length of [`FuseRemoveMappingOne`] on the wire.
pub const FUSE_REMOVEMAPPING_ONE_LEN: usize = 16;

/// Byte length of [`FuseStatfsOut`] (`fuse_kstatfs`) on the wire.
pub const FUSE_STATFS_OUT_LEN: usize = 80;

/// `FUSE_INIT` flag: asynchronous read requests permitted.
pub const FUSE_ASYNC_READ: u32 = 1 << 0;

/// `FUSE_INIT` flag: kernel caches symlink targets (`FUSE_CACHE_SYMLINKS`).
pub const FUSE_CACHE_SYMLINKS: u32 = 1 << 23;

/// `FUSE_INIT` flag: `map_alignment` field in `FuseInitOut` is valid (`FUSE_MAP_ALIGNMENT`).
pub const FUSE_MAP_ALIGNMENT: u32 = 1 << 26;

/// `FuseOpenOut` flag: keep page cache across `open(2)` calls (`FOPEN_KEEP_CACHE`).
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;

/// `FuseSetupMappingIn` flag: map range with write permissions.
pub const FUSE_SETUPMAPPING_FLAG_WRITE: u64 = 1 << 0;

/// `FuseSetupMappingIn` flag: map range with read permissions.
pub const FUSE_SETUPMAPPING_FLAG_READ: u64 = 1 << 1;

/// Default `map_alignment` log2 byte shift (`12` = 4 KiB page alignment).
pub const DEFAULT_DAX_MAP_ALIGNMENT: u16 = 12;

/// Default attribute and entry cache validity in seconds (24 hours for immutable projections).
pub const DEFAULT_CACHE_TTL_SECS: u64 = 86_400;

/// POSIX errno constants used in `FuseOutHeader::error` (as negative values `-errno`).
pub mod errno {
    /// No such file or directory (`ENOENT = 2`).
    pub const ENOENT: i32 = 2;
    /// Input/output error (`EIO = 5`).
    pub const EIO: i32 = 5;
    /// Bad file descriptor (`EBADF = 9`).
    pub const EBADF: i32 = 9;
    /// Permission denied (`EACCES = 13`).
    pub const EACCES: i32 = 13;
    /// File exists (`EEXIST = 17`).
    pub const EEXIST: i32 = 17;
    /// Not a directory (`ENOTDIR = 20`).
    pub const ENOTDIR: i32 = 20;
    /// Is a directory (`EISDIR = 21`).
    pub const EISDIR: i32 = 21;
    /// Invalid argument (`EINVAL = 22`).
    pub const EINVAL: i32 = 22;
    /// Read-only filesystem (`EROFS = 30`).
    pub const EROFS: i32 = 30;
    /// Result too large (`ERANGE = 34`).
    pub const ERANGE: i32 = 34;
    /// Function not implemented (`ENOSYS = 38`).
    pub const ENOSYS: i32 = 38;
}

/// Map a [`projgit_core::FsError`] to a positive POSIX errno value.
pub fn fs_error_to_errno(err: &FsError) -> i32 {
    match err {
        FsError::NotFound => errno::ENOENT,
        FsError::NotADirectory => errno::ENOTDIR,
        FsError::NotAFile => errno::EISDIR,
        FsError::NotASymlink => errno::EINVAL,
        FsError::Io(_) => errno::EIO,
        FsError::Unsupported => errno::ENOSYS,
    }
}

/// Standard FUSE kernel wire opcodes (`enum fuse_opcode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum FuseOpcode {
    /// Look up a directory entry by name and return its node ID and attributes (`FUSE_LOOKUP = 1`).
    Lookup = 1,
    /// Forget about an inode lookup reference count (`FUSE_FORGET = 2`).
    Forget = 2,
    /// Get file attributes (`FUSE_GETATTR = 3`).
    Getattr = 3,
    /// Set file attributes (`FUSE_SETATTR = 4`, rejected with `-EROFS`).
    Setattr = 4,
    /// Read the target of a symbolic link (`FUSE_READLINK = 5`).
    Readlink = 5,
    /// Create a symbolic link (`FUSE_SYMLINK = 6`, rejected with `-EROFS`).
    Symlink = 6,
    /// Create a special or ordinary file node (`FUSE_MKNOD = 8`, rejected with `-EROFS`).
    Mknod = 8,
    /// Create a directory (`FUSE_MKDIR = 9`, rejected with `-EROFS`).
    Mkdir = 9,
    /// Remove a file (`FUSE_UNLINK = 10`, rejected with `-EROFS`).
    Unlink = 10,
    /// Remove a directory (`FUSE_RMDIR = 11`, rejected with `-EROFS`).
    Rmdir = 11,
    /// Rename a file or directory (`FUSE_RENAME = 12`, rejected with `-EROFS`).
    Rename = 12,
    /// Create a hard link (`FUSE_LINK = 13`, rejected with `-EROFS`).
    Link = 13,
    /// Open a regular file (`FUSE_OPEN = 14`).
    Open = 14,
    /// Read bytes from an open file (`FUSE_READ = 15`).
    Read = 15,
    /// Write bytes to an open file (`FUSE_WRITE = 16`, rejected with `-EROFS`).
    Write = 16,
    /// Get filesystem statistics (`FUSE_STATFS = 17`).
    Statfs = 17,
    /// Release an open regular file handle (`FUSE_RELEASE = 18`).
    Release = 18,
    /// Initialize the FUSE session (`FUSE_INIT = 26`).
    Init = 26,
    /// Open a directory for reading (`FUSE_OPENDIR = 27`).
    Opendir = 27,
    /// Read directory entries (`FUSE_READDIR = 28`).
    Readdir = 28,
    /// Release an open directory handle (`FUSE_RELEASEDIR = 29`).
    Releasedir = 29,
    /// Create and open a file (`FUSE_CREATE = 35`, rejected with `-EROFS`).
    Create = 35,
    /// Clean up the filesystem session (`FUSE_DESTROY = 38`).
    Destroy = 38,
    /// Forget multiple inodes in one batch (`FUSE_BATCH_FORGET = 42`).
    BatchForget = 42,
    /// Map a host backing file range into the `virtio-fs` DAX window (`FUSE_SETUPMAPPING = 48`).
    SetupMapping = 48,
    /// Unmap one or more ranges from the `virtio-fs` DAX window (`FUSE_REMOVEMAPPING = 49`).
    RemoveMapping = 49,
}

impl FuseOpcode {
    /// Convert a raw `u32` opcode from [`FuseInHeader::opcode`] into a [`FuseOpcode`].
    pub const fn from_u32(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Lookup),
            2 => Some(Self::Forget),
            3 => Some(Self::Getattr),
            4 => Some(Self::Setattr),
            5 => Some(Self::Readlink),
            6 => Some(Self::Symlink),
            8 => Some(Self::Mknod),
            9 => Some(Self::Mkdir),
            10 => Some(Self::Unlink),
            11 => Some(Self::Rmdir),
            12 => Some(Self::Rename),
            13 => Some(Self::Link),
            14 => Some(Self::Open),
            15 => Some(Self::Read),
            16 => Some(Self::Write),
            17 => Some(Self::Statfs),
            18 => Some(Self::Release),
            26 => Some(Self::Init),
            27 => Some(Self::Opendir),
            28 => Some(Self::Readdir),
            29 => Some(Self::Releasedir),
            35 => Some(Self::Create),
            38 => Some(Self::Destroy),
            42 => Some(Self::BatchForget),
            48 => Some(Self::SetupMapping),
            49 => Some(Self::RemoveMapping),
            _ => None,
        }
    }

    /// Returns `true` if this opcode mutates filesystem state and must be
    /// rejected with `-EROFS` on a read-only projection mount.
    pub const fn is_write_operation(self) -> bool {
        matches!(
            self,
            Self::Setattr
                | Self::Symlink
                | Self::Mknod
                | Self::Mkdir
                | Self::Unlink
                | Self::Rmdir
                | Self::Rename
                | Self::Link
                | Self::Write
                | Self::Create
        )
    }
}

/// 40-byte FUSE request header (`struct fuse_in_header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseInHeader {
    /// Total byte length of the request message (header + payload).
    pub len: u32,
    /// Raw FUSE opcode (`FuseOpcode as u32`).
    pub opcode: u32,
    /// Unique request identifier echoed in [`FuseOutHeader::unique`].
    pub unique: u64,
    /// Target inode number (`1` for root).
    pub nodeid: u64,
    /// Calling process user ID.
    pub uid: u32,
    /// Calling process group ID.
    pub gid: u32,
    /// Calling process ID.
    pub pid: u32,
    /// Reserved padding word.
    pub padding: u32,
}

impl FuseInHeader {
    /// Encode this header into its 40-byte little-endian wire representation.
    pub fn encode(&self) -> [u8; FUSE_IN_HEADER_LEN] {
        let mut buf = [0u8; FUSE_IN_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.len.to_le_bytes());
        buf[4..8].copy_from_slice(&self.opcode.to_le_bytes());
        buf[8..16].copy_from_slice(&self.unique.to_le_bytes());
        buf[16..24].copy_from_slice(&self.nodeid.to_le_bytes());
        buf[24..28].copy_from_slice(&self.uid.to_le_bytes());
        buf[28..32].copy_from_slice(&self.gid.to_le_bytes());
        buf[32..36].copy_from_slice(&self.pid.to_le_bytes());
        buf[36..40].copy_from_slice(&self.padding.to_le_bytes());
        buf
    }

    /// Decode a [`FuseInHeader`] from the first 40 bytes of `bytes`.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_IN_HEADER_LEN {
            return None;
        }
        Some(Self {
            len: read_u32(bytes, 0)?,
            opcode: read_u32(bytes, 4)?,
            unique: read_u64(bytes, 8)?,
            nodeid: read_u64(bytes, 16)?,
            uid: read_u32(bytes, 24)?,
            gid: read_u32(bytes, 28)?,
            pid: read_u32(bytes, 32)?,
            padding: read_u32(bytes, 36)?,
        })
    }
}

/// 16-byte FUSE response header (`struct fuse_out_header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseOutHeader {
    /// Total byte length of the response message (header + payload).
    pub len: u32,
    /// `0` on success, or negative POSIX errno (e.g. `-ENOENT = -2`) on failure.
    pub error: i32,
    /// Unique request ID copied from [`FuseInHeader::unique`].
    pub unique: u64,
}

impl FuseOutHeader {
    /// Encode this header into its 16-byte little-endian wire representation.
    pub fn encode(&self) -> [u8; FUSE_OUT_HEADER_LEN] {
        let mut buf = [0u8; FUSE_OUT_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.len.to_le_bytes());
        buf[4..8].copy_from_slice(&self.error.to_le_bytes());
        buf[8..16].copy_from_slice(&self.unique.to_le_bytes());
        buf
    }

    /// Decode a [`FuseOutHeader`] from the first 16 bytes of `bytes`.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_OUT_HEADER_LEN {
            return None;
        }
        Some(Self {
            len: read_u32(bytes, 0)?,
            error: read_i32(bytes, 4)?,
            unique: read_u64(bytes, 8)?,
        })
    }

    /// Build a complete response frame consisting of a [`FuseOutHeader`] followed by `payload`.
    pub fn reply_ok(unique: u64, payload: &[u8]) -> Vec<u8> {
        let total_len = (FUSE_OUT_HEADER_LEN + payload.len()) as u32;
        let hdr = Self {
            len: total_len,
            error: 0,
            unique,
        };
        let mut out = Vec::with_capacity(FUSE_OUT_HEADER_LEN + payload.len());
        out.extend_from_slice(&hdr.encode());
        out.extend_from_slice(payload);
        out
    }

    /// Build a 16-byte error response frame with `error = -errno.abs()`.
    pub fn reply_err(unique: u64, errno_val: i32) -> Vec<u8> {
        let neg_err = if errno_val > 0 { -errno_val } else { errno_val };
        let hdr = Self {
            len: FUSE_OUT_HEADER_LEN as u32,
            error: neg_err,
            unique,
        };
        hdr.encode().to_vec()
    }
}

/// `FUSE_INIT` request payload (`struct fuse_init_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseInitIn {
    /// Guest FUSE protocol major version.
    pub major: u32,
    /// Guest FUSE protocol minor version.
    pub minor: u32,
    /// Maximum readahead in bytes requested by the guest.
    pub max_readahead: u32,
    /// Capability flags advertised by the guest.
    pub flags: u32,
}

impl FuseInitIn {
    /// Encode into 16 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_INIT_IN_MIN_LEN] {
        let mut buf = [0u8; FUSE_INIT_IN_MIN_LEN];
        buf[0..4].copy_from_slice(&self.major.to_le_bytes());
        buf[4..8].copy_from_slice(&self.minor.to_le_bytes());
        buf[8..12].copy_from_slice(&self.max_readahead.to_le_bytes());
        buf[12..16].copy_from_slice(&self.flags.to_le_bytes());
        buf
    }

    /// Decode from `bytes` (must be at least 16 bytes).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_INIT_IN_MIN_LEN {
            return None;
        }
        Some(Self {
            major: read_u32(bytes, 0)?,
            minor: read_u32(bytes, 4)?,
            max_readahead: read_u32(bytes, 8)?,
            flags: read_u32(bytes, 12)?,
        })
    }
}

/// 64-byte `FUSE_INIT` response payload (`struct fuse_init_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseInitOut {
    /// Negotiated FUSE major version (`7`).
    pub major: u32,
    /// Negotiated FUSE minor version (`38`).
    pub minor: u32,
    /// Negotiated maximum readahead in bytes.
    pub max_readahead: u32,
    /// Negotiated capability flags.
    pub flags: u32,
    /// Maximum number of background requests.
    pub max_background: u16,
    /// Congestion threshold for background requests.
    pub congestion_threshold: u16,
    /// Maximum write size in bytes.
    pub max_write: u32,
    /// Timestamp granularity in nanoseconds (`1`).
    pub time_gran: u32,
    /// Maximum pages per request.
    pub max_pages: u16,
    /// Log2 byte alignment required for `FUSE_SETUPMAPPING` offsets (`12` = 4 KiB).
    pub map_alignment: u16,
    /// Reserved zero words.
    pub unused: [u32; 8],
}

impl FuseInitOut {
    /// Encode into 64 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_INIT_OUT_LEN] {
        let mut buf = [0u8; FUSE_INIT_OUT_LEN];
        buf[0..4].copy_from_slice(&self.major.to_le_bytes());
        buf[4..8].copy_from_slice(&self.minor.to_le_bytes());
        buf[8..12].copy_from_slice(&self.max_readahead.to_le_bytes());
        buf[12..16].copy_from_slice(&self.flags.to_le_bytes());
        buf[16..18].copy_from_slice(&self.max_background.to_le_bytes());
        buf[18..20].copy_from_slice(&self.congestion_threshold.to_le_bytes());
        buf[20..24].copy_from_slice(&self.max_write.to_le_bytes());
        buf[24..28].copy_from_slice(&self.time_gran.to_le_bytes());
        buf[28..30].copy_from_slice(&self.max_pages.to_le_bytes());
        buf[30..32].copy_from_slice(&self.map_alignment.to_le_bytes());
        for (i, word) in self.unused.iter().enumerate() {
            let start = 32 + i * 4;
            buf[start..start + 4].copy_from_slice(&word.to_le_bytes());
        }
        buf
    }

    /// Decode from a 64-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_INIT_OUT_LEN {
            return None;
        }
        let mut unused = [0u32; 8];
        for (i, slot) in unused.iter_mut().enumerate() {
            *slot = read_u32(bytes, 32 + i * 4)?;
        }
        Some(Self {
            major: read_u32(bytes, 0)?,
            minor: read_u32(bytes, 4)?,
            max_readahead: read_u32(bytes, 8)?,
            flags: read_u32(bytes, 12)?,
            max_background: read_u16(bytes, 16)?,
            congestion_threshold: read_u16(bytes, 18)?,
            max_write: read_u32(bytes, 20)?,
            time_gran: read_u32(bytes, 24)?,
            max_pages: read_u16(bytes, 28)?,
            map_alignment: read_u16(bytes, 30)?,
            unused,
        })
    }
}

/// 88-byte FUSE file attribute record (`struct fuse_attr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseAttr {
    /// Inode number.
    pub ino: u64,
    /// Logical file size in bytes.
    pub size: u64,
    /// Allocated 512-byte blocks.
    pub blocks: u64,
    /// Access time seconds.
    pub atime: u64,
    /// Modification time seconds.
    pub mtime: u64,
    /// Status change time seconds.
    pub ctime: u64,
    /// Access time nanoseconds.
    pub atimensec: u32,
    /// Modification time nanoseconds.
    pub mtimensec: u32,
    /// Status change time nanoseconds.
    pub ctimensec: u32,
    /// Full POSIX `st_mode` (file type + permission bits).
    pub mode: u32,
    /// Hard link count.
    pub nlink: u32,
    /// Owner user ID.
    pub uid: u32,
    /// Owner group ID.
    pub gid: u32,
    /// Device ID (0 for regular files/directories).
    pub rdev: u32,
    /// Preferred I/O block size in bytes.
    pub blksize: u32,
    /// Attribute flags.
    pub flags: u32,
}

impl FuseAttr {
    /// Convert a [`projgit_core::Attr`] into a wire [`FuseAttr`].
    pub fn from_file_attr(attr: &Attr, uid: u32, gid: u32) -> Self {
        let mode = match attr.kind {
            FileType::Directory => 0o040000 | (attr.mode as u32 & 0o7777),
            FileType::RegularFile => 0o100000 | (attr.mode as u32 & 0o7777),
            FileType::Symlink => 0o120000 | (attr.mode as u32 & 0o7777),
        };
        let (secs, nsecs) = attr
            .mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        Self {
            ino: attr.inode,
            size: attr.size,
            blocks: attr.size.div_ceil(512),
            atime: secs,
            mtime: secs,
            ctime: secs,
            atimensec: nsecs,
            mtimensec: nsecs,
            ctimensec: nsecs,
            mode,
            nlink: attr.nlink,
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Encode into 88 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_ATTR_LEN] {
        let mut buf = [0u8; FUSE_ATTR_LEN];
        buf[0..8].copy_from_slice(&self.ino.to_le_bytes());
        buf[8..16].copy_from_slice(&self.size.to_le_bytes());
        buf[16..24].copy_from_slice(&self.blocks.to_le_bytes());
        buf[24..32].copy_from_slice(&self.atime.to_le_bytes());
        buf[32..40].copy_from_slice(&self.mtime.to_le_bytes());
        buf[40..48].copy_from_slice(&self.ctime.to_le_bytes());
        buf[48..52].copy_from_slice(&self.atimensec.to_le_bytes());
        buf[52..56].copy_from_slice(&self.mtimensec.to_le_bytes());
        buf[56..60].copy_from_slice(&self.ctimensec.to_le_bytes());
        buf[60..64].copy_from_slice(&self.mode.to_le_bytes());
        buf[64..68].copy_from_slice(&self.nlink.to_le_bytes());
        buf[68..72].copy_from_slice(&self.uid.to_le_bytes());
        buf[72..76].copy_from_slice(&self.gid.to_le_bytes());
        buf[76..80].copy_from_slice(&self.rdev.to_le_bytes());
        buf[80..84].copy_from_slice(&self.blksize.to_le_bytes());
        buf[84..88].copy_from_slice(&self.flags.to_le_bytes());
        buf
    }

    /// Decode from an 88-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_ATTR_LEN {
            return None;
        }
        Some(Self {
            ino: read_u64(bytes, 0)?,
            size: read_u64(bytes, 8)?,
            blocks: read_u64(bytes, 16)?,
            atime: read_u64(bytes, 24)?,
            mtime: read_u64(bytes, 32)?,
            ctime: read_u64(bytes, 40)?,
            atimensec: read_u32(bytes, 48)?,
            mtimensec: read_u32(bytes, 52)?,
            ctimensec: read_u32(bytes, 56)?,
            mode: read_u32(bytes, 60)?,
            nlink: read_u32(bytes, 64)?,
            uid: read_u32(bytes, 68)?,
            gid: read_u32(bytes, 72)?,
            rdev: read_u32(bytes, 76)?,
            blksize: read_u32(bytes, 80)?,
            flags: read_u32(bytes, 84)?,
        })
    }
}

/// 128-byte `FUSE_LOOKUP` response payload (`struct fuse_entry_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseEntryOut {
    /// Looked-up inode number.
    pub nodeid: u64,
    /// Inode generation number (`0` for immutable projections).
    pub generation: u64,
    /// Directory entry cache validity in seconds.
    pub entry_valid: u64,
    /// Attribute cache validity in seconds.
    pub attr_valid: u64,
    /// Directory entry cache validity nanoseconds.
    pub entry_valid_nsec: u32,
    /// Attribute cache validity nanoseconds.
    pub attr_valid_nsec: u32,
    /// File attributes for the looked-up entry.
    pub attr: FuseAttr,
}

impl FuseEntryOut {
    /// Construct a [`FuseEntryOut`] from an [`Attr`] using [`DEFAULT_CACHE_TTL_SECS`].
    pub fn from_file_attr(attr: &Attr, uid: u32, gid: u32) -> Self {
        Self {
            nodeid: attr.inode,
            generation: 0,
            entry_valid: DEFAULT_CACHE_TTL_SECS,
            attr_valid: DEFAULT_CACHE_TTL_SECS,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: FuseAttr::from_file_attr(attr, uid, gid),
        }
    }

    /// Encode into 128 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_ENTRY_OUT_LEN] {
        let mut buf = [0u8; FUSE_ENTRY_OUT_LEN];
        buf[0..8].copy_from_slice(&self.nodeid.to_le_bytes());
        buf[8..16].copy_from_slice(&self.generation.to_le_bytes());
        buf[16..24].copy_from_slice(&self.entry_valid.to_le_bytes());
        buf[24..32].copy_from_slice(&self.attr_valid.to_le_bytes());
        buf[32..36].copy_from_slice(&self.entry_valid_nsec.to_le_bytes());
        buf[36..40].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        buf[40..128].copy_from_slice(&self.attr.encode());
        buf
    }

    /// Decode from a 128-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_ENTRY_OUT_LEN {
            return None;
        }
        Some(Self {
            nodeid: read_u64(bytes, 0)?,
            generation: read_u64(bytes, 8)?,
            entry_valid: read_u64(bytes, 16)?,
            attr_valid: read_u64(bytes, 24)?,
            entry_valid_nsec: read_u32(bytes, 32)?,
            attr_valid_nsec: read_u32(bytes, 36)?,
            attr: FuseAttr::decode(&bytes[40..128])?,
        })
    }
}

/// 104-byte `FUSE_GETATTR` response payload (`struct fuse_attr_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseAttrOut {
    /// Attribute cache validity in seconds.
    pub attr_valid: u64,
    /// Attribute cache validity nanoseconds.
    pub attr_valid_nsec: u32,
    /// Reserved padding word.
    pub dummy: u32,
    /// File attributes.
    pub attr: FuseAttr,
}

impl FuseAttrOut {
    /// Construct a [`FuseAttrOut`] from an [`Attr`] using [`DEFAULT_CACHE_TTL_SECS`].
    pub fn from_file_attr(attr: &Attr, uid: u32, gid: u32) -> Self {
        Self {
            attr_valid: DEFAULT_CACHE_TTL_SECS,
            attr_valid_nsec: 0,
            dummy: 0,
            attr: FuseAttr::from_file_attr(attr, uid, gid),
        }
    }

    /// Encode into 104 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_ATTR_OUT_LEN] {
        let mut buf = [0u8; FUSE_ATTR_OUT_LEN];
        buf[0..8].copy_from_slice(&self.attr_valid.to_le_bytes());
        buf[8..12].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        buf[12..16].copy_from_slice(&self.dummy.to_le_bytes());
        buf[16..104].copy_from_slice(&self.attr.encode());
        buf
    }

    /// Decode from a 104-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_ATTR_OUT_LEN {
            return None;
        }
        Some(Self {
            attr_valid: read_u64(bytes, 0)?,
            attr_valid_nsec: read_u32(bytes, 8)?,
            dummy: read_u32(bytes, 12)?,
            attr: FuseAttr::decode(&bytes[16..104])?,
        })
    }
}

/// 8-byte `FUSE_OPEN` / `FUSE_OPENDIR` request payload (`struct fuse_open_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseOpenIn {
    /// `open(2)` flags (e.g. `O_RDONLY`).
    pub flags: u32,
    /// Additional FUSE open flags.
    pub open_flags: u32,
}

impl FuseOpenIn {
    /// Encode into 8 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_OPEN_IN_LEN] {
        let mut buf = [0u8; FUSE_OPEN_IN_LEN];
        buf[0..4].copy_from_slice(&self.flags.to_le_bytes());
        buf[4..8].copy_from_slice(&self.open_flags.to_le_bytes());
        buf
    }

    /// Decode from an 8-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_OPEN_IN_LEN {
            return None;
        }
        Some(Self {
            flags: read_u32(bytes, 0)?,
            open_flags: read_u32(bytes, 4)?,
        })
    }
}

/// 16-byte `FUSE_OPEN` / `FUSE_OPENDIR` response payload (`struct fuse_open_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseOpenOut {
    /// Stateless file handle (`0`).
    pub fh: u64,
    /// Open response flags (e.g. [`FOPEN_KEEP_CACHE`]).
    pub open_flags: u32,
    /// Reserved padding word.
    pub padding: u32,
}

impl FuseOpenOut {
    /// Encode into 16 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_OPEN_OUT_LEN] {
        let mut buf = [0u8; FUSE_OPEN_OUT_LEN];
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..12].copy_from_slice(&self.open_flags.to_le_bytes());
        buf[12..16].copy_from_slice(&self.padding.to_le_bytes());
        buf
    }

    /// Decode from a 16-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_OPEN_OUT_LEN {
            return None;
        }
        Some(Self {
            fh: read_u64(bytes, 0)?,
            open_flags: read_u32(bytes, 8)?,
            padding: read_u32(bytes, 12)?,
        })
    }
}

/// `FUSE_READ` / `FUSE_READDIR` request payload (`struct fuse_read_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseReadIn {
    /// File handle returned by `FUSE_OPEN` / `FUSE_OPENDIR`.
    pub fh: u64,
    /// Byte offset (for `FUSE_READ`) or directory entry cookie (for `FUSE_READDIR`).
    pub offset: u64,
    /// Maximum number of bytes to return.
    pub size: u32,
    /// Read flags.
    pub read_flags: u32,
    /// Lock owner identifier (0 when unused).
    pub lock_owner: u64,
    /// File status flags.
    pub flags: u32,
    /// Reserved padding word.
    pub padding: u32,
}

impl FuseReadIn {
    /// Encode into 40 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_READ_IN_LEN] {
        let mut buf = [0u8; FUSE_READ_IN_LEN];
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.size.to_le_bytes());
        buf[20..24].copy_from_slice(&self.read_flags.to_le_bytes());
        buf[24..32].copy_from_slice(&self.lock_owner.to_le_bytes());
        buf[32..36].copy_from_slice(&self.flags.to_le_bytes());
        buf[36..40].copy_from_slice(&self.padding.to_le_bytes());
        buf
    }

    /// Decode from a slice of at least 24 bytes (supporting both 24-byte and 40-byte layouts).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_READ_IN_MIN_LEN {
            return None;
        }
        let lock_owner = if bytes.len() >= 32 {
            read_u64(bytes, 24)?
        } else {
            0
        };
        let flags = if bytes.len() >= 36 {
            read_u32(bytes, 32)?
        } else {
            0
        };
        let padding = if bytes.len() >= 40 {
            read_u32(bytes, 36)?
        } else {
            0
        };
        Some(Self {
            fh: read_u64(bytes, 0)?,
            offset: read_u64(bytes, 8)?,
            size: read_u32(bytes, 16)?,
            read_flags: read_u32(bytes, 20)?,
            lock_owner,
            flags,
            padding,
        })
    }
}

/// 8-byte-aligned directory entry record (`struct fuse_dirent`) returned by `FUSE_READDIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuseDirent {
    /// Child inode number.
    pub ino: u64,
    /// Cookie offset of the *next* directory entry (`1`-based).
    pub off: u64,
    /// POSIX `d_type` (`DT_DIR = 4`, `DT_REG = 8`, `DT_LNK = 10`).
    pub typ: u32,
    /// Unpadded entry name bytes.
    pub name: Vec<u8>,
}

impl FuseDirent {
    /// Convert a [`FileType`] to its POSIX `d_type` constant.
    pub const fn dirent_type(kind: FileType) -> u32 {
        match kind {
            FileType::Directory => 4,
            FileType::RegularFile => 8,
            FileType::Symlink => 10,
        }
    }

    /// Total 8-byte-aligned wire size of this `fuse_dirent` record.
    pub const fn encoded_len(name_len: usize) -> usize {
        (FUSE_DIRENT_HEADER_LEN + name_len + 7) & !7
    }

    /// Append this record (including 8-byte zero padding) to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let namelen = self.name.len() as u32;
        let total = Self::encoded_len(self.name.len());
        out.extend_from_slice(&self.ino.to_le_bytes());
        out.extend_from_slice(&self.off.to_le_bytes());
        out.extend_from_slice(&namelen.to_le_bytes());
        out.extend_from_slice(&self.typ.to_le_bytes());
        out.extend_from_slice(&self.name);
        let pad = total - (FUSE_DIRENT_HEADER_LEN + self.name.len());
        if pad > 0 {
            out.extend(std::iter::repeat_n(0u8, pad));
        }
    }

    /// Encode this directory entry into a standalone byte vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::encoded_len(self.name.len()));
        self.encode_into(&mut out);
        out
    }

    /// Decode a sequence of 8-byte-aligned `fuse_dirent` records from a `FUSE_READDIR` payload.
    pub fn decode_all(mut bytes: &[u8]) -> Option<Vec<Self>> {
        let mut entries = Vec::new();
        while !bytes.is_empty() {
            if bytes.len() < FUSE_DIRENT_HEADER_LEN {
                return None;
            }
            let ino = read_u64(bytes, 0)?;
            let off = read_u64(bytes, 8)?;
            let namelen = read_u32(bytes, 16)? as usize;
            let typ = read_u32(bytes, 20)?;
            let total = Self::encoded_len(namelen);
            if bytes.len() < total {
                return None;
            }
            let name = bytes[FUSE_DIRENT_HEADER_LEN..FUSE_DIRENT_HEADER_LEN + namelen].to_vec();
            entries.push(Self {
                ino,
                off,
                typ,
                name,
            });
            bytes = &bytes[total..];
        }
        Some(entries)
    }
}

/// 40-byte `FUSE_SETUPMAPPING` request payload (`struct fuse_setupmapping_in`, opcode 48).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseSetupMappingIn {
    /// Open file handle (unused by stateless `projgit-virtiofs`; `nodeid` identifies the file).
    pub fh: u64,
    /// Byte offset within the backing file to map.
    pub foffset: u64,
    /// Length in bytes of the mapping.
    pub len: u64,
    /// Mapping flags ([`FUSE_SETUPMAPPING_FLAG_READ`], [`FUSE_SETUPMAPPING_FLAG_WRITE`]).
    pub flags: u64,
    /// Byte offset within the shared `virtio-fs` DAX window (`shm` region 0).
    pub moffset: u64,
}

impl FuseSetupMappingIn {
    /// Encode into 40 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_SETUPMAPPING_IN_LEN] {
        let mut buf = [0u8; FUSE_SETUPMAPPING_IN_LEN];
        buf[0..8].copy_from_slice(&self.fh.to_le_bytes());
        buf[8..16].copy_from_slice(&self.foffset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.len.to_le_bytes());
        buf[24..32].copy_from_slice(&self.flags.to_le_bytes());
        buf[32..40].copy_from_slice(&self.moffset.to_le_bytes());
        buf
    }

    /// Decode from a 40-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_SETUPMAPPING_IN_LEN {
            return None;
        }
        Some(Self {
            fh: read_u64(bytes, 0)?,
            foffset: read_u64(bytes, 8)?,
            len: read_u64(bytes, 16)?,
            flags: read_u64(bytes, 24)?,
            moffset: read_u64(bytes, 32)?,
        })
    }
}

/// 4-byte `FUSE_REMOVEMAPPING` request header (`struct fuse_removemapping_in`, opcode 49).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseRemoveMappingIn {
    /// Number of [`FuseRemoveMappingOne`] entries immediately following this header.
    pub count: u32,
}

impl FuseRemoveMappingIn {
    /// Encode into 4 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_REMOVEMAPPING_IN_LEN] {
        self.count.to_le_bytes()
    }

    /// Decode from a 4-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_REMOVEMAPPING_IN_LEN {
            return None;
        }
        Some(Self {
            count: read_u32(bytes, 0)?,
        })
    }
}

/// 16-byte single range entry in a `FUSE_REMOVEMAPPING` request (`struct fuse_removemapping_one`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseRemoveMappingOne {
    /// Byte offset within the `virtio-fs` DAX window to unmap.
    pub moffset: u64,
    /// Byte length of the DAX window mapping to unmap.
    pub len: u64,
}

impl FuseRemoveMappingOne {
    /// Encode into 16 little-endian bytes.
    pub fn encode(&self) -> [u8; FUSE_REMOVEMAPPING_ONE_LEN] {
        let mut buf = [0u8; FUSE_REMOVEMAPPING_ONE_LEN];
        buf[0..8].copy_from_slice(&self.moffset.to_le_bytes());
        buf[8..16].copy_from_slice(&self.len.to_le_bytes());
        buf
    }

    /// Decode from a 16-byte slice.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FUSE_REMOVEMAPPING_ONE_LEN {
            return None;
        }
        Some(Self {
            moffset: read_u64(bytes, 0)?,
            len: read_u64(bytes, 8)?,
        })
    }
}

/// Decode a complete `FUSE_REMOVEMAPPING` payload (`FuseRemoveMappingIn` + `count` entries).
pub fn decode_remove_mappings(bytes: &[u8]) -> Option<Vec<FuseRemoveMappingOne>> {
    let hdr = FuseRemoveMappingIn::decode(bytes)?;
    let count = hdr.count as usize;
    let needed =
        FUSE_REMOVEMAPPING_IN_LEN.checked_add(count.checked_mul(FUSE_REMOVEMAPPING_ONE_LEN)?)?;
    if bytes.len() < needed {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let start = FUSE_REMOVEMAPPING_IN_LEN + i * FUSE_REMOVEMAPPING_ONE_LEN;
        entries.push(FuseRemoveMappingOne::decode(
            &bytes[start..start + FUSE_REMOVEMAPPING_ONE_LEN],
        )?);
    }
    Some(entries)
}

/// Encode a read-only `FUSE_STATFS` response payload (`struct fuse_kstatfs`, 80 bytes).
pub fn encode_readonly_statfs() -> [u8; FUSE_STATFS_OUT_LEN] {
    let mut buf = [0u8; FUSE_STATFS_OUT_LEN];
    let bsize: u32 = 4096;
    let namelen: u32 = 255;
    let frsize: u32 = 4096;
    // offsets inside fuse_kstatfs:
    // 0..48: blocks, bfree, bavail, files, ffree (6 * u64 = 0)
    // 48..52: bsize
    // 52..56: namelen
    // 56..60: frsize
    buf[48..52].copy_from_slice(&bsize.to_le_bytes());
    buf[52..56].copy_from_slice(&namelen.to_le_bytes());
    buf[56..60].copy_from_slice(&frsize.to_le_bytes());
    buf
}

/// Build a raw FUSE request frame (`FuseInHeader` + `payload`).
pub fn build_request(unique: u64, opcode: FuseOpcode, nodeid: u64, payload: &[u8]) -> Vec<u8> {
    let total_len = (FUSE_IN_HEADER_LEN + payload.len()) as u32;
    let hdr = FuseInHeader {
        len: total_len,
        opcode: opcode as u32,
        unique,
        nodeid,
        uid: 1000,
        gid: 1000,
        pid: 1,
        padding: 0,
    };
    let mut buf = Vec::with_capacity(FUSE_IN_HEADER_LEN + payload.len());
    buf.extend_from_slice(&hdr.encode());
    buf.extend_from_slice(payload);
    buf
}

/// Build a `FUSE_INIT` request frame.
pub fn build_init_request(unique: u64, major: u32, minor: u32, flags: u32) -> Vec<u8> {
    let init = FuseInitIn {
        major,
        minor,
        max_readahead: 131_072,
        flags,
    };
    build_request(unique, FuseOpcode::Init, 0, &init.encode())
}

/// Build a `FUSE_LOOKUP` request frame (`name` is automatically NUL-terminated).
pub fn build_lookup_request(unique: u64, parent_ino: u64, name: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(name.len() + 1);
    payload.extend_from_slice(name);
    payload.push(0);
    build_request(unique, FuseOpcode::Lookup, parent_ino, &payload)
}

/// Build a `FUSE_GETATTR` request frame (`struct fuse_getattr_in` is 16 zero bytes).
pub fn build_getattr_request(unique: u64, ino: u64) -> Vec<u8> {
    build_request(unique, FuseOpcode::Getattr, ino, &[0u8; 16])
}

/// Build a `FUSE_READLINK` request frame.
pub fn build_readlink_request(unique: u64, ino: u64) -> Vec<u8> {
    build_request(unique, FuseOpcode::Readlink, ino, &[])
}

/// Build a `FUSE_OPEN` request frame.
pub fn build_open_request(unique: u64, ino: u64, flags: u32) -> Vec<u8> {
    let open = FuseOpenIn {
        flags,
        open_flags: 0,
    };
    build_request(unique, FuseOpcode::Open, ino, &open.encode())
}

/// Build a `FUSE_OPENDIR` request frame.
pub fn build_opendir_request(unique: u64, ino: u64, flags: u32) -> Vec<u8> {
    let open = FuseOpenIn {
        flags,
        open_flags: 0,
    };
    build_request(unique, FuseOpcode::Opendir, ino, &open.encode())
}

/// Build a `FUSE_READ` request frame.
pub fn build_read_request(unique: u64, ino: u64, fh: u64, offset: u64, size: u32) -> Vec<u8> {
    let read = FuseReadIn {
        fh,
        offset,
        size,
        read_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    };
    build_request(unique, FuseOpcode::Read, ino, &read.encode())
}

/// Build a `FUSE_READDIR` request frame.
pub fn build_readdir_request(unique: u64, ino: u64, fh: u64, offset: u64, size: u32) -> Vec<u8> {
    let read = FuseReadIn {
        fh,
        offset,
        size,
        read_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    };
    build_request(unique, FuseOpcode::Readdir, ino, &read.encode())
}

/// Build a `FUSE_SETUPMAPPING` request frame.
pub fn build_setupmapping_request(
    unique: u64,
    ino: u64,
    fh: u64,
    foffset: u64,
    len: u64,
    flags: u64,
    moffset: u64,
) -> Vec<u8> {
    let setup = FuseSetupMappingIn {
        fh,
        foffset,
        len,
        flags,
        moffset,
    };
    build_request(unique, FuseOpcode::SetupMapping, ino, &setup.encode())
}

/// Build a `FUSE_REMOVEMAPPING` request frame.
pub fn build_removemapping_request(
    unique: u64,
    ino: u64,
    entries: &[FuseRemoveMappingOne],
) -> Vec<u8> {
    let hdr = FuseRemoveMappingIn {
        count: entries.len() as u32,
    };
    let mut payload =
        Vec::with_capacity(FUSE_REMOVEMAPPING_IN_LEN + entries.len() * FUSE_REMOVEMAPPING_ONE_LEN);
    payload.extend_from_slice(&hdr.encode());
    for entry in entries {
        payload.extend_from_slice(&entry.encode());
    }
    build_request(unique, FuseOpcode::RemoveMapping, ino, &payload)
}

#[inline]
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

#[inline]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[inline]
fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[inline]
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}
