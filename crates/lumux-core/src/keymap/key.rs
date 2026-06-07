//! Keys and key encoding.
//!
//! The client forwards the user's terminal input as raw bytes. The server
//! decodes just enough to (a) recognize the prefix key and (b) match the single
//! key that follows it against the binding table. Everything else passes
//! through to the focused pane unchanged, so we never lossily re-encode normal
//! typing.

/// A decoded key press: a base key plus modifier flags. Only the subset lumux
/// needs to bind against is modeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    PageUp,
    PageDown,
    Home,
    End,
    Space,
}

impl Key {
    pub fn ctrl(c: char) -> Self {
        Key {
            code: KeyCode::Char(c.to_ascii_lowercase()),
            ctrl: true,
            alt: false,
        }
    }

    pub fn plain(code: KeyCode) -> Self {
        Key {
            code,
            ctrl: false,
            alt: false,
        }
    }

    pub fn char(c: char) -> Self {
        Key {
            code: KeyCode::Char(c),
            ctrl: false,
            alt: false,
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ctrl {
            write!(f, "C-")?;
        }
        if self.alt {
            write!(f, "M-")?;
        }
        match self.code {
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Up => write!(f, "Up"),
            KeyCode::Down => write!(f, "Down"),
            KeyCode::Left => write!(f, "Left"),
            KeyCode::Right => write!(f, "Right"),
            KeyCode::Enter => write!(f, "Enter"),
            KeyCode::Escape => write!(f, "Escape"),
            KeyCode::PageUp => write!(f, "PageUp"),
            KeyCode::PageDown => write!(f, "PageDown"),
            KeyCode::Home => write!(f, "Home"),
            KeyCode::End => write!(f, "End"),
            KeyCode::Space => write!(f, "Space"),
        }
    }
}

/// Decode the leading key from a raw input byte slice, returning the key and
/// the number of bytes consumed. Handles ASCII control bytes (Ctrl-letter),
/// plain printable ASCII, and the common CSI/SS3 arrow + nav escape sequences
/// that ConPTY and Unix terminals emit. Unknown sequences consume one byte as a
/// best effort so the stream never deadlocks.
pub fn decode_key(bytes: &[u8]) -> Option<(Key, usize)> {
    let first = *bytes.first()?;
    match first {
        // ESC: could be a standalone Escape, an Alt-key, or a CSI/SS3 sequence.
        0x1b => decode_escape(bytes),
        b'\r' | b'\n' => Some((Key::plain(KeyCode::Enter), 1)),
        b' ' => Some((Key::plain(KeyCode::Space), 1)),
        // Ctrl-A .. Ctrl-Z are bytes 1..=26 (excluding the special ones above).
        0x01..=0x1a => {
            let c = (b'a' + (first - 1)) as char;
            Some((Key::ctrl(c), 1))
        }
        // Other printable ASCII (space handled above).
        0x21..=0x7e => Some((Key::char(first as char), 1)),
        _ => Some((Key::char(first as char), 1)),
    }
}

fn decode_escape(bytes: &[u8]) -> Option<(Key, usize)> {
    // Bare ESC.
    if bytes.len() == 1 {
        return Some((Key::plain(KeyCode::Escape), 1));
    }
    match bytes[1] {
        b'[' => decode_csi(bytes),
        b'O' => decode_ss3(bytes),
        // ESC + printable => Alt-key.
        c @ 0x20..=0x7e => Some((
            Key {
                code: KeyCode::Char(c as char),
                ctrl: false,
                alt: true,
            },
            2,
        )),
        // Unknown; treat the ESC alone.
        _ => Some((Key::plain(KeyCode::Escape), 1)),
    }
}

fn decode_csi(bytes: &[u8]) -> Option<(Key, usize)> {
    // bytes[0]=ESC bytes[1]='['; final byte distinguishes the key.
    let final_byte = bytes.get(2)?;
    // Modified keys arrive as CSI 1 ; <mod> <letter>, e.g. ESC[1;3D = Alt-Left,
    // ESC[1;5C = Ctrl-Right. Decode the modifier and the arrow/nav letter.
    if *final_byte == b'1' && bytes.get(3) == Some(&b';') {
        if let (Some(&m), Some(&letter)) = (bytes.get(4), bytes.get(5)) {
            if let Some(code) = csi_letter_to_code(letter) {
                let (ctrl, alt) = modifier_flags(m);
                return Some((Key { code, ctrl, alt }, 6));
            }
        }
        return Some((Key::plain(KeyCode::Escape), 1));
    }
    let key = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'5' => {
            // CSI 5 ~ = PageUp
            if bytes.get(3) == Some(&b'~') {
                return Some((Key::plain(KeyCode::PageUp), 4));
            }
            return Some((Key::plain(KeyCode::Escape), 1));
        }
        b'6' => {
            if bytes.get(3) == Some(&b'~') {
                return Some((Key::plain(KeyCode::PageDown), 4));
            }
            return Some((Key::plain(KeyCode::Escape), 1));
        }
        _ => return Some((Key::plain(KeyCode::Escape), 1)),
    };
    Some((Key::plain(key), 3))
}

fn decode_ss3(bytes: &[u8]) -> Option<(Key, usize)> {
    // ESC O x — application-cursor mode arrows and Home/End.
    let final_byte = bytes.get(2)?;
    let key = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        _ => return Some((Key::plain(KeyCode::Escape), 1)),
    };
    Some((Key::plain(key), 3))
}

/// The arrow/nav letter in a CSI sequence -> KeyCode.
fn csi_letter_to_code(letter: u8) -> Option<KeyCode> {
    Some(match letter {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        _ => return None,
    })
}

/// Decode an xterm modifier digit (1 + bitmask) into (ctrl, alt). The bitmask
/// is: 1=Shift, 2=Alt, 4=Ctrl. So '2'=Shift, '3'=Alt, '5'=Ctrl, '4'=Shift+Alt…
fn modifier_flags(m: u8) -> (bool, bool) {
    let mask = m.wrapping_sub(b'1');
    let alt = mask & 0x2 != 0;
    let ctrl = mask & 0x4 != 0;
    (ctrl, alt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ctrl_letters() {
        assert_eq!(decode_key(&[0x02]).unwrap().0, Key::ctrl('b'));
        assert_eq!(decode_key(&[0x01]).unwrap().0, Key::ctrl('a'));
    }

    #[test]
    fn decodes_printable() {
        assert_eq!(decode_key(b"x").unwrap().0, Key::char('x'));
        assert_eq!(decode_key(b"|").unwrap().0, Key::char('|'));
    }

    #[test]
    fn decodes_enter_space() {
        assert_eq!(decode_key(b"\r").unwrap().0, Key::plain(KeyCode::Enter));
        assert_eq!(decode_key(b" ").unwrap().0, Key::plain(KeyCode::Space));
    }

    #[test]
    fn decodes_arrows_csi() {
        assert_eq!(decode_key(b"\x1b[A").unwrap(), (Key::plain(KeyCode::Up), 3));
        assert_eq!(
            decode_key(b"\x1b[D").unwrap(),
            (Key::plain(KeyCode::Left), 3)
        );
    }

    #[test]
    fn decodes_arrows_ss3() {
        assert_eq!(
            decode_key(b"\x1bOB").unwrap(),
            (Key::plain(KeyCode::Down), 3)
        );
    }

    #[test]
    fn decodes_alt_arrows() {
        let (k, n) = decode_key(b"\x1b[1;3D").unwrap();
        assert_eq!(n, 6);
        assert_eq!(k.code, KeyCode::Left);
        assert!(k.alt && !k.ctrl);
        let (k, _) = decode_key(b"\x1b[1;3A").unwrap();
        assert_eq!(k.code, KeyCode::Up);
        assert!(k.alt);
    }

    #[test]
    fn decodes_ctrl_arrows() {
        let (k, n) = decode_key(b"\x1b[1;5C").unwrap();
        assert_eq!(n, 6);
        assert_eq!(k.code, KeyCode::Right);
        assert!(k.ctrl && !k.alt);
    }

    #[test]
    fn decodes_page_nav() {
        assert_eq!(
            decode_key(b"\x1b[5~").unwrap(),
            (Key::plain(KeyCode::PageUp), 4)
        );
        assert_eq!(
            decode_key(b"\x1b[6~").unwrap(),
            (Key::plain(KeyCode::PageDown), 4)
        );
    }

    #[test]
    fn bare_escape_and_alt() {
        assert_eq!(
            decode_key(b"\x1b").unwrap(),
            (Key::plain(KeyCode::Escape), 1)
        );
        let (k, n) = decode_key(b"\x1bx").unwrap();
        assert_eq!(n, 2);
        assert!(k.alt);
        assert_eq!(k.code, KeyCode::Char('x'));
    }
}
