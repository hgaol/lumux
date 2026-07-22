//! Shared VT lifecycle for the interactive client's outer terminal.

use std::io::{self, Write};

const ENTER_SCREEN: &[u8] = b"\x1b[?1049h\x1b[2J\x1b[H";
const RESTORE_PREFIX: &[u8] = b"\x1b[0m\x1b[r\x1b[?7h";
const RESTORE_SUFFIX: &[u8] = b"\x1b[?1049l\x1b[H\x1b[2J\x1b[?25h";

/// Enter the alternate screen and enable outer-window focus reports.
pub(crate) fn enter(out: &mut impl Write) -> io::Result<()> {
    out.write_all(ENTER_SCREEN)?;
    out.write_all(lumux_core::terminal_input::FOCUS_ENABLE)?;
    out.flush()
}

/// Enter the screen while an already-active native terminal-mode guard is in
/// scope. If any VT write fails, `guard` is dropped before the error escapes,
/// restoring termios/console modes instead of leaving the caller wedged in raw
/// mode. On success ownership of the guard returns to the attach lifecycle.
pub(crate) fn enter_with_guard<T>(out: &mut impl Write, guard: T) -> io::Result<T> {
    enter(out)?;
    Ok(guard)
}

/// Restore every terminal mode owned by the attach client.
///
/// Reset the pen, scroll region, and autowrap before disabling focus reports
/// and leaving the alternate screen. Clear after leaving it so mosh (which may
/// ignore the alternate-screen switch) cannot leave the multiplexer layout on
/// the user's primary buffer.
pub(crate) fn restore(out: &mut impl Write) -> io::Result<()> {
    out.write_all(RESTORE_PREFIX)?;
    out.write_all(lumux_core::terminal_input::FOCUS_DISABLE)?;
    out.write_all(RESTORE_SUFFIX)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed terminal"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct DropProbe(std::rc::Rc<std::cell::Cell<bool>>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[test]
    fn terminal_lifecycle_is_symmetric_and_restores_in_safe_order() {
        let mut enter_bytes = Vec::new();
        enter(&mut enter_bytes).unwrap();
        assert_eq!(enter_bytes, b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?1004h");

        let mut restore_bytes = Vec::new();
        restore(&mut restore_bytes).unwrap();
        assert_eq!(
            restore_bytes,
            b"\x1b[0m\x1b[r\x1b[?7h\x1b[?1004l\x1b[?1049l\x1b[H\x1b[2J\x1b[?25h"
        );
        let sequence = std::str::from_utf8(&restore_bytes).unwrap();
        assert!(sequence.find("\x1b[r").unwrap() < sequence.find("\x1b[?1049l").unwrap());
        assert!(sequence.find("\x1b[0m").unwrap() < sequence.find("\x1b[?1049l").unwrap());
        assert!(sequence.find("\x1b[?1004l").unwrap() < sequence.find("\x1b[?1049l").unwrap());
        assert!(sequence.find("\x1b[2J").unwrap() > sequence.find("\x1b[?1049l").unwrap());
    }

    #[test]
    fn failed_screen_entry_drops_the_native_mode_guard() {
        let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
        let result = enter_with_guard(&mut FailingWriter, DropProbe(dropped.clone()));

        assert!(result.is_err());
        assert!(
            dropped.get(),
            "a failed VT write must roll back native raw/console modes"
        );
    }
}
