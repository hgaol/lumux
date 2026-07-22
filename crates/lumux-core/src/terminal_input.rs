//! Decoding for input emitted by the outer terminal itself.

const FOCUS_GAINED: &[u8] = b"\x1b[I";
const FOCUS_LOST: &[u8] = b"\x1b[O";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Ask the outer terminal to report host-window focus changes.
pub const FOCUS_ENABLE: &[u8] = b"\x1b[?1004h";
/// Stop outer-terminal focus reporting during attach teardown.
pub const FOCUS_DISABLE: &[u8] = b"\x1b[?1004l";

/// One ordered item decoded from the outer terminal's input stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OuterInputEvent {
    /// User input that must continue through the normal keymap/pane path.
    Input(Vec<u8>),
    /// A host-window focus report produced by DEC mode 1004.
    FocusChanged { focused: bool },
}

/// Streaming decoder for DEC focus reports.
///
/// Reads from a terminal are not message-framed, so `CSI I` / `CSI O` may be
/// split after either introducer byte. At most two ambiguous bytes are retained
/// between calls; callers may flush them after an idle read so a literal Escape
/// key is not delayed indefinitely.
#[derive(Debug, Default)]
pub struct OuterInputDecoder {
    pending: Vec<u8>,
    in_bracketed_paste: bool,
}

impl OuterInputDecoder {
    /// Decode one read while preserving event/input order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<OuterInputEvent> {
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(bytes);

        let mut events = Vec::new();
        let mut raw = Vec::new();
        let mut cursor = 0;
        while cursor < combined.len() {
            let tail = &combined[cursor..];

            if self.in_bracketed_paste {
                if tail.starts_with(BRACKETED_PASTE_END) {
                    raw.extend_from_slice(BRACKETED_PASTE_END);
                    cursor += BRACKETED_PASTE_END.len();
                    self.in_bracketed_paste = false;
                } else if BRACKETED_PASTE_END.starts_with(tail) {
                    self.pending.extend_from_slice(tail);
                    break;
                } else {
                    raw.push(combined[cursor]);
                    cursor += 1;
                }
                continue;
            }

            if tail.starts_with(BRACKETED_PASTE_START) {
                raw.extend_from_slice(BRACKETED_PASTE_START);
                cursor += BRACKETED_PASTE_START.len();
                self.in_bracketed_paste = true;
                continue;
            }

            let focused = if tail.starts_with(FOCUS_GAINED) {
                Some(true)
            } else if tail.starts_with(FOCUS_LOST) {
                Some(false)
            } else {
                None
            };

            if let Some(focused) = focused {
                push_input(&mut events, &mut raw);
                events.push(OuterInputEvent::FocusChanged { focused });
                cursor += FOCUS_GAINED.len();
                continue;
            }

            if FOCUS_GAINED.starts_with(tail)
                || FOCUS_LOST.starts_with(tail)
                || BRACKETED_PASTE_START.starts_with(tail)
            {
                self.pending.extend_from_slice(tail);
                break;
            }

            raw.push(combined[cursor]);
            cursor += 1;
        }
        push_input(&mut events, &mut raw);
        events
    }

    /// Release an incomplete focus introducer as ordinary user input.
    pub fn flush_pending(&mut self) -> Option<OuterInputEvent> {
        if self.pending.is_empty() {
            return None;
        }
        // A retained prefix while inside paste can only be the beginning of the
        // bracketed-paste end marker. Once an idle timeout releases that prefix
        // as literal input, it can never be completed on a later read; fail open
        // so focus reports and ordinary input are not treated as pasted forever.
        if self.in_bracketed_paste {
            self.in_bracketed_paste = false;
        }
        Some(OuterInputEvent::Input(std::mem::take(&mut self.pending)))
    }
}

fn push_input(events: &mut Vec<OuterInputEvent>, bytes: &mut Vec<u8>) {
    if !bytes.is_empty() {
        events.push(OuterInputEvent::Input(std::mem::take(bytes)));
    }
}

#[cfg(test)]
mod tests {
    use super::{OuterInputDecoder, OuterInputEvent, FOCUS_DISABLE, FOCUS_ENABLE};

    #[test]
    fn dec_focus_reporting_has_symmetric_lifecycle_sequences() {
        assert_eq!(FOCUS_ENABLE, b"\x1b[?1004h");
        assert_eq!(FOCUS_DISABLE, b"\x1b[?1004l");
    }

    #[test]
    fn focus_reports_are_structured_without_leaking_into_pane_input() {
        let mut decoder = OuterInputDecoder::default();

        assert_eq!(
            decoder.feed(b"before\x1b[Obetween\x1b[Iafter"),
            vec![
                OuterInputEvent::Input(b"before".to_vec()),
                OuterInputEvent::FocusChanged { focused: false },
                OuterInputEvent::Input(b"between".to_vec()),
                OuterInputEvent::FocusChanged { focused: true },
                OuterInputEvent::Input(b"after".to_vec()),
            ]
        );
    }

    #[test]
    fn focus_report_split_across_reads_is_reassembled() {
        let mut decoder = OuterInputDecoder::default();

        assert!(decoder.feed(b"\x1b").is_empty());
        assert!(decoder.feed(b"[").is_empty());
        assert_eq!(
            decoder.feed(b"I"),
            vec![OuterInputEvent::FocusChanged { focused: true }]
        );
    }

    #[test]
    fn a_split_non_focus_sequence_remains_verbatim_input() {
        let mut decoder = OuterInputDecoder::default();

        assert!(decoder.feed(b"\x1b[").is_empty());
        assert_eq!(
            decoder.feed(b"A"),
            vec![OuterInputEvent::Input(b"\x1b[A".to_vec())]
        );
    }

    #[test]
    fn flushing_an_ambiguous_prefix_preserves_user_input() {
        let mut decoder = OuterInputDecoder::default();

        assert!(decoder.feed(b"\x1b").is_empty());
        assert_eq!(
            decoder.flush_pending(),
            Some(OuterInputEvent::Input(b"\x1b".to_vec()))
        );
        assert_eq!(decoder.flush_pending(), None);
    }

    #[test]
    fn focus_looking_bytes_inside_bracketed_paste_remain_input() {
        let mut decoder = OuterInputDecoder::default();

        assert_eq!(
            decoder.feed(b"\x1b[200~literal \x1b[I and \x1b[O\x1b[201~\x1b[I"),
            vec![
                OuterInputEvent::Input(b"\x1b[200~literal \x1b[I and \x1b[O\x1b[201~".to_vec()),
                OuterInputEvent::FocusChanged { focused: true },
            ]
        );
    }

    #[test]
    fn bracketed_paste_markers_split_across_reads_still_protect_contents() {
        let mut decoder = OuterInputDecoder::default();

        assert!(decoder.feed(b"\x1b[20").is_empty());
        assert_eq!(
            decoder.feed(b"0~x\x1b[O\x1b[20"),
            vec![OuterInputEvent::Input(b"\x1b[200~x\x1b[O".to_vec())]
        );
        assert_eq!(
            decoder.feed(b"1~\x1b[O"),
            vec![
                OuterInputEvent::Input(b"\x1b[201~".to_vec()),
                OuterInputEvent::FocusChanged { focused: false },
            ]
        );
    }

    #[test]
    fn flushing_a_partial_paste_end_does_not_trap_the_decoder_in_paste_mode() {
        let mut decoder = OuterInputDecoder::default();

        assert_eq!(
            decoder.feed(b"\x1b[200~pasted\x1b[20"),
            vec![OuterInputEvent::Input(b"\x1b[200~pasted".to_vec())]
        );
        assert_eq!(
            decoder.flush_pending(),
            Some(OuterInputEvent::Input(b"\x1b[20".to_vec()))
        );

        // Once the ambiguous prefix has been released as literal input, it can
        // no longer complete the paste-end marker. The decoder must fail open
        // rather than swallowing every later focus report forever.
        assert_eq!(
            decoder.feed(b"1~\x1b[I"),
            vec![
                OuterInputEvent::Input(b"1~".to_vec()),
                OuterInputEvent::FocusChanged { focused: true },
            ]
        );
    }
}
