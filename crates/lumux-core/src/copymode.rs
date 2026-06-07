//! Copy-mode: scroll back through a pane's history and select text to yank.
//!
//! Copy-mode is per-pane view state layered over the [`Grid`]'s combined
//! history+visible buffer. The user enters it with `prefix [`, navigates with
//! arrows/PageUp/PageDown/vi-keys, optionally marks a selection, and yanks. The
//! daemon renders the scrolled view and, on yank, hands the text to a
//! [`Clipboard`](crate::traits::Clipboard). Live output keeps accumulating in
//! the grid while copy-mode is active; exiting returns to the live tail.

use crate::grid::Grid;
use crate::keymap::CopyKey;

/// A position in the combined history+visible buffer: (row, col).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

/// Per-pane copy-mode state.
#[derive(Debug, Clone)]
pub struct CopyMode {
    /// Index into the combined buffer of the *top* visible row.
    top: usize,
    /// Cursor position within the combined buffer.
    cursor: Pos,
    /// Selection anchor; Some once the user starts selecting.
    anchor: Option<Pos>,
    /// Viewport height (visible rows), to bound paging and clamp.
    height: usize,
}

impl CopyMode {
    /// Enter copy-mode anchored at the live tail (bottom of the screen).
    pub fn enter(grid: &Grid) -> Self {
        let (_w, h) = grid.dimensions();
        let combined = grid.combined_len();
        // Top row so the visible window shows the live screen bottom.
        let top = combined.saturating_sub(h);
        Self {
            top,
            cursor: Pos { row: top, col: 0 },
            anchor: None,
            height: h,
        }
    }

    pub fn top(&self) -> usize {
        self.top
    }

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    pub fn has_selection(&self) -> bool {
        self.anchor.is_some()
    }

    /// Begin (or restart) a selection at the cursor.
    pub fn start_selection(&mut self) {
        self.anchor = Some(self.cursor);
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Handle a navigation key. Returns true if still in copy-mode, false if the
    /// key requested exit (Quit).
    pub fn navigate(&mut self, key: CopyKey, grid: &Grid) -> bool {
        let combined = grid.combined_len();
        let max_row = combined.saturating_sub(1);
        match key {
            CopyKey::Quit => return false,
            CopyKey::StartSelection | CopyKey::Yank => {
                // Selection start / yank are handled by the daemon, not here.
            }
            CopyKey::Up => {
                self.cursor.row = self.cursor.row.saturating_sub(1);
            }
            CopyKey::Down => {
                self.cursor.row = (self.cursor.row + 1).min(max_row);
            }
            CopyKey::Left => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
            }
            CopyKey::Right => {
                self.cursor.col += 1;
            }
            CopyKey::PageUp => {
                self.cursor.row = self.cursor.row.saturating_sub(self.height);
            }
            CopyKey::PageDown => {
                self.cursor.row = (self.cursor.row + self.height).min(max_row);
            }
            CopyKey::HalfPageUp => {
                self.cursor.row = self.cursor.row.saturating_sub(self.height / 2);
            }
            CopyKey::HalfPageDown => {
                self.cursor.row = (self.cursor.row + self.height / 2).min(max_row);
            }
            CopyKey::Home => {
                self.cursor.col = 0;
            }
            CopyKey::End => {
                // Move to end of the current row's content.
                if let Some(row) = grid.combined_row(self.cursor.row) {
                    self.cursor.col = row.to_trimmed_string().chars().count();
                }
            }
        }
        self.scroll_to_cursor(grid);
        true
    }

    /// Keep the cursor row within the visible window by adjusting `top`.
    fn scroll_to_cursor(&mut self, grid: &Grid) {
        let combined = grid.combined_len();
        if self.cursor.row < self.top {
            self.top = self.cursor.row;
        } else if self.cursor.row >= self.top + self.height {
            self.top = self.cursor.row + 1 - self.height;
        }
        let max_top = combined.saturating_sub(self.height);
        self.top = self.top.min(max_top);
    }

    /// Extract the currently selected text (inclusive of cursor cell). Returns
    /// empty string when there is no selection.
    pub fn selected_text(&self, grid: &Grid) -> String {
        let Some(anchor) = self.anchor else {
            return String::new();
        };
        let (start, end) = order(anchor, self.cursor);
        let mut out = String::new();
        for row in start.row..=end.row {
            let Some(r) = grid.combined_row(row) else {
                continue;
            };
            let text: Vec<char> = r.to_string_full().chars().collect();
            let (c0, c1) = if start.row == end.row {
                (start.col, end.col)
            } else if row == start.row {
                (start.col, text.len())
            } else if row == end.row {
                (0, end.col)
            } else {
                (0, text.len())
            };
            let c1 = c1.min(text.len());
            let c0 = c0.min(c1);
            let slice: String = text[c0..c1.min(text.len())].iter().collect();
            out.push_str(slice.trim_end());
            if row != end.row {
                out.push('\n');
            }
        }
        out
    }
}

/// Order two positions so the first is earlier (row, then col).
fn order(a: Pos, b: Pos) -> (Pos, Pos) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Encode `text` as an OSC-52 set-clipboard sequence. Sent down the client
/// connection so the *client's* local terminal copies the text — the right
/// behavior for attach over SSH/tunnel.
pub fn osc52(text: &str) -> Vec<u8> {
    let b64 = base64_encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07").into_bytes()
}

/// Minimal standard base64 (avoids a dependency for one small use).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn grid_with_history() -> Grid {
        // 10 wide, 3 tall; push enough lines that some go to scrollback.
        let mut g = Grid::new(10, 3, 100);
        for i in 0..8 {
            g.feed(format!("line{i}\r\n").as_bytes());
        }
        g
    }

    #[test]
    fn enter_shows_live_tail() {
        let g = grid_with_history();
        let cm = CopyMode::enter(&g);
        // Top should place the visible window at the bottom of the buffer.
        assert_eq!(cm.top(), g.combined_len() - 3);
    }

    #[test]
    fn scroll_up_reveals_history() {
        let g = grid_with_history();
        let mut cm = CopyMode::enter(&g);
        let start_top = cm.top();
        for _ in 0..5 {
            cm.navigate(CopyKey::Up, &g);
        }
        assert!(cm.top() < start_top, "scrolling up moves the window up");
    }

    #[test]
    fn page_up_then_down_returns() {
        let g = grid_with_history();
        let mut cm = CopyMode::enter(&g);
        let bottom = cm.top();
        cm.navigate(CopyKey::PageUp, &g);
        assert!(cm.top() <= bottom);
        cm.navigate(CopyKey::PageDown, &g);
        // Should not exceed the max top (live tail).
        assert!(cm.top() <= g.combined_len().saturating_sub(3));
    }

    #[test]
    fn quit_returns_false() {
        let g = grid_with_history();
        let mut cm = CopyMode::enter(&g);
        assert!(!cm.navigate(CopyKey::Quit, &g));
    }

    #[test]
    fn selection_on_single_row() {
        let mut g = Grid::new(20, 2, 50);
        g.feed(b"hello world");
        let mut cm = CopyMode::enter(&g);
        // Move cursor to row of "hello world" (top visible row 0 in a fresh grid).
        cm.cursor = Pos { row: 0, col: 0 };
        cm.start_selection();
        cm.cursor = Pos { row: 0, col: 5 };
        let sel = cm.selected_text(&g);
        assert_eq!(sel, "hello");
    }

    #[test]
    fn selection_across_rows() {
        let mut g = Grid::new(20, 3, 50);
        g.feed(b"abc\r\ndef");
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 0 };
        cm.start_selection();
        cm.cursor = Pos { row: 1, col: 3 };
        let sel = cm.selected_text(&g);
        assert_eq!(sel, "abc\ndef");
    }

    #[test]
    fn no_selection_is_empty() {
        let g = grid_with_history();
        let cm = CopyMode::enter(&g);
        assert_eq!(cm.selected_text(&g), "");
    }

    #[test]
    fn osc52_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        let seq = String::from_utf8(osc52("hi")).unwrap();
        assert_eq!(seq, "\x1b]52;c;aGk=\x07");
    }
}
