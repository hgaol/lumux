//! Unix-domain-socket transport + listener implementing the lumux_core IPC
//! traits. The Windows backend (Phase 10) provides the named-pipe equivalent.
//!
//! A connection splits into independent read and write halves via
//! `UnixStream::try_clone`, so a blocking reader thread and a writer thread
//! share no lock.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use lumux_core::proto::FrameCodec;
use lumux_core::traits::{FrameReader, FrameWriter, Listener, Transport};

/// A connected Unix socket, not yet split.
pub struct UnixTransport {
    stream: UnixStream,
}

impl UnixTransport {
    pub fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Connect to a daemon listening at `path`.
    pub fn connect(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self::new(UnixStream::connect(path)?))
    }
}

impl Transport for UnixTransport {
    type Reader = UnixReader;
    type Writer = UnixWriter;

    fn split(self) -> std::io::Result<(Self::Reader, Self::Writer)> {
        let write_stream = self.stream.try_clone()?;
        Ok((
            UnixReader {
                stream: self.stream,
                codec: FrameCodec::new(),
                buf: [0u8; 8192],
            },
            UnixWriter {
                stream: write_stream,
            },
        ))
    }
}

pub struct UnixReader {
    stream: UnixStream,
    codec: FrameCodec,
    buf: [u8; 8192],
}

impl FrameReader for UnixReader {
    fn read_frame(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = self
                .codec
                .next_frame()
                .map_err(|e| std::io::Error::other(e.to_string()))?
            {
                return Ok(Some(frame));
            }
            let n = self.stream.read(&mut self.buf)?;
            if n == 0 {
                return Ok(None);
            }
            self.codec.extend(&self.buf[..n]);
        }
    }
}

pub struct UnixWriter {
    stream: UnixStream,
}

impl FrameWriter for UnixWriter {
    fn write_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(frame)?;
        self.stream.flush()
    }
}

/// Accepts client connections on a Unix socket.
pub struct UnixSocketListener {
    listener: UnixListener,
    path: PathBuf,
}

impl UnixSocketListener {
    /// Bind a fresh socket at `path`, removing any stale file first.
    pub fn bind(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)?;
        Ok(Self { listener, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Listener for UnixSocketListener {
    type Conn = UnixTransport;

    fn accept(&mut self) -> std::io::Result<Self::Conn> {
        let (stream, _addr) = self.listener.accept()?;
        Ok(UnixTransport::new(stream))
    }
}

impl Drop for UnixSocketListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
