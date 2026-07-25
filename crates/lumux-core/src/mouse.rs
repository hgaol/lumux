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
    /// The raw SGR button code and press/release, kept so the event can be
    /// re-encoded faithfully (preserving modifier bits) when forwarding to an app
    /// that has mouse reporting on. See [`encode_sgr`].
    pub raw_button: u32,
    pub press: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    /// A button was pressed (left/middle/right).
    Down(MouseButton),
    /// A button was released.
    Up(MouseButton),
    /// Motion with a button held (drag).
    Drag(MouseButton),
    /// Pointer motion with NO button held (button code 3 + motion bit). Reported
    /// because any-motion tracking (DECSET 1003) is on; the daemon ignores it,
    /// but it MUST be parsed and consumed so the raw bytes don't leak through to
    /// the keymap/shell as text like `[<35;54;21M`.
    Move,
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
    Some((
        MouseEvent {
            kind,
            col,
            row,
            raw_button: b,
            press: is_press,
        },
        end + 1,
    ))
}

/// Re-encode a mouse event as an SGR sequence (`ESC[<b;x;yM/m`) with the given
/// 0-based coordinates (translated to 1-based on the wire). Used to forward an
/// event to an app that enabled mouse reporting, with pane-relative coords. The
/// original raw button code is preserved so modifiers survive the round trip.
pub fn encode_sgr(raw_button: u32, col: u16, row: u16, press: bool) -> Vec<u8> {
    let final_byte = if press { 'M' } else { 'm' };
    format!(
        "\x1b[<{};{};{}{}",
        raw_button,
        col as u32 + 1,
        row as u32 + 1,
        final_byte
    )
    .into_bytes()
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
        // 3 = "no button". With the motion bit set this is a bare pointer move
        // (DECSET 1003 any-motion tracking); consume it as Move. Without motion
        // it's a legacy release, which SGR mode reports via 'm' instead — ignore.
        _ => {
            return if motion { Some(MouseKind::Move) } else { None };
        }
    };
    Some(if motion {
        MouseKind::Drag(button)
    } else if is_press {
        MouseKind::Down(button)
    } else {
        MouseKind::Up(button)
    })
}

/// VT sequence the client sends to enable SGR mouse reporting: button-event
/// tracking (DECSET 1002) + SGR extended coordinates (1006). 1002 reports
/// presses, releases, scroll, and motion *while a button is held* (drag) — all
/// lumux acts on. We deliberately do NOT enable any-motion tracking (1003):
/// it floods a flurry of bare move events (button code 35) on terminals that
/// honor it (e.g. Windows Terminal over RDP), which lumux ignores anyway but
/// which can leak as visible `[<35;…M` text and disturb overlays. tmux likewise
/// uses 1002, not 1003.
pub const ENABLE: &str = "\x1b[?1002h\x1b[?1006h";
/// Turn on any-motion tracking (DECSET 1003) so the terminal reports pointer
/// motion with no button held. Enabled only while a menu is open: 1003 floods
/// motion events on some terminals (Windows Terminal over RDP), which is why it
/// is not part of [`ENABLE`], but hover feedback genuinely needs it and the
/// exposure is bounded to the menu's lifetime.
pub const ENABLE_HOVER: &str = "\x1b[?1003h";
/// Return to button-event tracking only (leaves 1002/1006 from [`ENABLE`] on).
pub const DISABLE_HOVER: &str = "\x1b[?1003l";

/// VT sequence to disable mouse reporting on detach. Also clears 1003 in case an
/// older build (or another program) left any-motion tracking on.
pub const DISABLE: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l";

/// Whether `bytes` is the *start* of an SGR mouse report (`ESC [ <`) that hasn't
/// yet reached its `M`/`m` terminator — i.e. a report truncated at a read
/// boundary. The daemon uses this to hold the partial bytes until the rest
/// arrives in the next frame, instead of leaking them to the app as text.
///
/// Only the unambiguous `CSI <` introducer is treated as partial. A lone `ESC`
/// or `ESC [` is deliberately NOT held: those are also the start of a real
/// Escape key or an arrow/function key, and buffering them would delay those
/// keystrokes (e.g. Escape in vim). So a split that lands exactly inside the
/// 3-byte `ESC [ <` introducer isn't reassembled — far rarer than a split in the
/// numeric body, which this does handle.
pub fn is_partial(bytes: &[u8]) -> bool {
    bytes.len() >= 3
        && bytes[0] == 0x1b
        && bytes[1] == b'['
        && bytes[2] == b'<'
        && !bytes.iter().any(|&b| b == b'M' || b == b'm')
}

/// Whether `bytes` is a bare *prefix* of the `ESC [ <` SGR introducer that
/// hasn't reached the `<` yet — i.e. exactly `ESC` or `ESC [` at a read
/// boundary. Unlike [`is_partial`], this can't be distinguished from the start
/// of a real Escape / arrow key, so callers must only hold it *mid-drag*, when
/// the next report (typically the release) is genuinely expected. Covers the
/// introducer-boundary split that [`is_partial`] deliberately skips.
pub fn is_introducer_prefix(bytes: &[u8]) -> bool {
    bytes == b"\x1b" || bytes == b"\x1b["
}

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
    fn parse_then_encode_roundtrips() {
        // An event parsed and re-encoded at the SAME coords must be byte-identical
        // — this is what makes forwarding to a mouse-aware app faithful.
        for seq in [
            &b"\x1b[<0;5;3M"[..],   // left press
            &b"\x1b[<0;5;3m"[..],   // left release
            &b"\x1b[<64;10;20M"[..], // wheel up
            &b"\x1b[<65;10;20M"[..], // wheel down
            &b"\x1b[<2;7;9M"[..],    // right press
        ] {
            let (ev, _) = parse(seq).unwrap();
            let out = encode_sgr(ev.raw_button, ev.col, ev.row, ev.press);
            assert_eq!(out, seq, "roundtrip mismatch for {:?}", std::str::from_utf8(seq));
        }
    }

    #[test]
    fn encode_translates_to_pane_relative() {
        // Forwarding subtracts the pane origin from the screen coords; encode then
        // re-adds the 1-based offset. A screen event at col 4,row 2 in a pane whose
        // origin is (3,1) is pane-relative (1,1) -> wire "2;2".
        let out = encode_sgr(0, 1, 1, true);
        assert_eq!(out, b"\x1b[<0;2;2M");
    }

    #[test]
    fn parses_drag() {
        // 0x20 motion bit + left button = 32.
        let (ev, _) = parse(b"\x1b[<32;10;7M").unwrap();
        assert_eq!(ev.kind, MouseKind::Drag(MouseButton::Left));
        assert_eq!((ev.col, ev.row), (9, 6));
    }

    #[test]
    fn parses_bare_motion_as_move() {
        // ESC[<35;54;21M — motion bit (0x20) + no button (low=3) = 35. This is
        // the any-motion report (DECSET 1003) that previously failed to parse and
        // leaked through as the literal text "[<35;54;21M", dismissing overlays.
        let (ev, n) = parse(b"\x1b[<35;54;21M").unwrap();
        assert_eq!(ev.kind, MouseKind::Move);
        assert_eq!((ev.col, ev.row), (53, 20));
        assert_eq!(n, 12);
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

    #[test]
    fn enable_does_not_turn_on_any_motion_tracking() {
        // Regression: enabling 1003 (any-motion) floods bare move events (code 35)
        // on terminals that honor it (Windows Terminal over RDP), which leak as
        // "[<35;…M" text and disturb overlays. We must enable button-event
        // tracking (1002) and SGR coords (1006) but NOT any-motion (1003).
        assert!(ENABLE.contains("1002h"), "must enable button-event tracking");
        assert!(ENABLE.contains("1006h"), "must enable SGR extended coordinates");
        assert!(
            !ENABLE.contains("1003"),
            "must NOT enable any-motion tracking (1003); got {ENABLE:?}"
        );
        // DISABLE should still clear all three, including 1003, to undo any older
        // build that left it on.
        assert!(DISABLE.contains("1002l") && DISABLE.contains("1006l"));
        assert!(DISABLE.contains("1003l"), "disable must clear 1003 too");
    }

    #[test]
    fn is_partial_detects_truncated_sgr_reports() {
        // A report split anywhere in its numeric body is partial.
        assert!(is_partial(b"\x1b[<64;10"));
        assert!(is_partial(b"\x1b[<0;5;3")); // no terminator yet
        assert!(is_partial(b"\x1b[<"));
        // A complete report is NOT partial (it has its M/m terminator).
        assert!(!is_partial(b"\x1b[<64;10;12M"));
        assert!(!is_partial(b"\x1b[<0;5;3m"));
        // A lone ESC or ESC[ is NOT treated as a partial mouse report: those are
        // also a real Escape / arrow-key prefix, and holding them would delay the
        // keystroke. Only the unambiguous CSI < introducer is buffered.
        assert!(!is_partial(b"\x1b"));
        assert!(!is_partial(b"\x1b["));
        // Ordinary text is never partial.
        assert!(!is_partial(b"hello"));
        assert!(!is_partial(b""));
    }

    #[test]
    fn is_introducer_prefix_holds_only_bare_esc_prefixes() {
        // A boundary landing inside the `ESC [ <` introducer: held mid-drag.
        assert!(is_introducer_prefix(b"\x1b"));
        assert!(is_introducer_prefix(b"\x1b["));
        // The full introducer is is_partial's job, not this.
        assert!(!is_introducer_prefix(b"\x1b[<"));
        // A real Escape / arrow / other CSI must NOT be caught (would delay the
        // keystroke by a frame).
        assert!(!is_introducer_prefix(b"\x1b[A"));
        assert!(!is_introducer_prefix(b"\x1bO"));
        assert!(!is_introducer_prefix(b""));
        assert!(!is_introducer_prefix(b"x"));
    }
}
