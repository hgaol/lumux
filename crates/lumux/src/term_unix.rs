//! Terminal raw-mode + size for the client (Unix).
//!
//! Puts the controlling terminal into raw mode and the alternate screen on
//! attach, restoring on drop so a crash or detach never leaves the user's shell
//! wedged. Phase 10 adds the Windows equivalent (ENABLE_VIRTUAL_TERMINAL_*).

#![cfg(unix)]

use std::io::{self, Write};
use std::os::fd::AsRawFd;

use lumux_core::traits::PtySize;

/// VT emitted on teardown to restore the outer terminal. We reset more than just
/// the alt screen because some emulators (notably over SSH) apply DECSTBM scroll
/// regions and SGR to the primary screen buffer too:
///   ESC[0m  reset pen (no leftover color/reverse)
///   ESC[r   reset scroll region to the full screen
///   ESC[?7h re-enable autowrap (an app in a pane may have turned it off)
///   ESC[?1049l  leave the alternate screen (restores the pre-attach view)
///   ESC[?25h    show the cursor
pub const RESTORE: &[u8] = b"\x1b[0m\x1b[r\x1b[?7h\x1b[?1049l\x1b[?25h";

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
        // Alternate screen + hide cursor; clear.
        let mut out = io::stdout();
        out.write_all(b"\x1b[?1049h\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(Self { fd, original })
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
        // Restore the terminal cleanly; see RESTORE for the rationale and order.
        let mut out = io::stdout();
        let _ = out.write_all(RESTORE);
        let _ = out.flush();
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RESTORE;

    #[test]
    fn restore_resets_all_leaky_modes() {
        // The teardown must reset pen, scroll region, and autowrap, and leave the
        // alt screen with the cursor shown — in that order — so nothing bleeds
        // onto the user's shell after exit (esp. over SSH).
        assert_eq!(RESTORE, b"\x1b[0m\x1b[r\x1b[?7h\x1b[?1049l\x1b[?25h");
        // SGR/scroll-region/autowrap reset must come BEFORE leaving the alt
        // screen, since some terminals apply them to the primary buffer too.
        let s = std::str::from_utf8(RESTORE).unwrap();
        assert!(s.find("\x1b[r").unwrap() < s.find("\x1b[?1049l").unwrap());
        assert!(s.find("\x1b[0m").unwrap() < s.find("\x1b[?1049l").unwrap());
    }
}
