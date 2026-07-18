//! Writable worktree overlay (Phase 2).
//!
//! A [`fuser::Filesystem`] that layers an in-memory **upper**
//! materialization store over a read-only [`FsProvider`] **lower**
//! (the projection), implementing the EdenFS-style materialize-on-write
//! model from `docs/design/writable-worktrees.md` §4–6:
//!
//! - Unmodified files stay **virtual** — reads fall through to the
//!   lower projection (which hydrates from the object store on demand).
//! - The first write to a file **materializes** it: the lower bytes are
//!   copied into the upper store and the write applied there; subsequent
//!   reads/writes serve the upper copy.
//! - Newly created files/dirs live entirely in the upper store; deleted
//!   entries are recorded as whiteouts.
//!
//! ## Path-keyed upper (Stage 7)
//!
//! Materialized/created entries (`edits`) and deletions (`whiteouts`)
//! are keyed by **worktree-relative path**, not inode. A baseline swap
//! ([`WritableHandle::swap_baseline`], a `checkout` of a different
//! commit) changes the inode of any path whose blob OID differs, so a
//! path-keyed upper lets local edits **survive a checkout** (EdenFS
//! semantics) — only the per-baseline inode cache is cleared on swap.
//!
//! ## Inode namespace
//!
//! Lower inodes come from the [`FsProvider`] (high bit clear for tree
//! entries). Created entries (no lower backing) are allocated in the
//! **synthetic** inode space (high bit set, [`SYNTHETIC_INODE_BIT`]),
//! keeping them disjoint from lower tree inodes. Writable worktrees
//! mount with an empty `RootOverlay` (the `.git/` lives outside the
//! mount), so there is no clash with dotgit synthetic inodes.

use crate::adapter::{errno_for, to_fuser_attr, to_fuser_kind};
use crate::upper_journal::{self, Record, UpperJournal};
use fuser::{
    Config, FileHandle, Filesystem, FopenFlags, Generation, INodeNo, MountOption, OpenFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, TimeOrNow,
};
use projgit_core::overlay::SYNTHETIC_INODE_BIT;
use projgit_core::{Attr, FileType, FsError, FsProvider};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Attr/entry cache TTL for a writable mount **with** an invalidator
/// attached. Writes push a FUSE invalidation off the handler thread, so
/// the kernel can safely cache attrs/data for unmodified files between
/// writes. A mount with no invalidator uses `Duration::ZERO` (every
/// getattr re-asks us) and stays correct.
const WRITABLE_TTL: Duration = Duration::from_secs(1);
const GEN: Generation = Generation(0);

/// A kernel-cache invalidation, applied off the FUSE handler thread.
/// Calling [`fuser::Notifier`] from *within* a handler can deadlock the
/// kernel, so write handlers enqueue these and a worker thread (holding
/// the post-mount `Notifier`) drains them.
enum Inval {
    /// Invalidate an inode's cached attributes + data.
    Inode(u64),
    /// Invalidate a `(parent, name)` directory-entry cache slot.
    Entry(u64, Vec<u8>),
}

impl Inval {
    fn apply(&self, n: &fuser::Notifier) {
        match self {
            Inval::Inode(ino) => {
                let _ = n.inval_inode(INodeNo(*ino), 0, 0);
            }
            Inval::Entry(parent, name) => {
                let _ = n.inval_entry(INodeNo(*parent), OsStr::from_bytes(name));
            }
        }
    }
}

/// Monotonic nanosecond clock for FSMonitor tokens.
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Join a worktree-relative parent path with a child name.
fn join_path(parent: &str, name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    if parent.is_empty() {
        name.into_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// Parent path of a worktree-relative path (`""` for a top-level entry).
fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Final component of a worktree-relative path.
fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Resolve a worktree-relative `path` against the lower baseline,
/// returning `(inode, attr)` if it exists there (walks from the root).
fn lower_resolve_path<F: FsProvider>(lower: &Arc<F>, path: &str) -> Option<(u64, Attr)> {
    let mut ino = projgit_core::ROOT_INODE;
    let mut attr = lower.getattr(ino).ok()?;
    if path.is_empty() {
        return Some((ino, attr));
    }
    for comp in path.split('/') {
        attr = lower.lookup(ino, comp.as_bytes()).ok()?;
        ino = attr.inode;
    }
    Some((ino, attr))
}

/// Read a lower file's full bytes (best effort; stops at first error).
fn lower_read_all<F: FsProvider>(lower: &Arc<F>, ino: u64, size: u64) -> Vec<u8> {
    let mut content = Vec::with_capacity(size as usize);
    let mut off = 0u64;
    while off < size {
        match lower.read(ino, off, 64 * 1024) {
            Ok(chunk) if !chunk.is_empty() => {
                off += chunk.len() as u64;
                content.extend_from_slice(&chunk);
            }
            _ => break,
        }
    }
    content
}

/// Apply replayed journal [`Record`]s into `up`, then **reconcile**
/// against the lower baseline: an edit whose content + mode already
/// match the baseline (i.e. it was committed, so the re-pinned HEAD now
/// carries it) is dropped; a surviving edit gets its `from_lower` set
/// from whether the path exists in the baseline; a whiteout of a path
/// absent from the baseline is dropped. Returns the compacted snapshot
/// of what survived (for [`UpperJournal::compact`]).
fn replay_into_upper<F: FsProvider>(
    up: &mut Upper,
    lower: &Arc<F>,
    records: Vec<Record>,
) -> Vec<Record> {
    for r in records {
        match r {
            Record::Set { path, kind, mode, content } => {
                up.whiteouts.remove(&path);
                up.edits.insert(
                    path,
                    Edit { kind, mode, content, from_lower: false },
                );
            }
            Record::Mkdir { path, mode } => {
                up.whiteouts.remove(&path);
                up.edits.insert(
                    path,
                    Edit {
                        kind: FileType::Directory,
                        mode,
                        content: Vec::new(),
                        from_lower: false,
                    },
                );
            }
            Record::Whiteout { path } => {
                up.edits.remove(&path);
                up.whiteouts.insert(path);
            }
        }
    }

    let snapshot = reconcile_upper(up, lower);
    up.fsm_token = now_nanos();
    snapshot
}

/// Reconcile the upper against the (current) lower baseline: an edit
/// whose content + mode already match the baseline — i.e. the baseline
/// (a re-pinned HEAD after a commit, or a checked-out commit) already
/// carries it — is **dropped**; a surviving edit gets its `from_lower`
/// from whether the path exists in the baseline; a whiteout of a path
/// absent from the baseline is dropped. Recomputes the FSMonitor
/// changed-set and returns the compacted snapshot of survivors.
///
/// Called both when replaying the journal at mount and after a
/// `swap_baseline` (checkout), so stock `git checkout`/`commit` (which
/// eagerly materialize) and `projgit checkout` (which keeps files
/// virtual) converge to the same lean upper.
fn reconcile_upper<F: FsProvider>(up: &mut Upper, lower: &Arc<F>) -> Vec<Record> {
    let mut drop_edits = Vec::new();
    for (path, edit) in up.edits.iter_mut() {
        match lower_resolve_path(lower, path) {
            Some((ino, attr)) => {
                if matches!(edit.kind, FileType::RegularFile)
                    && matches!(attr.kind, FileType::RegularFile)
                    && (attr.mode & 0o777) == edit.mode
                    && lower_read_all(lower, ino, attr.size) == edit.content
                {
                    // Identical to the baseline (committed / unchanged).
                    drop_edits.push(path.clone());
                } else {
                    edit.from_lower = true;
                }
            }
            None => edit.from_lower = false,
        }
    }
    for p in &drop_edits {
        up.edits.remove(p);
    }
    // A whiteout only means something if the path exists in the baseline.
    up.whiteouts
        .retain(|p| lower_resolve_path(lower, p).is_some());

    // Recompute the FSMonitor changed-set from what survived, so git
    // (with fsmonitor) rescans exactly the still-dirty paths.
    up.modified_paths = up
        .edits
        .keys()
        .cloned()
        .chain(up.whiteouts.iter().cloned())
        .collect();

    snapshot_records(up)
}

/// The current upper state as an ordered list of journal records (for
/// compaction after a reconcile).
fn snapshot_records(up: &Upper) -> Vec<Record> {
    let mut recs = Vec::with_capacity(up.edits.len() + up.whiteouts.len());
    for (path, edit) in &up.edits {
        if matches!(edit.kind, FileType::Directory) {
            recs.push(Record::Mkdir {
                path: path.clone(),
                mode: edit.mode,
            });
        } else {
            recs.push(Record::Set {
                path: path.clone(),
                kind: edit.kind,
                mode: edit.mode,
                content: edit.content.clone(),
            });
        }
    }
    for path in &up.whiteouts {
        recs.push(Record::Whiteout { path: path.clone() });
    }
    recs
}

/// Write the FSMonitor write-log file: `<token>\0 <path>\0 ...`. A
/// `core.fsmonitor` hook streams it to git verbatim.
fn write_fsm(path: &Path, token: u64, paths: &BTreeSet<String>) {
    let mut buf = Vec::new();
    buf.extend_from_slice(token.to_string().as_bytes());
    buf.push(0);
    for p in paths {
        buf.extend_from_slice(p.as_bytes());
        buf.push(0);
    }
    let _ = std::fs::write(path, &buf);
}

/// A materialized or created entry in the upper layer, keyed by path.
struct Edit {
    kind: FileType,
    mode: u16,
    /// File/symlink bytes (empty for directories).
    content: Vec<u8>,
    /// `true` if this is a *modified* lower file (the path also exists in
    /// the lower projection); `false` if created fresh in the mount.
    /// Drives readdir (created entries must be injected; modified-lower
    /// ones are already in the lower listing) and inode resolution.
    from_lower: bool,
}

/// The upper materialization layer.
///
/// The path-keyed `edits` / `whiteouts` are the source of truth and
/// **survive a baseline swap**; the inode maps are a per-baseline cache
/// rebuilt from `lookup` and cleared on swap.
#[derive(Default)]
struct Upper {
    /// Materialized/created entries, keyed by worktree-relative path.
    edits: HashMap<String, Edit>,
    /// Deleted/hidden worktree-relative paths.
    whiteouts: HashSet<String>,
    /// FSMonitor write-log: paths changed since mount.
    modified_paths: BTreeSet<String>,
    /// Monotonic, timestamp-shaped FSMonitor token.
    fsm_token: u64,

    // ---- per-baseline inode cache (cleared on swap) ----
    /// inode -> worktree-relative path (recorded on lookup).
    inode_paths: HashMap<u64, String>,
    /// `(parent_inode, name)` per looked-up inode, so a swap can enqueue
    /// precise `inval_entry` calls.
    inode_parent_name: HashMap<u64, (u64, Vec<u8>)>,
    /// path -> inode for *created* entries (no lower backing) so repeat
    /// lookups return a stable inode within a baseline.
    path_inode: HashMap<String, u64>,
    /// Counter for fresh synthetic inodes.
    next: u64,
}

impl Upper {
    fn alloc(&mut self) -> u64 {
        self.next += 1;
        SYNTHETIC_INODE_BIT | self.next
    }
}

/// Writable overlay filesystem: read-only `lower` projection + in-memory
/// upper materialization store. `lower` and `up` are shared (Arc) so a
/// [`WritableHandle`] can swap the baseline under a live mount.
pub struct WritableFs<F: FsProvider> {
    lower: Arc<Mutex<Arc<F>>>,
    up: Arc<Mutex<Upper>>,
    /// Off-thread kernel-cache invalidator. `None` => no invalidation
    /// and `ttl == 0` (every getattr re-asks us; always correct).
    inval: Option<Sender<Inval>>,
    /// Attr/entry cache TTL handed to the kernel.
    ttl: Duration,
    /// Optional FSMonitor write-log file (Stage 4 / R3).
    fsmonitor_file: Option<std::path::PathBuf>,
    /// Cone-mode sparse-checkout directories (Stage 5 / R2). Empty =>
    /// everything visible.
    cone: Vec<String>,
    /// Optional crash journal persisting the upper across unmount
    /// (Stage 7 / §10.3). `None` => in-memory only.
    journal: Option<Arc<UpperJournal>>,
}

impl<F: FsProvider> WritableFs<F> {
    /// Wrap a read-only projection provider as a writable overlay with
    /// no kernel-cache invalidation (TTL=0), no FSMonitor log, no cone.
    pub fn new(lower: Arc<F>) -> Self {
        Self {
            lower: Arc::new(Mutex::new(lower)),
            up: Arc::new(Mutex::new(Upper::default())),
            inval: None,
            ttl: Duration::ZERO,
            fsmonitor_file: None,
            cone: Vec::new(),
            journal: None,
        }
    }

    /// Like [`Self::new`] but with an off-thread invalidator (enabling a
    /// useful cache TTL), an optional FSMonitor write-log, an optional
    /// sparse cone, and an optional upper crash journal (replayed +
    /// reconciled against the baseline before mount).
    fn with_invalidator(
        lower: Arc<F>,
        inval: Sender<Inval>,
        fsmonitor_file: Option<PathBuf>,
        cone: Vec<String>,
        upper_dir: Option<PathBuf>,
    ) -> Self {
        let mut up = Upper::default();
        let journal = upper_dir.and_then(|dir| match UpperJournal::open(&dir) {
            Ok(j) => {
                // Restore uncommitted edits: replay the journal, reconcile
                // against the (re-pinned) baseline so committed/unchanged
                // entries drop out, then compact to that snapshot.
                let live = replay_into_upper(&mut up, &lower, upper_journal::replay(&dir));
                let _ = j.compact(&live);
                Some(Arc::new(j))
            }
            Err(_) => None,
        });
        if let Some(p) = &fsmonitor_file {
            write_fsm(p, up.fsm_token.max(now_nanos()), &up.modified_paths);
        }
        Self {
            lower: Arc::new(Mutex::new(lower)),
            up: Arc::new(Mutex::new(up)),
            inval: Some(inval),
            ttl: WRITABLE_TTL,
            fsmonitor_file,
            cone,
            journal,
        }
    }

    /// Current lower baseline (cheap Arc clone).
    fn lower(&self) -> Arc<F> {
        self.lower.lock().unwrap().clone()
    }

    /// A handle sharing this overlay's swappable state.
    fn handle(&self) -> WritableHandle<F> {
        WritableHandle {
            lower: self.lower.clone(),
            up: self.up.clone(),
            inval: self.inval.clone(),
            fsmonitor_file: self.fsmonitor_file.clone(),
            journal: self.journal.clone(),
        }
    }

    fn invalidate_inode(&self, ino: u64) {
        if let Some(tx) = &self.inval {
            let _ = tx.send(Inval::Inode(ino));
        }
    }

    fn invalidate_entry(&self, parent: u64, name: &[u8]) {
        if let Some(tx) = &self.inval {
            let _ = tx.send(Inval::Entry(parent, name.to_vec()));
        }
    }

    /// Whether a worktree-relative `path` is visible under the sparse
    /// cone. Cone-mode rules: files in the root and in directories that
    /// lead to a cone dir are shown; cone directories are shown
    /// recursively; everything else is hidden. Empty cone => all visible.
    fn cone_visible(&self, path: &str, is_dir: bool) -> bool {
        if self.cone.is_empty() {
            return true;
        }
        let recursively_in = |d: &str| {
            self.cone
                .iter()
                .any(|c| d == c || d.starts_with(&format!("{c}/")))
        };
        let leads_to_cone = |d: &str| {
            d.is_empty()
                || self
                    .cone
                    .iter()
                    .any(|c| c == d || c.starts_with(&format!("{d}/")))
        };
        if is_dir {
            recursively_in(path) || leads_to_cone(path)
        } else {
            let parent = parent_of(path);
            parent.is_empty() || recursively_in(parent) || leads_to_cone(parent)
        }
    }

    /// Record a worktree-relative path as changed and refresh the
    /// FSMonitor write-log (monotonic token + cumulative changed set).
    fn record_change(&self, up: &mut Upper, path: String) {
        up.modified_paths.insert(path);
        up.fsm_token = now_nanos().max(up.fsm_token + 1);
        if let Some(p) = &self.fsmonitor_file {
            write_fsm(p, up.fsm_token, &up.modified_paths);
        }
    }

    /// Persist a set (create/modify of a file/symlink) to the crash
    /// journal, if one is attached. No-op otherwise.
    fn journal_set(&self, path: &str, kind: FileType, mode: u16, content: &[u8]) {
        if let Some(j) = &self.journal {
            j.record_set(path, kind, mode, content);
        }
    }

    /// Persist a created directory to the crash journal, if attached.
    fn journal_mkdir(&self, path: &str, mode: u16) {
        if let Some(j) = &self.journal {
            j.record_mkdir(path, mode);
        }
    }

    /// Persist a whiteout (deletion) to the crash journal, if attached.
    fn journal_whiteout(&self, path: &str) {
        if let Some(j) = &self.journal {
            j.record_whiteout(path);
        }
    }

    /// Resolve `(parent, name)` -> child inode for a path, honoring
    /// edits/whiteouts before the lower projection, and recording the
    /// inode cache. Returns `NotFound` for whiteouted paths.
    fn resolve(&self, up: &mut Upper, parent: u64, name: &[u8], child_path: &str) -> Result<u64, FsError> {
        if up.whiteouts.contains(child_path) {
            return Err(FsError::NotFound);
        }
        let ino = match up.edits.get(child_path) {
            Some(edit) if edit.from_lower => {
                // Modified lower file: bind to the lower inode if the path
                // still exists in the current baseline, else resurrect it
                // under a stable synthetic inode.
                match self.lower().lookup(parent, name) {
                    Ok(a) => a.inode,
                    Err(_) => self.created_inode(up, child_path),
                }
            }
            Some(_) => self.created_inode(up, child_path),
            None => self.lower().lookup(parent, name)?.inode,
        };
        up.inode_paths.insert(ino, child_path.to_string());
        up.inode_parent_name
            .entry(ino)
            .or_insert_with(|| (parent, name.to_vec()));
        Ok(ino)
    }

    /// Stable synthetic inode for a created/resurrected path.
    fn created_inode(&self, up: &mut Upper, path: &str) -> u64 {
        if let Some(&ino) = up.path_inode.get(path) {
            return ino;
        }
        let ino = up.alloc();
        up.path_inode.insert(path.to_string(), ino);
        ino
    }

    /// Attributes for an inode (upper edit override, else lower).
    fn attr_for(&self, up: &Upper, ino: u64) -> Result<Attr, FsError> {
        if let Some(path) = up.inode_paths.get(&ino) {
            if let Some(edit) = up.edits.get(path) {
                let size = edit.content.len() as u64;
                let mut a = match edit.kind {
                    FileType::Directory => Attr::directory(ino),
                    FileType::Symlink => Attr::symlink(ino, size),
                    FileType::RegularFile => Attr::regular_file(ino, size, edit.mode),
                };
                a.mode = edit.mode;
                a.mtime = SystemTime::now();
                return Ok(a);
            }
        }
        self.lower().getattr(ino)
    }

    /// Ensure a modified-lower file's bytes are materialized into `edits`.
    fn materialize(&self, up: &mut Upper, ino: u64, path: &str) -> Result<(), FsError> {
        if up.edits.contains_key(path) {
            return Ok(());
        }
        let lower = self.lower();
        let attr = lower.getattr(ino)?;
        let size = attr.size;
        let mut content = Vec::with_capacity(size as usize);
        let mut off = 0u64;
        while off < size {
            let chunk = lower.read(ino, off, 64 * 1024)?;
            if chunk.is_empty() {
                break;
            }
            off += chunk.len() as u64;
            content.extend_from_slice(&chunk);
        }
        up.edits.insert(
            path.to_string(),
            Edit {
                kind: FileType::RegularFile,
                mode: attr.mode & 0o777,
                content,
                from_lower: true,
            },
        );
        Ok(())
    }
}

impl<F: FsProvider + 'static> Filesystem for WritableFs<F> {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut up = self.up.lock().unwrap();
        let name = name.as_bytes();
        let parent_path = up.inode_paths.get(&parent.0).cloned().unwrap_or_default();
        let child_path = join_path(&parent_path, name);
        match self.resolve(&mut up, parent.0, name, &child_path) {
            Ok(ino) => match self.attr_for(&up, ino) {
                Ok(a) => {
                    if !self.cone_visible(&child_path, matches!(a.kind, FileType::Directory)) {
                        reply.error(errno_for(&FsError::NotFound));
                        return;
                    }
                    reply.entry(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid()), GEN);
                }
                Err(e) => reply.error(errno_for(&e)),
            },
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let up = self.up.lock().unwrap();
        match self.attr_for(&up, ino.0) {
            Ok(a) => reply.attr(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid())),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let up = self.up.lock().unwrap();
        if let Some(path) = up.inode_paths.get(&ino.0) {
            if let Some(edit) = up.edits.get(path) {
                if matches!(edit.kind, FileType::Symlink) {
                    let b = edit.content.clone();
                    drop(up);
                    reply.data(&b);
                    return;
                }
            }
        }
        drop(up);
        match self.lower().readlink(ino.0) {
            Ok(t) => reply.data(t.as_slice()),
            Err(e) => reply.error(errno_for(&e)),
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
        let up = self.up.lock().unwrap();
        if let Some(path) = up.inode_paths.get(&ino.0) {
            if let Some(edit) = up.edits.get(path) {
                let buf = &edit.content;
                let start = (offset as usize).min(buf.len());
                let end = (start + size as usize).min(buf.len());
                let slice = buf[start..end].to_vec();
                drop(up);
                reply.data(&slice);
                return;
            }
        }
        drop(up);
        match self.lower().read(ino.0, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno_for(&e)),
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
        let up = self.up.lock().unwrap();
        let parent = ino.0;
        let dir_path = up.inode_paths.get(&parent).cloned().unwrap_or_default();
        // A created (no lower backing) directory has no lower listing.
        let is_created_dir = up
            .edits
            .get(&dir_path)
            .is_some_and(|e| !e.from_lower && matches!(e.kind, FileType::Directory));

        let mut entries: Vec<(u64, FileType, Vec<u8>)> = Vec::new();
        let mut lower_names: HashSet<Vec<u8>> = HashSet::new();
        if !is_created_dir {
            match self.lower().readdir(parent, 0) {
                Ok(lower) => {
                    for e in lower {
                        let name = e.name.to_vec();
                        let child_path = join_path(&dir_path, &name);
                        if up.whiteouts.contains(&child_path) {
                            continue;
                        }
                        lower_names.insert(name.clone());
                        entries.push((/* inode filled at add */ 0, e.kind, name));
                    }
                }
                Err(e) => {
                    reply.error(errno_for(&e));
                    return;
                }
            }
        }
        // Inject created entries (edits with no lower backing) under this dir.
        for (path, edit) in up.edits.iter() {
            if edit.from_lower || up.whiteouts.contains(path) {
                continue;
            }
            if parent_of(path) != dir_path {
                continue;
            }
            let name = basename(path).as_bytes().to_vec();
            if lower_names.contains(&name) {
                continue; // already listed by lower
            }
            entries.push((0, edit.kind, name));
        }

        // Cone filter.
        if !self.cone.is_empty() {
            entries.retain(|(_, kind, name)| {
                self.cone_visible(
                    &join_path(&dir_path, name),
                    matches!(kind, FileType::Directory),
                )
            });
        }

        let mut all: Vec<(FileType, Vec<u8>)> = vec![
            (FileType::Directory, b".".to_vec()),
            (FileType::Directory, b"..".to_vec()),
        ];
        all.extend(entries.into_iter().map(|(_, k, n)| (k, n)));
        for (i, (kind, name)) in all.iter().enumerate().skip(offset as usize) {
            // The kernel only needs a non-zero inode for `.`/`..`; for
            // real entries it re-`lookup`s by name (readdirplus is not
            // implemented), so a placeholder inode is fine here.
            let full = reply.add(
                INodeNo(if i < 2 { parent.max(1) } else { u64::MAX }),
                (i + 1) as u64,
                to_fuser_kind(*kind),
                OsStr::from_bytes(name),
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
        req: &Request,
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
        let mut up = self.up.lock().unwrap();
        let path = up.inode_paths.get(&ino.0).cloned();
        if let (Some(newsize), Some(path)) = (size, path.clone()) {
            if let Err(e) = self.materialize(&mut up, ino.0, &path) {
                reply.error(errno_for(&e));
                return;
            }
            if let Some(edit) = up.edits.get_mut(&path) {
                edit.content.resize(newsize as usize, 0);
            }
            if let Some(j) = &self.journal {
                if let Some(e) = up.edits.get(&path) {
                    j.record_set(&path, e.kind, e.mode, &e.content);
                }
            }
            self.record_change(&mut up, path);
        }
        let attr = self.attr_for(&up, ino.0);
        drop(up);
        if size.is_some() {
            self.invalidate_inode(ino.0);
        }
        match attr {
            Ok(a) => reply.attr(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid())),
            Err(e) => reply.error(errno_for(&e)),
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
        let mut up = self.up.lock().unwrap();
        let path = match up.inode_paths.get(&ino.0).cloned() {
            Some(p) => p,
            None => {
                reply.error(errno_for(&FsError::NotFound));
                return;
            }
        };
        if let Err(e) = self.materialize(&mut up, ino.0, &path) {
            reply.error(errno_for(&e));
            return;
        }
        {
            let edit = up.edits.get_mut(&path).unwrap();
            let start = offset as usize;
            let end = start + data.len();
            if edit.content.len() < end {
                edit.content.resize(end, 0);
            }
            edit.content[start..end].copy_from_slice(data);
        }
        if let Some(j) = &self.journal {
            let e = up.edits.get(&path).unwrap();
            j.record_set(&path, e.kind, e.mode, &e.content);
        }
        self.record_change(&mut up, path);
        drop(up);
        self.invalidate_inode(ino.0);
        reply.written(data.len() as u32);
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name = name.as_bytes();
        let mut up = self.up.lock().unwrap();
        let parent_path = up.inode_paths.get(&parent.0).cloned().unwrap_or_default();
        let path = join_path(&parent_path, name);
        up.whiteouts.remove(&path);
        up.edits.insert(
            path.clone(),
            Edit {
                kind: FileType::RegularFile,
                mode: (mode as u16) & 0o777,
                content: Vec::new(),
                from_lower: false,
            },
        );
        let ino = self.created_inode(&mut up, &path);
        up.inode_paths.insert(ino, path.clone());
        up.inode_parent_name.insert(ino, (parent.0, name.to_vec()));
        self.journal_set(&path, FileType::RegularFile, (mode as u16) & 0o777, &[]);
        self.record_change(&mut up, path);
        let attr = self.attr_for(&up, ino);
        drop(up);
        self.invalidate_entry(parent.0, name);
        match attr {
            Ok(a) => reply.created(
                &self.ttl,
                &to_fuser_attr(&a, req.uid(), req.gid()),
                GEN,
                FileHandle(0),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = name.as_bytes();
        let mut up = self.up.lock().unwrap();
        let parent_path = up.inode_paths.get(&parent.0).cloned().unwrap_or_default();
        let path = join_path(&parent_path, name);
        up.whiteouts.remove(&path);
        up.edits.insert(
            path.clone(),
            Edit {
                kind: FileType::Directory,
                mode: (mode as u16) & 0o777,
                content: Vec::new(),
                from_lower: false,
            },
        );
        let ino = self.created_inode(&mut up, &path);
        up.inode_paths.insert(ino, path.clone());
        up.inode_parent_name.insert(ino, (parent.0, name.to_vec()));
        self.journal_mkdir(&path, (mode as u16) & 0o777);
        let attr = self.attr_for(&up, ino);
        drop(up);
        self.invalidate_entry(parent.0, name);
        match attr {
            Ok(a) => reply.entry(&self.ttl, &to_fuser_attr(&a, req.uid(), req.gid()), GEN),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = name.as_bytes();
        let mut up = self.up.lock().unwrap();
        let parent_path = up.inode_paths.get(&parent.0).cloned().unwrap_or_default();
        let path = join_path(&parent_path, name);
        up.edits.remove(&path);
        up.path_inode.remove(&path);
        up.whiteouts.insert(path.clone());
        self.journal_whiteout(&path);
        self.record_change(&mut up, path);
        drop(up);
        self.invalidate_entry(parent.0, name);
        reply.ok();
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.unlink(req, parent, name, reply);
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
        let name = name.as_bytes();
        let newname = newname.as_bytes();
        let mut up = self.up.lock().unwrap();
        let src_parent_path = up.inode_paths.get(&parent.0).cloned().unwrap_or_default();
        let dst_parent_path = up.inode_paths.get(&newparent.0).cloned().unwrap_or_default();
        let src_path = join_path(&src_parent_path, name);
        let dst_path = join_path(&dst_parent_path, newname);
        if up.whiteouts.contains(&src_path) {
            reply.error(errno_for(&FsError::NotFound));
            return;
        }

        // Ensure the source bytes are captured at the destination path.
        if let Some(edit) = up.edits.remove(&src_path) {
            up.path_inode.remove(&src_path);
            up.edits.insert(
                dst_path.clone(),
                Edit {
                    from_lower: false,
                    ..edit
                },
            );
        } else {
            // Plain lower file: materialize its bytes under the dst path.
            if let Ok(a) = self.lower().lookup(parent.0, name) {
                if let Ok(attr) = self.lower().getattr(a.inode) {
                    let mut content = Vec::new();
                    let mut off = 0u64;
                    let lower = self.lower();
                    while off < attr.size {
                        match lower.read(a.inode, off, 64 * 1024) {
                            Ok(chunk) if !chunk.is_empty() => {
                                off += chunk.len() as u64;
                                content.extend_from_slice(&chunk);
                            }
                            _ => break,
                        }
                    }
                    up.edits.insert(
                        dst_path.clone(),
                        Edit {
                            kind: attr.kind,
                            mode: attr.mode & 0o777,
                            content,
                            from_lower: false,
                        },
                    );
                }
            }
        }
        up.whiteouts.remove(&dst_path);
        up.whiteouts.insert(src_path.clone());
        if let Some(j) = &self.journal {
            j.record_whiteout(&src_path);
            if let Some(e) = up.edits.get(&dst_path) {
                if matches!(e.kind, FileType::Directory) {
                    j.record_mkdir(&dst_path, e.mode);
                } else {
                    j.record_set(&dst_path, e.kind, e.mode, &e.content);
                }
            }
        }
        self.record_change(&mut up, src_path);
        self.record_change(&mut up, dst_path);
        drop(up);
        self.invalidate_entry(parent.0, name);
        self.invalidate_entry(newparent.0, newname);
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

/// A handle to a live writable mount that shares the overlay's swappable
/// state, so a caller can swap the LOWER baseline (a `checkout` of a
/// different commit) under the mount. Obtain it from
/// [`mount_writable_background_with_handle`].
pub struct WritableHandle<F: FsProvider> {
    lower: Arc<Mutex<Arc<F>>>,
    up: Arc<Mutex<Upper>>,
    inval: Option<Sender<Inval>>,
    fsmonitor_file: Option<std::path::PathBuf>,
    journal: Option<Arc<UpperJournal>>,
}

impl<F: FsProvider> WritableHandle<F> {
    /// Swap the LOWER baseline under the live mount (a `checkout` of a
    /// different commit). Local edits and whiteouts are **path-keyed**, so
    /// they survive the swap (EdenFS semantics — your local change shadows
    /// the checked-out version). Only the per-baseline inode cache is
    /// cleared; the kernel's attr + entry caches for every previously
    /// looked-up inode are invalidated (off-thread) so unmodified files
    /// re-virtualize to `new_lower`.
    ///
    /// The upper is then **reconciled** against the new baseline: any
    /// edit the new commit already carries is dropped, so a stock `git
    /// checkout`/`commit` (which eagerly materializes) and a `projgit
    /// checkout` (which keeps files virtual) converge to the same lean
    /// upper, and the crash journal is compacted to match.
    pub fn swap_baseline(&self, new_lower: Arc<F>) {
        let mut up = self.up.lock().unwrap();
        // Point the overlay at the new baseline.
        *self.lower.lock().unwrap() = new_lower.clone();
        // Re-lookup everything: the kernel must drop dentries (a changed
        // file's inode differs in the new baseline) and re-read data.
        if let Some(tx) = &self.inval {
            for (ino, (parent, name)) in up.inode_parent_name.iter() {
                let _ = tx.send(Inval::Entry(*parent, name.clone()));
                let _ = tx.send(Inval::Inode(*ino));
            }
        }
        // Clear the per-baseline inode cache; `edits` / `whiteouts`
        // survive (path-keyed).
        up.inode_paths.clear();
        up.inode_parent_name.clear();
        up.path_inode.clear();
        // Drop edits the new baseline already carries; recompute the
        // dirty set. Returns the compacted survivor snapshot.
        let live = reconcile_upper(&mut up, &new_lower);
        // Advance the FSMonitor token so a watcher sees state changed.
        up.fsm_token = now_nanos().max(up.fsm_token + 1);
        if let Some(p) = &self.fsmonitor_file {
            write_fsm(p, up.fsm_token, &up.modified_paths);
        }
        drop(up);
        // Keep the on-disk journal in step so it doesn't accumulate
        // now-committed edits across many checkouts in a long session.
        if let Some(j) = &self.journal {
            let _ = j.compact(&live);
        }
    }
}

/// Mount a [`WritableFs`] over `lower` at `mountpoint`, returning a
/// background session. Unlike [`crate::mount_background`], the mount is
/// **read-write** (no `MountOption::RO`). The default read-only mount
/// path is unaffected.
pub fn mount_writable_background<F: FsProvider + 'static>(
    lower: Arc<F>,
    mountpoint: impl AsRef<Path>,
    config: &crate::MountConfig,
) -> std::io::Result<fuser::BackgroundSession> {
    Ok(mount_writable_background_with_handle(lower, mountpoint, config)?.0)
}

/// Like [`mount_writable_background`] but also returns a
/// [`WritableHandle`] for swapping the LOWER baseline under the live
/// mount (checkout-under-live-mount).
pub fn mount_writable_background_with_handle<F: FsProvider + 'static>(
    lower: Arc<F>,
    mountpoint: impl AsRef<Path>,
    config: &crate::MountConfig,
) -> std::io::Result<(fuser::BackgroundSession, WritableHandle<F>)> {
    let (tx, rx) = std::sync::mpsc::channel::<Inval>();
    let fs = WritableFs::with_invalidator(
        lower,
        tx,
        config.fsmonitor_file.clone(),
        config.sparse_cone.clone(),
        config.upper_dir.clone(),
    );
    let handle = fs.handle();
    let mut fc = Config::default();
    fc.mount_options = vec![
        MountOption::FSName(config.name.clone()),
        MountOption::Subtype(config.subtype.clone()),
        MountOption::NoSuid,
        MountOption::NoDev,
    ];
    fc.acl = config.acl;
    fc.n_threads = config.n_threads;
    let session = fuser::spawn_mount2(fs, mountpoint, &fc)?;
    // Drain invalidations on a dedicated thread holding the post-mount
    // Notifier — calling it from within a handler can deadlock the
    // kernel. The thread exits when the WritableFs (and its Sender) is
    // dropped on unmount.
    let notifier = session.notifier();
    std::thread::Builder::new()
        .name("projgit-fuse-inval".to_string())
        .spawn(move || {
            for op in rx {
                op.apply(&notifier);
            }
        })
        .ok();
    Ok((session, handle))
}
