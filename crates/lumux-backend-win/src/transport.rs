//! Named-pipe transport + listener for the Windows backend.
//!
//! The daemon listens on `\\.\pipe\lumux-<user>` and clients connect by opening
//! that path. Both ends use **overlapped (asynchronous) I/O**: this is required
//! because lumux drives each connection with a separate reader thread and writer
//! thread, and a *synchronous* pipe handle serializes all I/O on a per-handle
//! lock — a blocking `ReadFile` would hold that lock and starve a concurrent
//! `WriteFile` on the same handle, deadlocking the duplex stream. With
//! overlapped handles each read/write carries its own OVERLAPPED + event, so
//! reads and writes proceed independently. The threads still block (via
//! `GetOverlappedResult`), so the threads+channels architecture is unchanged.
//!
//! NOTE: type-checked from Linux via the msvc target; real pipe behavior is
//! exercised on Windows.

use std::io;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

use lumux_core::proto::FrameCodec;
use lumux_core::traits::{FrameReader, FrameWriter, Listener, Transport};

/// Owns a pipe HANDLE and closes it on drop. Shared across read/write halves.
struct PipeHandle(HANDLE);

// Overlapped handles support concurrent reads and writes from separate threads.
unsafe impl Send for PipeHandle {}
unsafe impl Sync for PipeHandle {}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// An auto-managed Win32 event handle for one direction's overlapped I/O.
struct EventHandle(HANDLE);

// Each EventHandle is owned by exactly one thread (the reader or writer), and
// only ever used from that thread, so moving it across the thread boundary at
// spawn time is sound.
unsafe impl Send for EventHandle {}

impl EventHandle {
    fn new() -> io::Result<Self> {
        // Manual-reset = FALSE (auto-reset), initial state = FALSE, unnamed.
        let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(EventHandle(h))
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Build the default per-user pipe path. The user component keeps users isolated
/// and avoids name collisions.
pub fn default_pipe_path() -> String {
    let user = whoami().unwrap_or_else(|| "default".to_string());
    format!(r"\\.\pipe\lumux-{user}")
}

/// Best-effort current-user identifier for the pipe name (sanitized USERNAME).
fn whoami() -> Option<String> {
    std::env::var("USERNAME").ok().map(|u| {
        u.chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect()
    })
}

const PIPE_BUF: u32 = 64 * 1024;

/// Perform one overlapped operation (read or write) and block for completion.
/// `start` issues the ReadFile/WriteFile; returns its BOOL result and fills the
/// transferred count. Returns Ok(bytes) or Err. `Ok(0)` means EOF/closed.
unsafe fn overlapped_io(
    handle: HANDLE,
    event: HANDLE,
    start: impl FnOnce(*mut OVERLAPPED, *mut u32) -> i32,
) -> io::Result<u32> {
    let mut ov: OVERLAPPED = std::mem::zeroed();
    ov.hEvent = event;
    let mut transferred: u32 = 0;

    let ok = start(&mut ov, &mut transferred);
    if ok == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            // Immediate failure (e.g. broken pipe).
            return Err(err);
        }
        // I/O is pending: wait for the event, then collect the result.
        WaitForSingleObject(event, u32::MAX);
        let mut got: u32 = 0;
        let res = GetOverlappedResult(handle, &ov, &mut got, 1 /* bWait */);
        if res == 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(got);
    }
    // Completed synchronously.
    Ok(transferred)
}

/// A connected named pipe (overlapped), not yet split.
pub struct PipeTransport {
    handle: Arc<PipeHandle>,
}

impl PipeTransport {
    /// Connect to a daemon's pipe by path (overlapped mode).
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
                FILE_FLAG_OVERLAPPED,
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
                event: EventHandle::new()?,
                codec: FrameCodec::new(),
                buf: [0u8; 8192],
            },
            PipeWriter {
                handle: self.handle,
                event: EventHandle::new()?,
            },
        ))
    }
}

pub struct PipeReader {
    handle: Arc<PipeHandle>,
    event: EventHandle,
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
            let h = self.handle.0;
            let ev = self.event.0;
            let ptr = self.buf.as_mut_ptr();
            let len = self.buf.len() as u32;
            let n = unsafe {
                overlapped_io(h, ev, |ov, transferred| {
                    ReadFile(h, ptr, len, transferred, ov)
                })
            };
            match n {
                Ok(0) => return Ok(None), // EOF
                Ok(read) => self.codec.extend(&self.buf[..read as usize]),
                Err(_) => return Ok(None), // pipe closed / error => EOF for caller
            }
        }
    }
}

pub struct PipeWriter {
    handle: Arc<PipeHandle>,
    event: EventHandle,
}

impl FrameWriter for PipeWriter {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        let mut off = 0;
        while off < frame.len() {
            let h = self.handle.0;
            let ev = self.event.0;
            let ptr = frame[off..].as_ptr();
            let len = (frame.len() - off) as u32;
            let written = unsafe {
                overlapped_io(h, ev, |ov, transferred| {
                    WriteFile(h, ptr, len, transferred, ov)
                })
            }?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pipe write returned 0",
                ));
            }
            off += written as usize;
        }
        Ok(())
    }
}

/// Listens for client connections on a named pipe. Each `accept` creates a fresh
/// overlapped pipe instance and blocks until a client connects.
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
        // Overlapped, byte-mode, duplex pipe.
        let handle = unsafe {
            CreateNamedPipeW(
                self.wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
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
        // ConnectNamedPipe on an overlapped handle returns 0 / ERROR_IO_PENDING
        // and signals the event when a client connects.
        let event = EventHandle::new()?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.0;

        let rc = unsafe { ConnectNamedPipe(handle, &mut ov) };
        if rc == 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error().map(|c| c as u32) {
                // Client already connected between Create and Connect.
                Some(ERROR_PIPE_CONNECTED) => {}
                // Connection is being established asynchronously: wait for it.
                Some(ERROR_IO_PENDING) => {
                    let w = unsafe { WaitForSingleObject(event.0, u32::MAX) };
                    if w != WAIT_OBJECT_0 {
                        unsafe {
                            DisconnectNamedPipe(handle);
                            CloseHandle(handle);
                        }
                        return Err(io::Error::other("ConnectNamedPipe wait failed"));
                    }
                    let mut got: u32 = 0;
                    let res = unsafe { GetOverlappedResult(handle, &ov, &mut got, 1) };
                    if res == 0 {
                        let e = io::Error::last_os_error();
                        unsafe {
                            DisconnectNamedPipe(handle);
                            CloseHandle(handle);
                        }
                        return Err(e);
                    }
                }
                _ => {
                    unsafe {
                        DisconnectNamedPipe(handle);
                        CloseHandle(handle);
                    }
                    return Err(err);
                }
            }
        }
        Ok(PipeTransport {
            handle: Arc::new(PipeHandle(handle)),
        })
    }
}
