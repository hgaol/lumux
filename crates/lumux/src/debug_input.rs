//! `lumux debug-input`: show what the outer terminal actually sends.
//!
//! lumux only ever sees the bytes its terminal chooses to hand over, and
//! terminals disagree about the mouse — several keep the right button for
//! themselves (VS Code's `terminal.integrated.rightClickBehavior`, or any
//! terminal while Shift is held) and never forward it, no matter what the
//! application asked for. When a click "does nothing", the first thing worth
//! knowing is whether it reached the process at all.
//!
//! This turns on exactly the mouse reporting the attach client uses and prints
//! each event as it arrives, so a missing report is visibly missing rather than
//! indistinguishable from a bug further in. It touches no daemon and no session.

use std::io::{self, Read, Write};

#[cfg(unix)]
use crate::term_unix::RawTerminal;
#[cfg(windows)]
use crate::term_win::RawTerminal;

/// Transcript lines kept for the post-exit replay. The alternate screen is
/// restored on the way out, so the live view is gone by then; a bounded replay
/// leaves the evidence on the user's scrollback without unbounded growth.
const TRANSCRIPT_LIMIT: usize = 400;

pub fn run() -> anyhow::Result<()> {
    // Inside a pane the attached lumux consumes mouse reports before this ever
    // sees them, so the results would describe lumux rather than the terminal.
    let nested = std::env::var_os("LUMUX").is_some();
    let mut transcript: Vec<String> = Vec::new();
    {
        let _term = RawTerminal::enter()?;
        let mut out = io::stdout();
        let _ = out.write_all(lumux_core::mouse::ENABLE.as_bytes());
        let _ = out.write_all(
            b"lumux debug-input \x1b[2m(press q to quit)\x1b[0m\r\n\
              click, right-click and scroll; every decoded event is listed below\r\n\r\n",
        );
        if nested {
            let _ = out.write_all(
                b"\x1b[33mwarning:\x1b[0m running inside a lumux pane - the attached \
                  session takes mouse events first.\r\n          Detach and run this \
                  from a plain shell to see what the terminal really sends.\r\n\r\n",
            );
        }
        let _ = out.flush();

        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        // Mirrors the daemon: an SGR report can be split across reads, so hold
        // the truncated tail rather than reporting it as stray text.
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let quit = buf[..n].iter().any(|b| *b == b'q' || *b == 0x03);
            pending.extend_from_slice(&buf[..n]);
            let (lines, rest) = decode(&pending);
            pending = rest;
            for line in lines {
                let _ = out.write_all(line.as_bytes());
                let _ = out.write_all(b"\r\n");
                if transcript.len() < TRANSCRIPT_LIMIT {
                    transcript.push(line);
                }
            }
            let _ = out.flush();
            if quit {
                break;
            }
        }
        let _ = out.write_all(lumux_core::mouse::DISABLE.as_bytes());
        let _ = out.flush();
    }
    // The guard has restored the primary screen: replay what was seen so the
    // evidence survives for a bug report.
    println!(
        "lumux debug-input transcript ({} events):",
        transcript.len()
    );
    for line in &transcript {
        println!("{line}");
    }
    if !transcript.iter().any(|line| line.contains("right")) {
        println!(
            "\nNo right-button report arrived. If you right-clicked, this terminal \
             is keeping that button for itself; use `prefix M` or `:menu` for the \
             context menu, or turn off the terminal's own right-click handling."
        );
    }
    if nested {
        println!("\n(Run outside a lumux session for a reading of the terminal itself.)");
    }
    Ok(())
}

/// Decode one read into human-readable lines, returning any trailing partial
/// mouse report to be completed by the next read.
fn decode(bytes: &[u8]) -> (Vec<String>, Vec<u8>) {
    let mut lines = Vec::new();
    let mut text: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some((ev, used)) = lumux_core::mouse::parse(&bytes[i..]) {
            flush_text(&mut lines, &mut text);
            lines.push(describe_mouse(&ev));
            i += used;
        } else if lumux_core::mouse::is_partial(&bytes[i..]) {
            flush_text(&mut lines, &mut text);
            return (lines, bytes[i..].to_vec());
        } else {
            text.push(bytes[i]);
            i += 1;
        }
    }
    flush_text(&mut lines, &mut text);
    (lines, Vec::new())
}

fn describe_mouse(ev: &lumux_core::mouse::MouseEvent) -> String {
    use lumux_core::mouse::{MouseButton, MouseKind};
    let name = |button: MouseButton| match button {
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    };
    let what = match ev.kind {
        MouseKind::Down(b) => format!("{} press", name(b)),
        MouseKind::Up(b) => format!("{} release", name(b)),
        MouseKind::Drag(b) => format!("{} drag", name(b)),
        MouseKind::Move => "motion".to_string(),
        MouseKind::ScrollUp => "scroll up".to_string(),
        MouseKind::ScrollDown => "scroll down".to_string(),
    };
    format!(
        "mouse  {what:<14} col {:<3} row {:<3} (SGR button {})",
        ev.col, ev.row, ev.raw_button
    )
}

fn flush_text(lines: &mut Vec<String>, text: &mut Vec<u8>) {
    if text.is_empty() {
        return;
    }
    lines.push(format!("keys   {}", escape(text)));
    text.clear();
}

/// Render bytes so control characters are visible rather than acted on.
fn escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x1b => out.push_str("<ESC>"),
            b'\r' => out.push_str("<CR>"),
            b'\n' => out.push_str("<LF>"),
            b'\t' => out.push_str("<TAB>"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("<{b:02x}>")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_right_press_is_reported_as_such() {
        let (lines, rest) = decode(b"\x1b[<2;10;5M");
        assert!(rest.is_empty());
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("right press") && lines[0].contains("col 9"),
            "unexpected: {}",
            lines[0]
        );
    }

    #[test]
    fn keys_and_mouse_are_reported_in_order() {
        let (lines, _) = decode(b"hi\x1b[<0;1;1Mbye");
        assert_eq!(
            lines,
            vec![
                "keys   hi".to_string(),
                lines[1].clone(),
                "keys   bye".to_string(),
            ]
        );
        assert!(lines[1].contains("left press"));
    }

    #[test]
    fn a_split_report_is_held_until_it_completes() {
        // The diagnostic must not accuse the terminal of sending garbage just
        // because a read boundary landed inside a report.
        let (lines, rest) = decode(b"\x1b[<2;10");
        assert!(lines.is_empty());
        assert_eq!(rest, b"\x1b[<2;10");

        let mut joined = rest;
        joined.extend_from_slice(b";5M");
        let (lines, rest) = decode(&joined);
        assert!(rest.is_empty());
        assert!(lines[0].contains("right press"));
    }

    #[test]
    fn control_bytes_are_shown_not_acted_on() {
        let (lines, _) = decode(b"\x1b\r\x01");
        assert_eq!(lines, vec!["keys   <ESC><CR><01>".to_string()]);
    }
}
