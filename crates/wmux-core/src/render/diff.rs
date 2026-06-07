//! Damage-tracked diffing: previous Screen + next Screen -> minimal VT bytes.
//!
//! For each row, find runs of changed cells and emit a cursor-move to the run
//! start followed by the cells, re-emitting SGR only when the active pen
//! changes. This is what keeps the server-rendered-VT model cheap on the wire:
//! a steady-state screen with one blinking cursor sends almost nothing.

use std::fmt::Write;
use termwiz::cell::CellAttributes;

use super::screen::Screen;
use super::sgr::sgr_for;

/// Produce VT bytes that transform `prev` into `next`. `prev` must match what
/// the client currently shows. If dimensions differ, callers should use
/// [`full_repaint`] instead.
pub fn diff(prev: &Screen, next: &Screen) -> String {
    if prev.dimensions() != next.dimensions() {
        return full_repaint(next);
    }
    let (w, h) = next.dimensions();
    let mut out = String::new();
    let mut pen: Option<CellAttributes> = None;

    for y in 0..h {
        let mut x = 0;
        while x < w {
            let same = match (prev.cell(x, y), next.cell(x, y)) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            if same {
                x += 1;
                continue;
            }
            // Start of a changed run; move the cursor there (1-based VT).
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
            // Emit contiguous changed cells.
            while x < w {
                match (prev.cell(x, y), next.cell(x, y)) {
                    (Some(a), Some(b)) if a == b => break,
                    (_, Some(b)) => {
                        if pen.as_ref() != Some(b.attrs()) {
                            out.push_str(&sgr_for(b.attrs()));
                            pen = Some(b.attrs().clone());
                        }
                        out.push_str(b.str());
                        x += 1;
                    }
                    _ => break,
                }
            }
        }
    }

    // Reset attributes so they don't bleed into the cursor / next frame.
    if pen.is_some() {
        out.push_str("\x1b[0m");
    }
    apply_cursor(&mut out, next);
    out
}

/// Clear the screen and redraw everything. Used on attach and after a resize.
pub fn full_repaint(next: &Screen) -> String {
    let (w, h) = next.dimensions();
    let mut out = String::from("\x1b[2J\x1b[H");
    let mut pen: Option<CellAttributes> = None;

    for y in 0..h {
        // Find the last non-blank cell so we don't emit trailing blanks.
        let last = (0..w)
            .rev()
            .find(|&x| next.cell(x, y).map(|c| c.str() != " ").unwrap_or(false));
        let _ = write!(out, "\x1b[{};1H", y + 1);
        if let Some(last) = last {
            for x in 0..=last {
                if let Some(c) = next.cell(x, y) {
                    if pen.as_ref() != Some(c.attrs()) {
                        out.push_str(&sgr_for(c.attrs()));
                        pen = Some(c.attrs().clone());
                    }
                    out.push_str(c.str());
                }
            }
        }
    }
    out.push_str("\x1b[0m");
    apply_cursor(&mut out, next);
    out
}

fn apply_cursor(out: &mut String, next: &Screen) {
    match next.cursor() {
        Some((cx, cy)) => {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", cy + 1, cx + 1);
        }
        None => out.push_str("\x1b[?25l"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::cell::{Cell, CellAttributes};

    fn screen_with(w: usize, h: usize, text: &[&str]) -> Screen {
        let mut s = Screen::new(w, h);
        for (y, line) in text.iter().enumerate() {
            s.write_str(0, y, line, &CellAttributes::default());
        }
        s
    }

    #[test]
    fn identical_screens_emit_only_cursor() {
        let a = screen_with(10, 2, &["hello", "world"]);
        let mut b = a.clone();
        b.set_cursor(Some((0, 0)));
        let mut a2 = a.clone();
        a2.set_cursor(Some((0, 0)));
        let out = diff(&a2, &b);
        // No content writes — only the cursor reposition + show. (Check for the
        // pane glyph 'h' specifically, not the 'h' in the \x1b[?25h show-cursor
        // sequence.)
        assert!(!out.contains("hello"));
        assert!(out.contains("\x1b[1;1H"));
    }

    #[test]
    fn single_cell_change_is_minimal() {
        let a = screen_with(10, 1, &["cat"]);
        let mut b = screen_with(10, 1, &["cot"]);
        b.set_cursor(None);
        let out = diff(&a, &b);
        // Should reposition to the changed column (x=1 -> col 2) and write 'o'.
        assert!(out.contains("\x1b[1;2H"));
        assert!(out.contains('o'));
        // It must NOT rewrite the unchanged 'c' or 't' as part of the run.
        assert!(!out.contains("cot"));
    }

    #[test]
    fn dimension_change_triggers_full_repaint() {
        let a = screen_with(10, 1, &["hi"]);
        let b = screen_with(20, 2, &["hello", "there"]);
        let out = diff(&a, &b);
        assert!(out.starts_with("\x1b[2J"));
        assert!(out.contains("hello"));
        assert!(out.contains("there"));
    }

    #[test]
    fn full_repaint_skips_trailing_blanks() {
        let s = screen_with(20, 1, &["short"]);
        let out = full_repaint(&s);
        assert!(out.contains("short"));
        // No long run of spaces padding to width 20.
        assert!(!out.contains("short               "));
    }

    #[test]
    fn changed_attributes_reemit_sgr() {
        let a = screen_with(10, 1, &["x"]);
        let mut b = Screen::new(10, 1);
        let mut bold = CellAttributes::default();
        bold.set_intensity(termwiz::cell::Intensity::Bold);
        b.set_cell(0, 0, Cell::new('x', bold));
        let out = diff(&a, &b);
        // Same glyph 'x' but now bold => must re-emit and rewrite the cell.
        assert!(out.contains("\x1b[1m"));
        assert!(out.contains('x'));
    }

    #[test]
    fn hidden_cursor_emits_hide() {
        let a = screen_with(5, 1, &["a"]);
        let mut b = screen_with(5, 1, &["a"]);
        b.set_cursor(None);
        let out = diff(&a, &b);
        assert!(out.contains("\x1b[?25l"));
    }
}
