//! Stage 0 spike for projgitd: prove that a process which did NOT open
//! /dev/fuse and did NOT call mount(2) can nonetheless serve the FUSE
//! protocol on the resulting fd (received via SCM_RIGHTS).
//!
//! This is the load-bearing assumption behind Stage 4 of the projgitd
//! plan (T4 last-mile, per-namespace mount established by Harbor, served
//! by the sidecar). If this spike works, fuser's `Session::from_fd` is
//! the production-ready primitive and Stage 4 is small. If it doesn't,
//! we redesign Stage 4 before committing to it.
//!
//! Usage:
//!
//! ```sh
//! # terminal 1: start the server, waits for fd
//! cargo run --release -- serve /tmp/spike.sock
//!
//! # terminal 2: opens /dev/fuse, mounts, passes fd, exits
//! mkdir -p /tmp/spike-mp
//! sudo cargo run --release -- open /tmp/spike-mp /tmp/spike.sock
//!
//! # terminal 3: verify
//! ls -la /tmp/spike-mp        # → "hello" with size 6
//! cat /tmp/spike-mp/hello     # → "world\n"
//!
//! # cleanup
//! # kill the serve process; then:
//! sudo fusermount3 -u /tmp/spike-mp
//! ```
//!
//! Throwaway. NOT shipped. NOT a workspace member.

// SCM_RIGHTS recv needs FromRawFd, hence no #![forbid(unsafe_code)] here.

use std::ffi::OsStr;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, INodeNo, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEntry, Request, Session, SessionACL,
};

const HELLO_NAME: &[u8] = b"hello";
const HELLO_CONTENT: &[u8] = b"world\n";
const HELLO_INODE: u64 = 2;
const ROOT_INODE: u64 = 1;
const TTL: Duration = Duration::from_secs(60);

fn main() -> Result<()> {
    let mut args = std::env::args();
    let _bin = args.next();
    let sub = args.next();
    match sub.as_deref() {
        Some("open") => {
            let mp = args.next().context("usage: spike open <mountpoint> <socket>")?;
            let sock = args
                .next()
                .context("usage: spike open <mountpoint> <socket>")?;
            cmd_open(&mp, &sock)
        }
        Some("serve") => {
            let sock = args.next().context("usage: spike serve <socket>")?;
            cmd_serve(&sock)
        }
        _ => {
            eprintln!("usage:");
            eprintln!("  spike open <mountpoint> <socket>");
            eprintln!("  spike serve <socket>");
            std::process::exit(2);
        }
    }
}

// -----------------------------------------------------------------------------
// `spike open` — opens /dev/fuse, calls mount(2), passes fd, exits.
// -----------------------------------------------------------------------------

fn cmd_open(mountpoint: &str, socket: &str) -> Result<()> {
    let mp_path = Path::new(mountpoint).canonicalize().with_context(|| {
        format!("mountpoint {mountpoint} must exist (mkdir -p it first)")
    })?;

    eprintln!("[open] opening /dev/fuse");
    let fuse_fd: OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .context("open /dev/fuse (needs root or fusermount setuid)")?
        .into();
    let fuse_raw = fuse_fd.as_raw_fd();

    let uid = nix::unistd::geteuid().as_raw();
    let gid = nix::unistd::getegid().as_raw();

    // rootmode=040000 = S_IFDIR (directory). The kernel needs to know the
    // root inode's type at mount time.
    let opts = format!(
        "fd={fuse_raw},rootmode=40000,user_id={uid},group_id={gid},allow_other"
    );
    eprintln!("[open] mount(spike, {}, fuse, MS_NOSUID|MS_NODEV, \"{}\")", mp_path.display(), opts);

    nix::mount::mount(
        Some("spike"),
        &mp_path,
        Some("fuse"),
        nix::mount::MsFlags::MS_NOSUID | nix::mount::MsFlags::MS_NODEV,
        Some(opts.as_str()),
    )
    .context("mount(2): typically EPERM if not run as root")?;

    eprintln!("[open] mounted; fd={fuse_raw} uid={uid} gid={gid}");

    eprintln!("[open] connecting to {socket}");
    let stream = UnixStream::connect(socket).with_context(|| {
        format!("connect to {socket} (is `serve` running and listening?)")
    })?;

    eprintln!("[open] sending fd via SCM_RIGHTS");
    send_fd(&stream, fuse_fd.as_fd()).context("sendmsg SCM_RIGHTS")?;

    eprintln!("[open] sent. closing local fd, exiting. mount stays in namespace.");
    // OwnedFd drops here → process A's copy of the fd is closed.
    // The serve process now holds the only userspace fd; the kernel mount
    // table holds the mount.
    Ok(())
}

// -----------------------------------------------------------------------------
// `spike serve` — receives fd, wraps with Session::from_fd, runs protocol loop.
// -----------------------------------------------------------------------------

fn cmd_serve(socket: &str) -> Result<()> {
    let _ = std::fs::remove_file(socket); // idempotent
    let listener = UnixListener::bind(socket).with_context(|| format!("bind {socket}"))?;
    eprintln!("[serve] listening on {socket}; waiting for `open` to connect");

    let (stream, _) = listener.accept().context("accept")?;
    eprintln!("[serve] accepted; awaiting fd");

    let fd = recv_fd(&stream).context("recvmsg SCM_RIGHTS")?;
    eprintln!("[serve] received fd={}; wrapping with Session::from_fd", fd.as_raw_fd());

    let session = Session::from_fd(HelloFs, fd, SessionACL::All, Config::default())
        .context("Session::from_fd (handshake)")?;
    eprintln!("[serve] handshake ok; entering protocol loop");

    // Session::run is pub(crate); the public path to drive the loop is
    // spawn() (background thread) + join() (block until unmount).
    let bg = session.spawn().context("session.spawn")?;
    eprintln!("[serve] background session running; waiting on join (umount or kill to exit)");
    bg.join().context("BackgroundSession::join")?;
    eprintln!("[serve] joined; serve exiting");
    Ok(())
}

// -----------------------------------------------------------------------------
// HelloFs — minimal in-memory filesystem with one file "hello".
// -----------------------------------------------------------------------------

struct HelloFs;

fn dir_attr(ino: u64) -> FileAttr {
    let now = SystemTime::UNIX_EPOCH;
    FileAttr {
        ino: INodeNo(ino),
        size: 0,
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

fn file_attr(ino: u64, size: u64) -> FileAttr {
    let now = SystemTime::UNIX_EPOCH;
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: (size + 511) / 512,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

impl Filesystem for HelloFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent.0 == ROOT_INODE && name.as_bytes() == HELLO_NAME {
            reply.entry(
                &TTL,
                &file_attr(HELLO_INODE, HELLO_CONTENT.len() as u64),
                fuser::Generation(0),
            );
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match ino.0 {
            ROOT_INODE => reply.attr(&TTL, &dir_attr(ROOT_INODE)),
            HELLO_INODE => reply.attr(&TTL, &file_attr(HELLO_INODE, HELLO_CONTENT.len() as u64)),
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        if ino.0 != HELLO_INODE {
            reply.error(Errno::ENOENT);
            return;
        }
        let start = offset as usize;
        if start >= HELLO_CONTENT.len() {
            reply.data(&[]);
            return;
        }
        let end = (start + size as usize).min(HELLO_CONTENT.len());
        reply.data(&HELLO_CONTENT[start..end]);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino.0 != ROOT_INODE {
            reply.error(Errno::ENOTDIR);
            return;
        }
        // entries: (inode, kind, name)
        let entries = [
            (ROOT_INODE, FileType::Directory, "."),
            (ROOT_INODE, FileType::Directory, ".."),
            (HELLO_INODE, FileType::RegularFile, "hello"),
        ];
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            let next = (i + 1) as u64;
            if reply.add(INodeNo(*ino), next, *kind, OsStr::new(name)) {
                break;
            }
        }
        reply.ok();
    }
}

// -----------------------------------------------------------------------------
// SCM_RIGHTS helpers
// -----------------------------------------------------------------------------

fn send_fd(stream: &UnixStream, fd: std::os::fd::BorrowedFd<'_>) -> Result<()> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    let raw_fds = [fd.as_raw_fd()];
    let cmsg = [ControlMessage::ScmRights(&raw_fds)];
    // Some payload is required so the receiver has something to recv.
    let payload = b"FD";
    let iov = [IoSlice::new(payload)];
    sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)?;
    Ok(())
}

fn recv_fd(stream: &UnixStream) -> Result<OwnedFd> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
    let mut buf = [0u8; 8];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsgspace = nix::cmsg_space!([std::os::fd::RawFd; 1]);
    let msg = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsgspace),
        MsgFlags::empty(),
    )?;
    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            if let Some(&raw) = fds.first() {
                // SAFETY: raw came from SCM_RIGHTS on this process — kernel
                // gives us a fresh, owned fd. Wrap with FromRawFd to take ownership.
                let owned = unsafe { OwnedFd::from_raw_fd(raw) };
                return Ok(owned);
            }
        }
    }
    bail!("no SCM_RIGHTS fd in received message");
}
