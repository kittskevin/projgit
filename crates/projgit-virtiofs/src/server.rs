//! Length-prefixed FUSE frame transport and Unix domain socket server for `projgit-virtiofs`.
//!
//! Each message on a framed stream is preceded by its 4-byte little-endian payload
//! length (`u32`), followed immediately by the raw FUSE request or response bytes
//! (whose own [`crate::wire::FuseInHeader`] / [`crate::wire::FuseOutHeader`] starts
//! with the matching `len` field).

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(unix)]
use std::thread::{self, JoinHandle};
#[cfg(unix)]
use std::time::Duration;

use projgit_core::fs_provider::FsProvider;

use crate::dax::DaxMapper;
use crate::ProjgitVirtioFs;

/// Maximum permitted FUSE frame size (16 MiB) to guard against malformed frame headers.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Write a single length-prefixed FUSE frame to `writer`.
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("FUSE frame length {} exceeds MAX_FRAME_LEN", payload.len()),
        ));
    }
    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Read a single length-prefixed FUSE frame from `reader`.
///
/// Returns `Ok(None)` on clean EOF before the 4-byte frame length header.
pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    let mut read_hdr = 0usize;
    while read_hdr < 4 {
        match reader.read(&mut len_buf[read_hdr..]) {
            Ok(0) if read_hdr == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated 4-byte FUSE frame header",
                ))
            }
            Ok(n) => read_hdr += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid FUSE frame length {len}"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Send one FUSE request frame over `stream` and read back the corresponding response frame.
pub fn exchange_frame<S: Read + Write>(stream: &mut S, request: &[u8]) -> io::Result<Vec<u8>> {
    write_frame(stream, request)?;
    read_frame(stream)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed stream before sending FUSE response frame",
        )
    })
}

/// Serve length-prefixed FUSE frames from `reader` to `writer` until EOF using `fs`.
///
/// Returns the total number of requests processed.
pub fn serve_stream<R, W, F, M>(
    fs: &ProjgitVirtioFs<F, M>,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<u64>
where
    R: Read,
    W: Write,
    F: FsProvider,
    M: DaxMapper,
{
    let mut count = 0u64;
    while let Some(req) = read_frame(reader)? {
        let resp = fs.dispatch(&req);
        write_frame(writer, &resp)?;
        count += 1;
    }
    Ok(count)
}

/// Handle to a background Unix domain socket server spawned by [`spawn_unix_socket_server`].
#[cfg(unix)]
pub struct UnixSocketServerHandle {
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<io::Result<()>>>,
}

#[cfg(unix)]
impl UnixSocketServerHandle {
    /// Filesystem path of the listening Unix domain socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Signal the server loop to stop accepting new connections and wait for the background thread.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        // Poke the listener so a blocking/polling accept wakes up promptly.
        let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        if let Some(handle) = self.join.take() {
            handle.join().map_err(|_| {
                io::Error::other("projgit-virtiofs unix socket server thread panicked")
            })??;
        }
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for UnixSocketServerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Serve incoming connections on a Unix domain socket at `socket_path` until `shutdown` is set.
#[cfg(unix)]
pub fn serve_unix_socket<F, M>(
    socket_path: &Path,
    fs: Arc<ProjgitVirtioFs<F, M>>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()>
where
    F: FsProvider + 'static,
    M: DaxMapper + 'static,
{
    use std::os::unix::net::UnixListener;

    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                stream.set_nonblocking(false)?;
                let mut reader = stream.try_clone()?;
                let _ = serve_stream(&fs, &mut reader, &mut stream);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// Bind a Unix domain socket at `socket_path` and spawn a background thread serving `fs`.
#[cfg(unix)]
pub fn spawn_unix_socket_server<F, M>(
    socket_path: impl Into<PathBuf>,
    fs: Arc<ProjgitVirtioFs<F, M>>,
) -> io::Result<UnixSocketServerHandle>
where
    F: FsProvider + 'static,
    M: DaxMapper + 'static,
{
    use std::os::unix::net::UnixListener;

    let socket_path = socket_path.into();
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    // Bind synchronously so the socket is guaranteed ready when this function returns.
    let listener = UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_bg = Arc::clone(&shutdown);
    let path_bg = socket_path.clone();

    let join = thread::spawn(move || {
        while !shutdown_bg.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    if shutdown_bg.load(Ordering::SeqCst) {
                        break;
                    }
                    stream.set_nonblocking(false)?;
                    let mut reader = stream.try_clone()?;
                    let _ = serve_stream(&fs, &mut reader, &mut stream);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let _ = std::fs::remove_file(&path_bg);
        Ok(())
    });

    Ok(UnixSocketServerHandle {
        socket_path,
        shutdown,
        join: Some(join),
    })
}
