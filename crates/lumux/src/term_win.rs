//! Terminal raw-mode + size for the client (Windows).
//!
//! Enables ENABLE_VIRTUAL_TERMINAL_PROCESSING on stdout and
//! ENABLE_VIRTUAL_TERMINAL_INPUT on stdin so the dumb client can both render
//! the daemon's VT and forward the user's keys as VT sequences — the same wire
//! contract as the unix client. Restores the original console modes on drop.
//!
//! Type-checked from Linux via the msvc target; exercised on Windows.

#![cfg(windows)]

use std::io::{self, Write};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode,
    CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_EXTENDED_FLAGS,
    ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
};

use lumux_core::traits::PtySize;

pub struct RawTerminal {
    in_handle: HANDLE,
    out_handle: HANDLE,
    orig_in: u32,
    orig_out: u32,
}

impl RawTerminal {
    pub fn enter() -> io::Result<Self> {
        unsafe {
            let in_handle = GetStdHandle(STD_INPUT_HANDLE);
            let out_handle = GetStdHandle(STD_OUTPUT_HANDLE);

            let mut orig_in: u32 = 0;
            let mut orig_out: u32 = 0;
            GetConsoleMode(in_handle, &mut orig_in);
            GetConsoleMode(out_handle, &mut orig_out);

            // Output: VT processing on, keep processed output.
            let new_out = orig_out
                | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                | ENABLE_PROCESSED_OUTPUT
                | DISABLE_NEWLINE_AUTO_RETURN;
            SetConsoleMode(out_handle, new_out);

            // Input: VT input on; disable line input/echo/processed so keys come
            // through raw. (ENABLE_LINE_INPUT=2, ENABLE_ECHO_INPUT=4,
            // ENABLE_PROCESSED_INPUT=1.)
            //
            // Mouse: turn ENABLE_MOUSE_INPUT on and ENABLE_QUICK_EDIT_MODE off so
            // the console hands mouse events to us (as VT/SGR sequences, since VT
            // input is enabled) instead of using them for native text selection.
            // Clearing QuickEdit only takes effect when ENABLE_EXTENDED_FLAGS is
            // also set in the same call, so we set it explicitly.
            let new_in = (orig_in & !(0x0002 | 0x0004 | 0x0001 | ENABLE_QUICK_EDIT_MODE))
                | ENABLE_VIRTUAL_TERMINAL_INPUT
                | ENABLE_MOUSE_INPUT
                | ENABLE_EXTENDED_FLAGS;
            SetConsoleMode(in_handle, new_in);

            let mut out = io::stdout();
            out.write_all(b"\x1b[?1049h\x1b[2J\x1b[H")?;
            out.flush()?;

            Ok(Self {
                in_handle,
                out_handle,
                orig_in,
                orig_out,
            })
        }
    }

    pub fn size() -> PtySize {
        unsafe {
            let out_handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(out_handle, &mut info) != 0 {
                let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
                let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
                PtySize::new(cols, rows)
            } else {
                PtySize::new(80, 24)
            }
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // Reset pen + scroll region + autowrap, leave the alt screen, then clear
        // — so no leftover SGR/geometry/layout bleeds onto the restored primary
        // screen (mosh ignores the 1049 alt screen). Same sequence as the Unix
        // backend's RESTORE constant.
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[0m\x1b[r\x1b[?7h\x1b[?1049l\x1b[H\x1b[2J\x1b[?25h");
        let _ = out.flush();
        unsafe {
            SetConsoleMode(self.in_handle, self.orig_in);
            SetConsoleMode(self.out_handle, self.orig_out);
        }
    }
}
