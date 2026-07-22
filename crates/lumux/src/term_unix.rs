//! Terminal raw-mode + size for the client (Unix).
//!
//! Puts the controlling terminal into raw mode and the alternate screen on
//! attach, restoring on drop so a crash or detach never leaves the user's shell
//! wedged. Phase 10 adds the Windows equivalent (ENABLE_VIRTUAL_TERMINAL_*).

#![cfg(unix)]

use std::io;
use std::os::fd::AsRawFd;

use lumux_core::traits::PtySize;

pub struct RawTerminal {
    fd: i32,
    original: libc::termios,
}

impl RawTerminal {
    /// Enter raw mode on stdin and switch to the alternate screen.
    pub fn enter() -> io::Result<Self> {
        let fd = io::stdin().as_raw_fd();
        let original = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = t;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            t
        };
        // Alternate screen + clear, then ask the outer terminal to report host
        // window focus changes as CSI I / CSI O.
        let mut out = io::stdout();
        crate::terminal_control::enter_with_guard(&mut out, Self { fd, original })
    }

    /// Current terminal size via TIOCGWINSZ.
    pub fn size() -> PtySize {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            let fd = io::stdout().as_raw_fd();
            if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                PtySize::new(ws.ws_col, ws.ws_row)
            } else {
                PtySize::new(80, 24)
            }
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // Restore the terminal cleanly through the platform-shared sequence.
        let mut out = io::stdout();
        let _ = crate::terminal_control::restore(&mut out);
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}
