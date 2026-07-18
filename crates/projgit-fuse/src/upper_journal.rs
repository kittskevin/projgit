//! Crash-recoverable persistence for the writable **upper** layer
//! (Phase 2, design §10.3 "overlay crash consistency").
//!
//! The upper (materialized/created files + whiteouts) is otherwise
//! in-memory and lost on unmount. This module makes it durable with the
//! design's content-addressed model (§6): each mutation appends a small
//! record to an append-only `journal`, and file bytes are written to a
//! content-addressed `blobs/` store keyed by git blob object id (so
//! identical content dedups and the names line up with git's odb).
//!
//! ## Crash consistency
//!
//! - Blobs are written temp-then-rename, so a torn blob never appears
//!   under its final (content-addressed) name.
//! - The journal is append-only and `fsync`ed per record, so a crash
//!   mid-write loses at most the in-flight record; [`replay`] skips a
//!   malformed trailing (torn) line.
//! - The immutable LOWER baseline means replay is *reconciled* against
//!   the current baseline by the caller (committed edits drop out, so
//!   the journal can be compacted — see [`UpperJournal::compact`]).
//!
//! The record grammar is line-oriented; the (arbitrary-bytes) path is
//! hex-encoded so it is delimiter- and newline-safe:
//!
//! ```text
//! S <blob-oid> <mode-octal> <f|l> <path-hex>   set file(f)/symlink(l)
//! D <mode-octal> <path-hex>                     created directory
//! W <path-hex>                                  whiteout (delete)
//! ```

use projgit_core::FileType;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One replayed upper mutation, in journal order.
pub(crate) enum Record {
    /// Set a regular file or symlink at `path` to `content` with `mode`.
    Set {
        /// Worktree-relative path.
        path: String,
        /// `RegularFile` or `Symlink`.
        kind: FileType,
        /// Unix mode bits (low 9).
        mode: u16,
        /// File bytes, or the symlink target for a symlink.
        content: Vec<u8>,
    },
    /// A created directory at `path`.
    Mkdir {
        /// Worktree-relative path.
        path: String,
        /// Unix mode bits (low 9).
        mode: u16,
    },
    /// A whiteout: `path` is deleted/hidden from the projection.
    Whiteout {
        /// Worktree-relative path.
        path: String,
    },
}

/// Append-only, crash-recoverable persistence for the writable upper.
pub(crate) struct UpperJournal {
    blobs: PathBuf,
    journal_path: PathBuf,
    log: Mutex<File>,
}

impl UpperJournal {
    /// Open (creating if needed) the journal + blob store under `dir`.
    pub(crate) fn open(dir: &Path) -> std::io::Result<Self> {
        let blobs = dir.join("blobs");
        fs::create_dir_all(&blobs)?;
        let journal_path = dir.join("journal");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal_path)?;
        Ok(Self {
            blobs,
            journal_path,
            log: Mutex::new(log),
        })
    }

    /// Record a set (create/modify) of a file or symlink. Writes the
    /// content-addressed blob, then appends + `fsync`s the record.
    pub(crate) fn record_set(&self, path: &str, kind: FileType, mode: u16, content: &[u8]) {
        let oid = match self.write_blob(content) {
            Ok(o) => o,
            Err(_) => return,
        };
        let k = if matches!(kind, FileType::Symlink) { 'l' } else { 'f' };
        self.append(&format!("S {oid} {mode:o} {k} {}", hex_encode(path.as_bytes())));
    }

    /// Record a created directory.
    pub(crate) fn record_mkdir(&self, path: &str, mode: u16) {
        self.append(&format!("D {mode:o} {}", hex_encode(path.as_bytes())));
    }

    /// Record a whiteout (deletion).
    pub(crate) fn record_whiteout(&self, path: &str) {
        self.append(&format!("W {}", hex_encode(path.as_bytes())));
    }

    /// Content-address `content` into the blob store (temp + rename),
    /// returning its git blob oid hex. A no-op if already present.
    fn write_blob(&self, content: &[u8]) -> std::io::Result<String> {
        let oid = projgit_core::blob_oid_hex(content);
        let dst = self.blobs.join(&oid);
        if !dst.exists() {
            let tmp = self.blobs.join(format!(".tmp-{}", oid));
            fs::write(&tmp, content)?;
            fs::rename(&tmp, &dst)?;
        }
        Ok(oid)
    }

    /// Append a record line and flush it durably.
    fn append(&self, line: &str) {
        if let Ok(mut f) = self.log.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
            let _ = f.sync_data();
        }
    }

    /// Rewrite the journal as a compact snapshot of the still-live
    /// `sets` (whiteouts included via [`Record::Whiteout`]) after a
    /// reconcile dropped committed/absent entries, then **garbage-collect
    /// stale blobs** — content-addressed blobs no longer referenced by
    /// the compacted journal (e.g. earlier versions of a re-edited file).
    /// Crash-safe: writes a sibling temp file, then renames over the
    /// journal; GC runs only after the rename, so a crash leaves at most
    /// unreferenced blobs (reclaimed on the next compaction).
    pub(crate) fn compact(&self, records: &[Record]) -> std::io::Result<()> {
        let tmp = self.journal_path.with_extension("compact");
        let mut live: HashSet<String> = HashSet::new();
        {
            let mut f = File::create(&tmp)?;
            for r in records {
                match r {
                    Record::Set { path, kind, mode, content } => {
                        let oid = self.write_blob(content)?;
                        live.insert(oid.clone());
                        let k = if matches!(kind, FileType::Symlink) { 'l' } else { 'f' };
                        writeln!(f, "S {oid} {mode:o} {k} {}", hex_encode(path.as_bytes()))?;
                    }
                    Record::Mkdir { path, mode } => {
                        writeln!(f, "D {mode:o} {}", hex_encode(path.as_bytes()))?;
                    }
                    Record::Whiteout { path } => {
                        writeln!(f, "W {}", hex_encode(path.as_bytes()))?;
                    }
                }
            }
            f.sync_data()?;
        }
        fs::rename(&tmp, &self.journal_path)?;
        // GC: drop blobs not referenced by the compacted journal (skip
        // in-flight temp files).
        if let Ok(entries) = fs::read_dir(&self.blobs) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".tmp-") || live.contains(name.as_ref()) {
                    continue;
                }
                let _ = fs::remove_file(e.path());
            }
        }
        // Reopen the appender on the freshly compacted file.
        if let Ok(mut guard) = self.log.lock() {
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.journal_path)?;
        }
        Ok(())
    }
}

/// Replay the on-disk journal under `dir` into ordered [`Record`]s.
///
/// Missing journal => empty. Malformed / torn (crash) lines and records
/// whose blob is missing are skipped, so a partially-written tail never
/// aborts recovery.
pub(crate) fn replay(dir: &Path) -> Vec<Record> {
    let mut out = Vec::new();
    let blobs = dir.join("blobs");
    let f = match File::open(dir.join("journal")) {
        Ok(f) => f,
        Err(_) => return out,
    };
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split(' ');
        match it.next() {
            Some("S") => {
                let (Some(oid), Some(mode), Some(kind), Some(path_hex)) =
                    (it.next(), it.next(), it.next(), it.next())
                else {
                    continue;
                };
                let (Some(mode), Some(path)) = (parse_mode(mode), hex_decode(path_hex)) else {
                    continue;
                };
                let content = match fs::read(blobs.join(oid)) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let kind = if kind == "l" {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                };
                out.push(Record::Set { path, kind, mode, content });
            }
            Some("D") => {
                let (Some(mode), Some(path_hex)) = (it.next(), it.next()) else {
                    continue;
                };
                let (Some(mode), Some(path)) = (parse_mode(mode), hex_decode(path_hex)) else {
                    continue;
                };
                out.push(Record::Mkdir { path, mode });
            }
            Some("W") => {
                let Some(path_hex) = it.next() else { continue };
                let Some(path) = hex_decode(path_hex) else {
                    continue;
                };
                out.push(Record::Whiteout { path });
            }
            _ => {}
        }
    }
    out
}

fn parse_mode(s: &str) -> Option<u16> {
    u16::from_str_radix(s, 8).ok()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

fn hex_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).ok()
}
