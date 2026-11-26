//! Symlink target classification and Windows `IO_REPARSE_TAG_SYMLINK`
//! buffer construction for `projgit-winfsp`.
//!
//! Windows distinguishes between file symlinks and directory symlinks at
//! the reparse-point metadata layer (`FILE_ATTRIBUTE_DIRECTORY` must be
//! set alongside `FILE_ATTRIBUTE_REPARSE_POINT` iff the symlink targets a
//! directory), whereas Git stores all symlinks under a single mode
//! (`120000`) whose blob payload is the raw target path.
//!
//! This module implements the target classifier and `REPARSE_DATA_BUFFER`
//! synthesizer specified in `docs/design/windows-symlinks.md` and
//! `docs/design/winfsp-implementation-plan.md` §4.3. It is pure safe Rust
//! and compiles on all platforms so unit tests run on both Linux and
//! Windows CI.

use projgit_core::fs_provider::{FileType, FsProvider, ROOT_INODE};

/// Windows reparse tag for NTFS / WinFsp symbolic links (`IO_REPARSE_TAG_SYMLINK`).
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// `SYMLINK_FLAG_RELATIVE` inside `REPARSE_DATA_BUFFER.SymbolicLinkReparseBuffer.Flags`.
pub const SYMLINK_FLAG_RELATIVE: u32 = 0x0000_0001;

/// Header byte size of `REPARSE_DATA_BUFFER` before `PathBuffer`
/// (`ReparseTag: u32`, `ReparseDataLength: u16`, `Reserved: u16`,
/// `SubstituteNameOffset: u16`, `SubstituteNameLength: u16`,
/// `PrintNameOffset: u16`, `PrintNameLength: u16`, `Flags: u32`).
pub const SYMLINK_REPARSE_HEADER_SIZE: usize = 20;

/// Maximum symlink resolution depth before treating a chain as cyclic.
pub const MAX_SYMLINK_HOPS: usize = 16;

/// How `projgit-winfsp` exposes Git mode-`120000` symlink entries to Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymlinkMode {
    /// Expose Git symlinks as native `IO_REPARSE_TAG_SYMLINK` reparse points.
    ///
    /// Because WinFsp synthesizes reparse points in memory over the FSD
    /// channel rather than creating NTFS on-disk symlinks, reading and
    /// traversing them does **not** require `SeCreateSymbolicLinkPrivilege`
    /// or Windows Developer Mode.
    #[default]
    NativeReparse,
    /// Expose Git symlinks as read-only regular files containing the raw
    /// target string (matching `git config core.symlinks=false`).
    PlainFile,
}

/// Classification of a Git mode-`120000` symlink target within the projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkKind {
    /// Target resolves in-tree to a regular file (or executable file).
    File,
    /// Target resolves in-tree to a directory (`FILE_ATTRIBUTE_DIRECTORY`
    /// must be set alongside `FILE_ATTRIBUTE_REPARSE_POINT`).
    Directory,
    /// Target is dangling, absolute, escapes the projection root, or hits
    /// a symlink cycle; Windows treats it as a file-flavored symlink so
    /// `readlink` / `git status` still work without breaking directory
    /// traversal.
    DanglingAssumeFile,
}

impl SymlinkKind {
    /// Returns `true` when the symlink should carry `FILE_ATTRIBUTE_DIRECTORY`.
    #[must_use]
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// Returns `true` if `target` is a relative symlink target (does not begin
/// with `/`, `\`, or a Windows drive prefix like `C:`).
#[must_use]
pub fn is_relative_symlink_target(target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') || target.starts_with('\\') {
        return false;
    }
    let bytes = target.as_bytes();
    !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Classify a Git symlink (`raw_target`) located inside the directory chain
/// `parent_chain` (where `parent_chain[0] == ROOT_INODE` and
/// `parent_chain.last()` is the immediate parent directory of the symlink).
pub fn classify_symlink_target<P: FsProvider + ?Sized>(
    provider: &P,
    parent_chain: &[u64],
    raw_target: &[u8],
) -> SymlinkKind {
    let Ok(initial_target) = std::str::from_utf8(raw_target) else {
        return SymlinkKind::DanglingAssumeFile;
    };
    if initial_target.is_empty() || initial_target.contains('\0') {
        return SymlinkKind::DanglingAssumeFile;
    }

    let mut dir_stack: Vec<u64> = if parent_chain.is_empty() {
        vec![ROOT_INODE]
    } else {
        parent_chain.to_vec()
    };
    let mut current_target = initial_target.to_owned();

    for _hop in 0..MAX_SYMLINK_HOPS {
        if !is_relative_symlink_target(&current_target) {
            return SymlinkKind::DanglingAssumeFile;
        }

        let normalized = current_target.replace('\\', "/");
        let trailing_slash = normalized.ends_with('/');
        let components: Vec<&str> = normalized
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();

        if components.is_empty() {
            return SymlinkKind::Directory;
        }

        let mut followed_symlink = None;
        for (idx, &comp) in components.iter().enumerate() {
            let is_last = idx + 1 == components.len();
            if comp == ".." {
                if dir_stack.len() <= 1 {
                    // Escapes above the projection root.
                    return SymlinkKind::DanglingAssumeFile;
                }
                dir_stack.pop();
                if is_last {
                    return SymlinkKind::Directory;
                }
                continue;
            }

            let parent_ino = *dir_stack.last().unwrap_or(&ROOT_INODE);
            let Ok(attr) = provider.lookup(parent_ino, comp.as_bytes()) else {
                return SymlinkKind::DanglingAssumeFile;
            };

            match attr.kind {
                FileType::Directory => {
                    if is_last {
                        return SymlinkKind::Directory;
                    }
                    dir_stack.push(attr.inode);
                }
                FileType::RegularFile => {
                    if is_last && !trailing_slash {
                        return SymlinkKind::File;
                    }
                    return SymlinkKind::DanglingAssumeFile;
                }
                FileType::Symlink => {
                    let Ok(next_bytes) = provider.readlink(attr.inode) else {
                        return SymlinkKind::DanglingAssumeFile;
                    };
                    let Ok(next_str) = std::str::from_utf8(next_bytes.as_slice()) else {
                        return SymlinkKind::DanglingAssumeFile;
                    };
                    if is_last {
                        followed_symlink = Some(next_str.to_owned());
                    } else {
                        let remaining = components[idx + 1..].join("/");
                        let combined = format!("{}/{}", next_str.trim_end_matches('/'), remaining);
                        followed_symlink = Some(combined);
                    }
                    break;
                }
            }
        }

        if let Some(next_target) = followed_symlink {
            current_target = next_target;
        } else {
            return SymlinkKind::DanglingAssumeFile;
        }
    }

    SymlinkKind::DanglingAssumeFile
}

/// Build a Windows `REPARSE_DATA_BUFFER` for `IO_REPARSE_TAG_SYMLINK` from a
/// UTF-8 Git symlink target.
///
/// Forward slashes (`/`) are normalized to Windows backslashes (`\`) in both
/// `SubstituteName` and `PrintName`. Relative targets set
/// `SYMLINK_FLAG_RELATIVE`.
pub fn build_symlink_reparse_buffer(raw_target: &[u8]) -> Result<Vec<u8>, String> {
    let target_str = std::str::from_utf8(raw_target)
        .map_err(|e| format!("symlink target is not valid UTF-8: {e}"))?;
    if target_str.is_empty() {
        return Err("symlink target is empty".to_owned());
    }

    let relative = is_relative_symlink_target(target_str);
    let win_target = target_str.replace('/', "\\");
    let utf16_units: Vec<u16> = win_target.encode_utf16().collect();
    let name_byte_len = utf16_units
        .len()
        .checked_mul(2)
        .ok_or_else(|| "symlink target UTF-16 length overflow".to_owned())?;
    let name_byte_len_u16 = u16::try_from(name_byte_len)
        .map_err(|_| "symlink target exceeds maximum REPARSE_DATA_BUFFER length".to_owned())?;

    // SymbolicLinkReparseBuffer has 12 bytes of fixed fields before PathBuffer,
    // followed by SubstituteName and PrintName.
    let reparse_data_len = 12u16
        .checked_add(name_byte_len_u16)
        .and_then(|n| n.checked_add(name_byte_len_u16))
        .ok_or_else(|| "symlink REPARSE_DATA_BUFFER size overflow".to_owned())?;

    let total_len = SYMLINK_REPARSE_HEADER_SIZE + name_byte_len * 2;
    let mut buf = Vec::with_capacity(total_len);

    // ReparseTag (u32)
    buf.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    // ReparseDataLength (u16)
    buf.extend_from_slice(&reparse_data_len.to_le_bytes());
    // Reserved (u16)
    buf.extend_from_slice(&0u16.to_le_bytes());
    // SubstituteNameOffset (u16) = 0
    buf.extend_from_slice(&0u16.to_le_bytes());
    // SubstituteNameLength (u16)
    buf.extend_from_slice(&name_byte_len_u16.to_le_bytes());
    // PrintNameOffset (u16) = SubstituteNameLength
    buf.extend_from_slice(&name_byte_len_u16.to_le_bytes());
    // PrintNameLength (u16)
    buf.extend_from_slice(&name_byte_len_u16.to_le_bytes());
    // Flags (u32)
    let flags = if relative { SYMLINK_FLAG_RELATIVE } else { 0 };
    buf.extend_from_slice(&flags.to_le_bytes());

    // SubstituteName followed by PrintName
    for &unit in &utf16_units {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    for &unit in &utf16_units {
        buf.extend_from_slice(&unit.to_le_bytes());
    }

    debug_assert_eq!(buf.len(), total_len);
    Ok(buf)
}

/// Parse a `REPARSE_DATA_BUFFER` produced by [`build_symlink_reparse_buffer`]
/// (or by WinFsp/NTFS), returning `(print_name, is_relative)`.
#[must_use]
pub fn parse_symlink_reparse_buffer(buf: &[u8]) -> Option<(String, bool)> {
    if buf.len() < SYMLINK_REPARSE_HEADER_SIZE {
        return None;
    }
    let tag = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if tag != IO_REPARSE_TAG_SYMLINK {
        return None;
    }
    let data_len = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    if buf.len() < 8 + data_len || data_len < 12 {
        return None;
    }
    let sub_off = u16::from_le_bytes([buf[8], buf[9]]) as usize;
    let sub_len = u16::from_le_bytes([buf[10], buf[11]]) as usize;
    let print_off = u16::from_le_bytes([buf[12], buf[13]]) as usize;
    let print_len = u16::from_le_bytes([buf[14], buf[15]]) as usize;
    let flags = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);

    let path_buf = &buf[SYMLINK_REPARSE_HEADER_SIZE..8 + data_len];
    let (off, len) = if print_len > 0 && print_off + print_len <= path_buf.len() {
        (print_off, print_len)
    } else if sub_off + sub_len <= path_buf.len() {
        (sub_off, sub_len)
    } else {
        return None;
    };
    if len % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = path_buf[off..off + len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let text = String::from_utf16(&units).ok()?;
    Some((text, (flags & SYMLINK_FLAG_RELATIVE) != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use projgit_core::InMemoryFsProvider;

    #[test]
    fn reparse_buffer_roundtrips_relative_and_absolute_targets() {
        let buf = build_symlink_reparse_buffer(b"../shared/config.toml").expect("build reparse");
        let (target, is_rel) = parse_symlink_reparse_buffer(&buf).expect("parse reparse");
        assert_eq!(target, "..\\shared\\config.toml");
        assert!(is_rel);

        let abs = build_symlink_reparse_buffer(b"C:\\Windows\\System32").expect("build abs");
        let (abs_target, abs_rel) = parse_symlink_reparse_buffer(&abs).expect("parse abs");
        assert_eq!(abs_target, "C:\\Windows\\System32");
        assert!(!abs_rel);
    }

    #[test]
    fn classify_symlink_resolves_files_dirs_chains_loops_and_escapes() {
        let provider = InMemoryFsProvider::new();
        let src_dir = provider.add_dir(ROOT_INODE, b"src");
        let nested_dir = provider.add_dir(src_dir, b"nested");
        provider.add_file(src_dir, b"lib.rs", b"pub fn hi() {}".to_vec());
        provider.add_file(ROOT_INODE, b"README.md", b"# readme".to_vec());
        provider.add_symlink(ROOT_INODE, b"link_to_src", b"src");
        provider.add_symlink(ROOT_INODE, b"loop_a", b"loop_b");
        provider.add_symlink(ROOT_INODE, b"loop_b", b"loop_a");

        // File target inside root and via `..` from `src`.
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE], b"README.md"),
            SymlinkKind::File
        );
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE, src_dir], b"../README.md"),
            SymlinkKind::File
        );

        // Directory target inside root and via `..` from `src/nested`.
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE], b"src"),
            SymlinkKind::Directory
        );
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE, src_dir, nested_dir], b".."),
            SymlinkKind::Directory
        );

        // Chained symlink (`link_to_src/lib.rs` -> `src/lib.rs`).
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE], b"link_to_src"),
            SymlinkKind::Directory
        );
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE], b"link_to_src/lib.rs"),
            SymlinkKind::File
        );

        // Escaping root (`../../outside`), dangling target, absolute path, and loops.
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE, src_dir], b"../../outside"),
            SymlinkKind::DanglingAssumeFile
        );
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE], b"missing.txt"),
            SymlinkKind::DanglingAssumeFile
        );
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE], b"/etc/passwd"),
            SymlinkKind::DanglingAssumeFile
        );
        assert_eq!(
            classify_symlink_target(&provider, &[ROOT_INODE], b"loop_a"),
            SymlinkKind::DanglingAssumeFile
        );
    }
}
