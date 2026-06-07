//! Windows backend: ConPTY PTY + named-pipe transport.
//!
//! Implements the same `lumux_core` traits as the unix backend so the daemon
//! control loop runs unchanged. The PTY half is portable-pty's ConPTY path; the
//! transport is a Win32 named pipe. ConPTY auto-translates legacy console apps
//! (cmd, PowerShell 5.x) to VT, giving shell-agnostic support for free.
//!
//! Built and type-checked from Linux via the x86_64-pc-windows-msvc target;
//! ConPTY/named-pipe behavior is validated on Windows CI (Phase 10/11).

#![cfg(windows)]

mod pty;
mod transport;

pub use pty::{WinPty, WinPtySystem, WinPtyWriter};
pub use transport::{default_pipe_path, PipeListener, PipeReader, PipeTransport, PipeWriter};

use lumux_core::traits::Clipboard;

/// Clipboard for the Windows backend. For remote/in-terminal attach the right
/// behavior is OSC-52 (populate the *client's* terminal clipboard), so this
/// delegates to the shared encoder. A native Win32 `SetClipboardData` path can
/// be added for local attach.
pub struct WinClipboard {
    last: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

impl WinClipboard {
    pub fn new() -> Self {
        Self {
            last: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl Default for WinClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for WinClipboard {
    fn set_text(&mut self, text: &str) -> std::io::Result<()> {
        *self.last.lock().unwrap() = Some(lumux_core::copymode::osc52(text));
        Ok(())
    }
}
