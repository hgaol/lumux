//! SGR mouse event parsing (DECSET 1006).
//!
//! When mouse reporting is on, terminals send events as
//! `ESC [ < b ; x ; y M`  (press/move) or `ESC [ < b ; x ; y m` (release),
//! where `b` encodes the button and modifiers, and `x`/`y` are 1-based columns
//! and rows. This module decodes that subset into [`MouseEvent`]s; the daemon
//! maps them to pane selection, scrolling, and resize.

/// A decoded mouse event in 0-based screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    /// A button was pressed (left/middle/right).
    Down(MouseButton),
    /// A button was released.
    Up(MouseButton),
    /// Motion with a button held (drag).
    Drag(MouseButton),
    /// Wheel scrolled up / down.
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Try to decode a single SGR mouse sequence at the start of `bytes`.
/// Returns the event and the number of bytes consumed, or None if `bytes` does
/// not begin with a complete `ESC [ < … M/m` sequence.
pub fn parse(bytes: &[u8]) -> Option<(MouseEvent, usize)> {
    // Must start with ESC [ <
    if bytes.len() < 4 || bytes[0] != 0x1b || bytes[1] != b'[' || bytes[2] != b'<' {
        return None;
    }
    // Find the terminating 'M' or 'm'.
    let mut end = 3;
    while end < bytes.len() && bytes[end] != b'M' && bytes[end] != b'm' {
        end += 1;
    }
    if end >= bytes.len() {
        return None; // incomplete
    }
    let is_press = bytes[end] == b'M';
    let body = std::str::from_utf8(&bytes[3..end]).ok()?;
    let mut parts = body.split(';');
    let b: u32 = parts.next()?.parse().ok()?;
    let x: u16 = parts.next()?.parse().ok()?;
    let y: u16 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    let col = x.saturating_sub(1);
    let row = y.saturating_sub(1);
    let kind = decode_button(b, is_press)?;
    Some((MouseEvent { kind, col, row }, end + 1))
}

fn decode_button(b: u32, is_press: bool) -> Option<MouseKind> {
    // Bit 6 (0x40) = wheel; low 2 bits select up/down. Bit 5 (0x20) = motion.
    let wheel = b & 0x40 != 0;
    let motion = b & 0x20 != 0;
    let low = b & 0x3;
    if wheel {
        return Some(if low == 0 {
            MouseKind::ScrollUp
        } else {
            MouseKind::ScrollDown
        });
    }
    let button = match low {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => return None, // 3 = release-in-legacy; not used in SGR
    };
    Some(if motion {
        MouseKind::Drag(button)
    } else if is_press {
        MouseKind::Down(button)
    } else {
        MouseKind::Up(button)
    })
}

/// VT sequence the client sends to enable SGR mouse reporting (button events +
/// any-motion + SGR extended coordinates).
pub const ENABLE: &str = "\x1b[?1002h\x1b[?1003h\x1b[?1006h";
/// VT sequence to disable mouse reporting on detach.
pub const DISABLE: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_left_click() {
        // ESC[<0;5;3M = left button press at col 5 row 3 (1-based).
        let (ev, n) = parse(b"\x1b[<0;5;3M").unwrap();
        assert_eq!(n, 9);
        assert_eq!(ev.kind, MouseKind::Down(MouseButton::Left));
        assert_eq!((ev.col, ev.row), (4, 2));
    }

    #[test]
    fn parses_release() {
        let (ev, _) = parse(b"\x1b[<0;5;3m").unwrap();
        assert_eq!(ev.kind, MouseKind::Up(MouseButton::Left));
    }

    #[test]
    fn parses_scroll() {
        let (up, _) = parse(b"\x1b[<64;1;1M").unwrap();
        assert_eq!(up.kind, MouseKind::ScrollUp);
        let (down, _) = parse(b"\x1b[<65;1;1M").unwrap();
        assert_eq!(down.kind, MouseKind::ScrollDown);
    }

    #[test]
    fn parses_drag() {
        // 0x20 motion bit + left button = 32.
        let (ev, _) = parse(b"\x1b[<32;10;7M").unwrap();
        assert_eq!(ev.kind, MouseKind::Drag(MouseButton::Left));
        assert_eq!((ev.col, ev.row), (9, 6));
    }

    #[test]
    fn parses_right_button() {
        let (ev, _) = parse(b"\x1b[<2;1;1M").unwrap();
        assert_eq!(ev.kind, MouseKind::Down(MouseButton::Right));
    }

    #[test]
    fn rejects_incomplete_or_non_mouse() {
        assert!(parse(b"\x1b[<0;5").is_none()); // no terminator
        assert!(parse(b"\x1b[A").is_none()); // arrow key, not mouse
        assert!(parse(b"hello").is_none());
    }

    #[test]
    fn multibyte_coordinates() {
        // col 128, row 40.
        let (ev, _) = parse(b"\x1b[<0;128;40M").unwrap();
        assert_eq!((ev.col, ev.row), (127, 39));
    }
}
