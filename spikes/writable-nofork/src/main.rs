//! Phase 2 spike — `vworktree`: a throwaway **virtual worktree** FUSE
//! filesystem used to answer the no-fork thesis in
//! `docs/design/writable-worktrees.md` §10.1.
//!
//! # What it is
//!
//! A minimal overlay filesystem whose LOWER layer is a git commit's
//! tree (files are *virtual* — their bytes are fetched from the object
//! store only when `read()` is actually called) and whose UPPER layer
//! is an in-memory materialisation store for writes. It is deliberately
//! NOT projgit's production FUSE backend (that one is hard-wired
//! read-only); this is a self-contained harness so the spike can test
//! the *write* path too.
//!
//! # Why it exists
//!
//! To measure, on real `git` (stock, unmodified, NO
//! `core.virtualFilesystem`):
//!   1. Does `git status` over this mount **hydrate** file content
//!      (i.e. cause `read()` of blobs)? — the load-bearing "cheap
//!      getattr is enough" claim. We count every lower-layer `read()`.
//!   2. Does sparse-index keep the index small / status fast at scale?
//!   3. Can a daemon-style FSMonitor hook make status skip scanning?
//!   4. Does materialise-on-write let `git add` + `git commit` work?
//!
//! # Instrumentation
//!
//! A hidden control file (`.nofork-stats`, resolvable by name but never
//! listed in `readdir`, so `git` never sees it) returns cumulative
//! counters as `key value` lines. The driver (`run.sh`) reads it before
//! and after each phase and diffs. The single most important counter is
//! `reads` — the number of `read()` calls served from the LOWER (git)
//! layer, i.e. real hydration events.
//!
//! # Not production code
//!
//! Single global `Mutex`, in-memory upper layer, `git cat-file --batch`
//! child for object bytes, panics turned into EIO. Throwaway by design.

use anyhow::{bail, Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType as FuserKind, Filesystem, FopenFlags, Generation,
    INodeNo, MountOption, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ROOT_INO: u64 = 1;
const STATS_INO: u64 = u64::MAX;
const STATS_NAME: &str = ".nofork-stats";
/// Attr/entry cache TTL we hand the kernel. We deliberately use ZERO so
/// the kernel never serves a *stale* getattr after a write materialises
/// new content. A production VFS would instead keep a longer TTL and
/// push a FUSE invalidation (`Notifier::inval_inode`) on every write —
/// that is exactly the cache-coherence seam called out in
/// writable-worktrees.md §10.7. TTL=0 is the throwaway-spike shortcut.
const TTL: Duration = Duration::ZERO;
const GEN: Generation = Generation(0);
/// Stable mtime reported for every un-materialised (lower) entry, so a
/// clean worktree presents stable stat data to git's index refresh.
const LOWER_MTIME: SystemTime = UNIX_EPOCH;

// ---------------------------------------------------------------------------
// Tree model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
}

struct Node {
    parent: u64,
    name: String,
    path: String, // full path from mount root; "" for root
    kind: Kind,
    oid: Option<String>, // lower object id (None for newly-created)
    size: u64,
    mode: u16, // unix perm bits (0o644 / 0o755 / 0o777)
    children: BTreeMap<String, u64>,
}

#[derive(Default)]
struct Counters {
    lookups: u64,
    getattrs: u64,
    readdirs: u64,
    reads: u64,        // read() served from LOWER (git) = hydration
    upper_reads: u64,  // read() served from UPPER (materialised)
    hydrations: u64,   // unique blobs fetched from object store
    hydrated_bytes: u64,
    writes: u64,
    creates: u64,
    materializations: u64,
}

struct State {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
    /// Materialised file content, keyed by inode.
    upper: HashMap<u64, Vec<u8>>,
    /// Inodes deleted in the upper layer (whiteouts).
    deleted: HashSet<u64>,
    /// Per-oid hydration cache so repeat reads don't re-fetch and so we
    /// can count *unique* hydrations.
    blob_cache: HashMap<String, Vec<u8>>,
    /// Paths changed since mount — the FSMonitor "write log".
    modified: BTreeSet<String>,
    /// Monotonic FSMonitor token. MUST advance whenever the worktree
    /// changes, or git treats the monitor state as unchanged and
    /// ignores the reported paths.
    fsm_token: u64,
    counters: Counters,
}

struct Vfs {
    state: Mutex<State>,
    catfile: Mutex<CatFile>,
    fsmonitor_file: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// git cat-file --batch child (lazy blob bytes)
// ---------------------------------------------------------------------------

struct CatFile {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    fn spawn(git_dir: &str) -> Result<Self> {
        let mut child = Command::new("git")
            .args(["--git-dir", git_dir, "cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn git cat-file --batch")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            _child: child,
            stdin,
            stdout,
        })
    }

    /// Fetch a blob's bytes by oid via the batch protocol.
    fn blob(&mut self, oid: &str) -> Result<Vec<u8>> {
        self.stdin.write_all(oid.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        // "<oid> <type> <size>\n" or "<oid> missing\n"
        let parts: Vec<&str> = header.trim_end().split(' ').collect();
        if parts.len() != 3 {
            bail!("cat-file unexpected header: {header:?}");
        }
        let size: usize = parts[2].parse().context("blob size")?;
        let mut buf = vec![0u8; size];
        self.stdout.read_exact(&mut buf)?;
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl)?; // trailing newline
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Build the lower tree from `git ls-tree -r -l -z`
// ---------------------------------------------------------------------------

fn build_tree(git_dir: &str, commit: &str) -> Result<(HashMap<u64, Node>, u64)> {
    let out = Command::new("git")
        .args(["--git-dir", git_dir, "ls-tree", "-r", "-l", "-z", commit])
        .output()
        .context("git ls-tree")?;
    if !out.status.success() {
        bail!(
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let mut nodes: HashMap<u64, Node> = HashMap::new();
    nodes.insert(
        ROOT_INO,
        Node {
            parent: ROOT_INO,
            name: String::new(),
            path: String::new(),
            kind: Kind::Dir,
            oid: None,
            size: 0,
            mode: 0o755,
            children: BTreeMap::new(),
        },
    );
    let mut next_ino: u64 = 2;

    for record in out.stdout.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        // "<mode> <type> <oid> <size>\t<path>"
        let (meta, path) = match record.split_once('\t') {
            Some(x) => x,
            None => continue,
        };
        let fields: Vec<&str> = meta.split_whitespace().collect();
        if fields.len() != 4 {
            continue;
        }
        let (gitmode, typ, oid) = (fields[0], fields[1], fields[2]);
        if typ == "commit" {
            continue; // submodule gitlink — skip
        }
        let size: u64 = fields[3].parse().unwrap_or(0);
        let (kind, mode) = match gitmode {
            "120000" => (Kind::Symlink, 0o777u16),
            "100755" => (Kind::File, 0o755),
            "120755" => (Kind::Symlink, 0o777),
            _ => (Kind::File, 0o644),
        };
        insert_path(
            &mut nodes,
            &mut next_ino,
            path,
            kind,
            oid.to_string(),
            size,
            mode,
        );
    }
    Ok((nodes, next_ino))
}

#[allow(clippy::too_many_arguments)]
fn insert_path(
    nodes: &mut HashMap<u64, Node>,
    next_ino: &mut u64,
    path: &str,
    kind: Kind,
    oid: String,
    size: u64,
    mode: u16,
) {
    let comps: Vec<&str> = path.split('/').collect();
    let mut parent = ROOT_INO;
    for (i, comp) in comps.iter().enumerate() {
        let last = i == comps.len() - 1;
        if last {
            let ino = *next_ino;
            *next_ino += 1;
            let full = if nodes[&parent].path.is_empty() {
                comp.to_string()
            } else {
                format!("{}/{}", nodes[&parent].path, comp)
            };
            nodes.insert(
                ino,
                Node {
                    parent,
                    name: comp.to_string(),
                    path: full,
                    kind,
                    oid: Some(oid.clone()),
                    size,
                    mode,
                    children: BTreeMap::new(),
                },
            );
            nodes
                .get_mut(&parent)
                .unwrap()
                .children
                .insert(comp.to_string(), ino);
        } else {
            // ensure dir exists
            let existing = nodes[&parent].children.get(*comp).copied();
            parent = match existing {
                Some(ino) => ino,
                None => {
                    let ino = *next_ino;
                    *next_ino += 1;
                    let full = if nodes[&parent].path.is_empty() {
                        comp.to_string()
                    } else {
                        format!("{}/{}", nodes[&parent].path, comp)
                    };
                    nodes.insert(
                        ino,
                        Node {
                            parent,
                            name: comp.to_string(),
                            path: full,
                            kind: Kind::Dir,
                            oid: None,
                            size: 0,
                            mode: 0o755,
                            children: BTreeMap::new(),
                        },
                    );
                    nodes
                        .get_mut(&parent)
                        .unwrap()
                        .children
                        .insert(comp.to_string(), ino);
                    ino
                }
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Vfs helpers
// ---------------------------------------------------------------------------

impl Vfs {
    /// Current logical size of a node (upper if materialised, else lower).
    fn size_of(st: &State, ino: u64) -> u64 {
        if let Some(buf) = st.upper.get(&ino) {
            return buf.len() as u64;
        }
        st.nodes.get(&ino).map(|n| n.size).unwrap_or(0)
    }

    fn attr(st: &State, ino: u64) -> Option<FileAttr> {
        if ino == STATS_INO {
            return Some(file_attr(STATS_INO, Kind::File, 1 << 16, 0o444, LOWER_MTIME));
        }
        let node = st.nodes.get(&ino)?;
        let size = Self::size_of(st, ino);
        let mtime = if st.upper.contains_key(&ino) {
            SystemTime::now()
        } else {
            LOWER_MTIME
        };
        Some(file_attr(ino, node.kind, size, node.mode, mtime))
    }

    /// Fetch lower blob bytes, caching + counting unique hydrations.
    fn hydrate(&self, st: &mut State, oid: &str) -> Result<Vec<u8>> {
        if let Some(b) = st.blob_cache.get(oid) {
            return Ok(b.clone());
        }
        let bytes = self.catfile.lock().unwrap().blob(oid)?;
        st.counters.hydrations += 1;
        st.counters.hydrated_bytes += bytes.len() as u64;
        st.blob_cache.insert(oid.to_string(), bytes.clone());
        Ok(bytes)
    }

    /// Ensure an inode's content is materialised into the upper layer.
    fn materialize(&self, st: &mut State, ino: u64) -> Result<()> {
        if st.upper.contains_key(&ino) {
            return Ok(());
        }
        let oid = st.nodes.get(&ino).and_then(|n| n.oid.clone());
        let content = match oid {
            Some(oid) => self.hydrate(st, &oid)?,
            None => Vec::new(),
        };
        st.upper.insert(ino, content);
        st.counters.materializations += 1;
        Ok(())
    }

    fn mark_modified(&self, st: &mut State, path: &str) {
        st.modified.insert(path.to_string());
        // Strictly-increasing, timestamp-shaped token.
        st.fsm_token = now_nanos().max(st.fsm_token + 1);
        if let Some(p) = &self.fsmonitor_file {
            write_fsm(p, st.fsm_token, &st.modified);
        }
    }

    fn stats_blob(st: &State) -> Vec<u8> {
        let c = &st.counters;
        format!(
            "lookups {}\ngetattrs {}\nreaddirs {}\nreads {}\nupper_reads {}\nhydrations {}\nhydrated_bytes {}\nwrites {}\ncreates {}\nmaterializations {}\nmodified_paths {}\n",
            c.lookups, c.getattrs, c.readdirs, c.reads, c.upper_reads, c.hydrations,
            c.hydrated_bytes, c.writes, c.creates, c.materializations, st.modified.len()
        )
        .into_bytes()
    }
}

/// Monotonic nanosecond clock for FSMonitor tokens. git's hook protocol
/// treats the token as opaque, but empirically it REJECTS small-integer
/// tokens (stores `0` and ignores all deltas); timestamp-shaped tokens
/// are accepted and advanced. So we mint nanosecond tokens.
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Write the FSMonitor "write log": `<token>\0 <path>\0 ...`. The hook
/// streams this verbatim to git. An advancing token + a (superset) list
/// of changed paths is all the fsmonitor v2 protocol needs.
fn write_fsm(path: &std::path::Path, token: u64, modified: &BTreeSet<String>) {
    let mut buf = Vec::new();
    buf.extend_from_slice(token.to_string().as_bytes());
    buf.push(0);
    for m in modified {
        buf.extend_from_slice(m.as_bytes());
        buf.push(0);
    }
    let _ = std::fs::write(path, &buf);
}

fn file_attr(ino: u64, kind: Kind, size: u64, mode: u16, mtime: SystemTime) -> FileAttr {
    let fkind = match kind {
        Kind::Dir => FuserKind::Directory,
        Kind::File => FuserKind::RegularFile,
        Kind::Symlink => FuserKind::Symlink,
    };
    let nlink = if matches!(kind, Kind::Dir) { 2 } else { 1 };
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: fkind,
        perm: mode,
        nlink,
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

// ---------------------------------------------------------------------------
// Filesystem impl
// ---------------------------------------------------------------------------

impl Filesystem for Vfs {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEntry) {
        let name = String::from_utf8_lossy(name.as_bytes()).to_string();
        let mut st = self.state.lock().unwrap();
        st.counters.lookups += 1;
        // hidden control file: resolvable by name, never listed
        if parent.0 == ROOT_INO && name == STATS_NAME {
            let a = Vfs::attr(&st, STATS_INO).unwrap();
            reply.entry(&TTL, &a, GEN);
            return;
        }
        let child = st
            .nodes
            .get(&parent.0)
            .and_then(|p| p.children.get(&name).copied());
        match child {
            Some(ino) if !st.deleted.contains(&ino) => {
                let a = Vfs::attr(&st, ino).unwrap();
                let _ = req;
                reply.entry(&TTL, &a, GEN);
            }
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let mut st = self.state.lock().unwrap();
        st.counters.getattrs += 1;
        match Vfs::attr(&st, ino.0) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let mut st = self.state.lock().unwrap();
        let oid = match st.nodes.get(&ino.0) {
            Some(n) if matches!(n.kind, Kind::Symlink) => n.oid.clone(),
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        if let Some(buf) = st.upper.get(&ino.0) {
            let b = buf.clone();
            reply.data(&b);
            return;
        }
        match oid {
            Some(oid) => match self.hydrate(&mut st, &oid) {
                Ok(b) => reply.data(&b),
                Err(_) => reply.error(Errno::EIO),
            },
            None => reply.data(b""),
        }
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let mut st = self.state.lock().unwrap();
        if ino.0 == STATS_INO {
            let blob = Vfs::stats_blob(&st);
            let start = (offset as usize).min(blob.len());
            let end = (start + size as usize).min(blob.len());
            reply.data(&blob[start..end]);
            return;
        }
        // upper first
        if st.upper.contains_key(&ino.0) {
            st.counters.upper_reads += 1;
            let buf = st.upper.get(&ino.0).unwrap();
            let start = (offset as usize).min(buf.len());
            let end = (start + size as usize).min(buf.len());
            let slice = buf[start..end].to_vec();
            reply.data(&slice);
            return;
        }
        // lower: this is a HYDRATION event
        let oid = match st.nodes.get(&ino.0) {
            Some(n) if matches!(n.kind, Kind::File) => n.oid.clone(),
            Some(_) => {
                reply.error(Errno::EISDIR);
                return;
            }
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        st.counters.reads += 1;
        match oid {
            Some(oid) => match self.hydrate(&mut st, &oid) {
                Ok(buf) => {
                    let start = (offset as usize).min(buf.len());
                    let end = (start + size as usize).min(buf.len());
                    reply.data(&buf[start..end]);
                }
                Err(_) => reply.error(Errno::EIO),
            },
            None => reply.data(b""),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut st = self.state.lock().unwrap();
        st.counters.readdirs += 1;
        let node = match st.nodes.get(&ino.0) {
            Some(n) if matches!(n.kind, Kind::Dir) => n,
            Some(_) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let mut entries: Vec<(u64, FuserKind, String)> = Vec::new();
        entries.push((ino.0, FuserKind::Directory, ".".to_string()));
        entries.push((node.parent, FuserKind::Directory, "..".to_string()));
        for (name, &cino) in &node.children {
            if st.deleted.contains(&cino) {
                continue;
            }
            let k = match st.nodes.get(&cino).map(|n| n.kind) {
                Some(Kind::Dir) => FuserKind::Directory,
                Some(Kind::Symlink) => FuserKind::Symlink,
                _ => FuserKind::RegularFile,
            };
            entries.push((cino, k, name.clone()));
        }
        // NOTE: `.nofork-stats` is deliberately NOT listed here.
        for (i, (cino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            let full = reply.add(
                INodeNo(*cino),
                (i + 1) as u64,
                *kind,
                OsStr::from_bytes(name.as_bytes()),
            );
            if full {
                break;
            }
        }
        reply.ok();
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut st = self.state.lock().unwrap();
        if let Some(newsize) = size {
            if self.materialize(&mut st, ino.0).is_err() {
                reply.error(Errno::EIO);
                return;
            }
            let buf = st.upper.get_mut(&ino.0).unwrap();
            buf.resize(newsize as usize, 0);
            let path = st.nodes.get(&ino.0).map(|n| n.path.clone()).unwrap_or_default();
            self.mark_modified(&mut st, &path);
        }
        match Vfs::attr(&st, ino.0) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut st = self.state.lock().unwrap();
        if self.materialize(&mut st, ino.0).is_err() {
            reply.error(Errno::EIO);
            return;
        }
        st.counters.writes += 1;
        let buf = st.upper.get_mut(&ino.0).unwrap();
        let start = offset as usize;
        let end = start + data.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[start..end].copy_from_slice(data);
        let path = st.nodes.get(&ino.0).map(|n| n.path.clone()).unwrap_or_default();
        self.mark_modified(&mut st, &path);
        reply.written(data.len() as u32);
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name = String::from_utf8_lossy(name.as_bytes()).to_string();
        let mut st = self.state.lock().unwrap();
        let ino = st.next_ino;
        st.next_ino += 1;
        let full = {
            let p = &st.nodes[&parent.0];
            if p.path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", p.path, name)
            }
        };
        st.nodes.insert(
            ino,
            Node {
                parent: parent.0,
                name: name.clone(),
                path: full.clone(),
                kind: Kind::File,
                oid: None,
                size: 0,
                mode: (mode as u16) & 0o777,
                children: BTreeMap::new(),
            },
        );
        st.nodes
            .get_mut(&parent.0)
            .unwrap()
            .children
            .insert(name, ino);
        st.upper.insert(ino, Vec::new());
        st.deleted.remove(&ino);
        st.counters.creates += 1;
        self.mark_modified(&mut st, &full);
        let a = Vfs::attr(&st, ino).unwrap();
        reply.created(&TTL, &a, GEN, FileHandle(0), FopenFlags::empty());
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let name = String::from_utf8_lossy(name.as_bytes()).to_string();
        let mut st = self.state.lock().unwrap();
        let ino = st.next_ino;
        st.next_ino += 1;
        let full = {
            let p = &st.nodes[&parent.0];
            if p.path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", p.path, name)
            }
        };
        st.nodes.insert(
            ino,
            Node {
                parent: parent.0,
                name: name.clone(),
                path: full,
                kind: Kind::Dir,
                oid: None,
                size: 0,
                mode: 0o755,
                children: BTreeMap::new(),
            },
        );
        st.nodes
            .get_mut(&parent.0)
            .unwrap()
            .children
            .insert(name, ino);
        let a = Vfs::attr(&st, ino).unwrap();
        reply.entry(&TTL, &a, GEN);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = String::from_utf8_lossy(name.as_bytes()).to_string();
        let mut st = self.state.lock().unwrap();
        let child = st
            .nodes
            .get(&parent.0)
            .and_then(|p| p.children.get(&name).copied());
        match child {
            Some(ino) => {
                st.deleted.insert(ino);
                st.nodes.get_mut(&parent.0).unwrap().children.remove(&name);
                let path = st.nodes.get(&ino).map(|n| n.path.clone()).unwrap_or_default();
                self.mark_modified(&mut st, &path);
                reply.ok();
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let name = String::from_utf8_lossy(name.as_bytes()).to_string();
        let newname = String::from_utf8_lossy(newname.as_bytes()).to_string();
        let mut st = self.state.lock().unwrap();
        let ino = match st
            .nodes
            .get(&parent.0)
            .and_then(|p| p.children.get(&name).copied())
        {
            Some(i) => i,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        // detach old, replace any existing target
        st.nodes.get_mut(&parent.0).unwrap().children.remove(&name);
        if let Some(old) = st
            .nodes
            .get_mut(&newparent.0)
            .unwrap()
            .children
            .insert(newname.clone(), ino)
        {
            st.deleted.insert(old);
        }
        let newfull = {
            let p = &st.nodes[&newparent.0];
            if p.path.is_empty() {
                newname.clone()
            } else {
                format!("{}/{}", p.path, newname)
            }
        };
        let oldpath = {
            let n = st.nodes.get_mut(&ino).unwrap();
            n.parent = newparent.0;
            n.name = newname.clone();
            std::mem::replace(&mut n.path, newfull.clone())
        };
        self.mark_modified(&mut st, &oldpath);
        self.mark_modified(&mut st, &newfull);
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _ds: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let git_dir = arg(&args, "--repo").context("--repo <git-dir> required")?;
    let mountpoint = arg(&args, "--mount").context("--mount <dir> required")?;
    let commit = arg(&args, "--commit").unwrap_or_else(|| "HEAD".to_string());
    let fsmonitor_file = arg(&args, "--fsmonitor-file").map(PathBuf::from);
    let ready_file = arg(&args, "--ready-file");

    let git_dir_abs = std::fs::canonicalize(&git_dir)?
        .to_string_lossy()
        .to_string();

    let (nodes, next_ino) = build_tree(&git_dir_abs, &commit)?;
    let n_files = nodes.values().filter(|n| matches!(n.kind, Kind::File)).count();
    eprintln!("[vworktree] built tree: {} files", n_files);

    let catfile = CatFile::spawn(&git_dir_abs)?;
    let vfs = Vfs {
        state: Mutex::new(State {
            nodes,
            next_ino,
            upper: HashMap::new(),
            deleted: HashSet::new(),
            blob_cache: HashMap::new(),
            modified: BTreeSet::new(),
            fsm_token: now_nanos(),
            counters: Counters::default(),
        }),
        catfile: Mutex::new(catfile),
        fsmonitor_file: fsmonitor_file.clone(),
    };

    // Seed the fsmonitor file: a baseline token, zero changed paths.
    if let Some(p) = &fsmonitor_file {
        write_fsm(p, now_nanos(), &BTreeSet::new());
    }

    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::FSName("nofork".to_string()),
        MountOption::Subtype("vworktree".to_string()),
    ];

    let session = fuser::spawn_mount2(vfs, &mountpoint, &config)?;
    eprintln!("[vworktree] MOUNTED at {mountpoint}");
    if let Some(rf) = ready_file {
        let _ = std::fs::write(rf, b"ready");
    }
    // Park until the mount is torn down (driver runs `fusermount -u`).
    let _ = session.join();
    Ok(())
}
