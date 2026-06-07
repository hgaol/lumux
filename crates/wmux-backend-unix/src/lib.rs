//! Unix backend: real Unix PTY + Unix-domain socket transport + OSC-52
//! clipboard. The development/CI substrate that lets the whole wmux daemon run
//! end-to-end on Linux.

#![cfg(unix)]

mod clipboard;
mod pty;
mod transport;

pub use clipboard::{MemoryClipboard, Osc52Clipboard};
pub use pty::{UnixPty, UnixPtySystem, UnixPtyWriter};
pub use transport::{UnixReader, UnixSocketListener, UnixTransport, UnixWriter};

/// Default daemon socket path for the current user under the runtime dir.
pub fn default_socket_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let uid = unsafe { libc::getuid() };
    base.join(format!("wmux-{uid}.sock"))
}
