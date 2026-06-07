//! Named-pipe transport + listener for the Windows backend.
//!
//! Mirrors the unix Unix-socket transport: the daemon listens on
//! `\\.\pipe\wmux-<user-sid>` and clients connect by opening that path. A
//! duplex byte-mode pipe handle is safe to read on one thread while writing on
//! another, so the reader and writer halves share the same handle via an `Arc`.
//!
//! NOTE: type-checked from Linux via the msvc target; real pipe behavior is
//! exercised on Windows CI.

use std::io;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};

use wmux_core::proto::FrameCodec;
use wmux_core::traits::{FrameReader, FrameWriter, Listener, Transport};

/// Owns a pipe HANDLE and closes it on drop. Shared across read/write halves.
struct PipeHandle(HANDLE);

// A duplex byte-mode pipe handle may be used for blocking reads and writes from
// separate threads concurrently; Windows serializes per-direction.
unsafe impl Send for PipeHandle {}
unsafe impl Sync for PipeHandle {}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Build the default per-user pipe path. The SID component keeps users isolated
/// and avoids name collisions.
pub fn default_pipe_path() -> String {
    let user = whoami_sid().unwrap_or_else(|| "default".to_string());
    format!(r"\\.\pipe\wmux-{user}")
}

/// Best-effort current-user identifier for the pipe name. Uses USERNAME; a true
/// SID lookup can be added, but USERNAME is unique per interactive session and
/// adequate for name-scoping.
fn whoami_sid() -> Option<String> {
    std::env::var("USERNAME").ok().map(|u| {
        // Sanitize for a pipe name.
        u.chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect()
    })
}

const PIPE_BUF: u32 = 64 * 1024;

/// A connected named pipe, not yet split.
pub struct PipeTransport {
    handle: Arc<PipeHandle>,
}

impl PipeTransport {
    /// Connect to a daemon's pipe by path.
    pub fn connect(path: &str) -> io::Result<Self> {
        let wide = to_wide(path);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                // GENERIC_READ | GENERIC_WRITE
                0x8000_0000 | 0x4000_0000,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: Arc::new(PipeHandle(handle)),
        })
    }
}

impl Transport for PipeTransport {
    type Reader = PipeReader;
    type Writer = PipeWriter;

    fn split(self) -> io::Result<(Self::Reader, Self::Writer)> {
        Ok((
            PipeReader {
                handle: self.handle.clone(),
                codec: FrameCodec::new(),
                buf: [0u8; 8192],
            },
            PipeWriter {
                handle: self.handle,
            },
        ))
    }
}

pub struct PipeReader {
    handle: Arc<PipeHandle>,
    codec: FrameCodec,
    buf: [u8; 8192],
}

impl FrameReader for PipeReader {
    fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = self
                .codec
                .next_frame()
                .map_err(|e| io::Error::other(e.to_string()))?
            {
                return Ok(Some(frame));
            }
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.handle.0,
                    self.buf.as_mut_ptr(),
                    self.buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                return Ok(None); // pipe closed / EOF
            }
            self.codec.extend(&self.buf[..read as usize]);
        }
    }
}

pub struct PipeWriter {
    handle: Arc<PipeHandle>,
}

impl FrameWriter for PipeWriter {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        let mut off = 0;
        while off < frame.len() {
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.handle.0,
                    frame[off..].as_ptr(),
                    (frame.len() - off) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            off += written as usize;
        }
        Ok(())
    }
}

/// Listens for client connections on a named pipe. Each `accept` creates a
/// fresh pipe instance and blocks until a client connects.
pub struct PipeListener {
    path: String,
    wide: Vec<u16>,
}

impl PipeListener {
    pub fn bind(path: impl Into<String>) -> io::Result<Self> {
        let path = path.into();
        let wide = to_wide(&path);
        Ok(Self { path, wide })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn create_instance(&self) -> io::Result<HANDLE> {
        // Blocking, byte-mode, duplex pipe (no FILE_FLAG_OVERLAPPED).
        let handle = unsafe {
            CreateNamedPipeW(
                self.wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                255, // max instances
                PIPE_BUF,
                PIPE_BUF,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }
}

impl Listener for PipeListener {
    type Conn = PipeTransport;

    fn accept(&mut self) -> io::Result<Self::Conn> {
        let handle = self.create_instance()?;
        let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        // ConnectNamedPipe returns 0 with ERROR_PIPE_CONNECTED (535) if a client
        // connected between Create and Connect — that's still success.
        if connected == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(535) {
                unsafe {
                    DisconnectNamedPipe(handle);
                    CloseHandle(handle);
                }
                return Err(err);
            }
        }
        Ok(PipeTransport {
            handle: Arc::new(PipeHandle(handle)),
        })
    }
}
