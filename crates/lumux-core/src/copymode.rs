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

/// Direction of a copy-mode search, remembered so `n`/`N` can repeat it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDir {
    /// Toward the top of the buffer (older lines) — tmux `?`.
    Backward,
    /// Toward the bottom of the buffer (newer lines) — tmux `/`.
    Forward,
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
    /// Last search query and direction, so `n`/`N` repeat without retyping.
    last_query: Option<String>,
    last_dir: SearchDir,
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
            last_query: None,
            last_dir: SearchDir::Backward,
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

    /// The remembered search query (for showing in the status line / repeating).
    pub fn last_query(&self) -> Option<&str> {
        self.last_query.as_deref()
    }

    /// Run a fresh search for `query` in `dir`, starting just past the cursor,
    /// and move the cursor to the first match. Remembers the query/direction so
    /// [`search_repeat`] (`n`/`N`) can continue it. Returns true if a match was
    /// found (cursor moved); false leaves the cursor put. An empty query is a
    /// no-op that still records nothing.
    pub fn search(&mut self, query: &str, dir: SearchDir, grid: &Grid) -> bool {
        if query.is_empty() {
            return false;
        }
        let found = self.find(query, dir, self.cursor, grid);
        self.last_query = Some(query.to_string());
        self.last_dir = dir;
        if let Some(pos) = found {
            self.cursor = pos;
            self.scroll_to_cursor(grid);
            true
        } else {
            false
        }
    }

    /// Repeat the last search. `same_dir` true is `n` (keep direction); false is
    /// `N` (reverse). No-op (returns false) if no search has run yet.
    pub fn search_repeat(&mut self, same_dir: bool, grid: &Grid) -> bool {
        let Some(query) = self.last_query.clone() else {
            return false;
        };
        let dir = match (self.last_dir, same_dir) {
            (d, true) => d,
            (SearchDir::Forward, false) => SearchDir::Backward,
            (SearchDir::Backward, false) => SearchDir::Forward,
        };
        if let Some(pos) = self.find(&query, dir, self.cursor, grid) {
            self.cursor = pos;
            self.scroll_to_cursor(grid);
            true
        } else {
            false
        }
    }

    /// Find the next occurrence of `query` relative to `from`, scanning in `dir`.
    /// Matching is plain (case-sensitive) substring over each row's full text.
    /// The search starts on the cell just after `from` (forward) or just before
    /// it (backward) so repeats advance past the current match.
    fn find(&self, query: &str, dir: SearchDir, from: Pos, grid: &Grid) -> Option<Pos> {
        let len = grid.combined_len();
        if len == 0 {
            return None;
        }
        match dir {
            SearchDir::Forward => {
                // Remainder of the start row (after the cursor col), then each
                // following row in full.
                for row in from.row..len {
                    let text = row_chars(grid, row)?;
                    let start = if row == from.row { from.col + 1 } else { 0 };
                    if let Some(col) = find_in_row(&text, query, start, true) {
                        return Some(Pos { row, col });
                    }
                }
            }
            SearchDir::Backward => {
                for row in (0..=from.row).rev() {
                    let text = row_chars(grid, row)?;
                    // On the start row, only look before the cursor col.
                    let limit = if row == from.row {
                        Some(from.col)
                    } else {
                        None
                    };
                    if let Some(col) = find_in_row_rev(&text, query, limit) {
                        return Some(Pos { row, col });
                    }
                }
            }
        }
        None
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
            CopyKey::SearchForward
            | CopyKey::SearchBackward
            | CopyKey::RepeatSearch
            | CopyKey::RepeatSearchRev => {
                // Search open/repeat is driven by the daemon (which owns the
                // query buffer and calls search()/search_repeat()), not by plain
                // navigation. No cursor movement here.
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

/// The full text of combined-buffer row `row` as a char vector, or None if the
/// row doesn't exist. Trailing blanks are kept trimmed off so a match column
/// lines up with visible content.
fn row_chars(grid: &Grid, row: usize) -> Option<Vec<char>> {
    Some(grid.combined_row(row)?.to_string_full().trim_end().chars().collect())
}

/// First column >= `start` in `hay` where `needle` matches, scanning forward.
/// `_forward` documents intent at the call site; the scan is always low→high
/// here. Column is a char index.
fn find_in_row(hay: &[char], needle: &str, start: usize, _forward: bool) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || n.len() > hay.len() {
        return None;
    }
    let last = hay.len() - n.len();
    (start..=last).find(|&i| hay[i..i + n.len()] == n[..])
}

/// Last column in `hay` where `needle` matches, scanning backward. If `limit`
/// is `Some(l)`, only matches that *start* strictly before column `l` count (so
/// a backward repeat moves past the current match). Column is a char index.
fn find_in_row_rev(hay: &[char], needle: &str, limit: Option<usize>) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || n.len() > hay.len() {
        return None;
    }
    let max_start = hay.len() - n.len();
    // Largest start index we may consider: bounded by max_start, and by limit-1
    // when a limit is set. limit == 0 means "nothing before column 0" → no match.
    let upper = match limit {
        Some(0) => return None,
        Some(l) => (l - 1).min(max_start),
        None => max_start,
    };
    (0..=upper).rev().find(|&i| hay[i..i + n.len()] == n[..])
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

    fn search_grid() -> Grid {
        // 20 wide, 4 tall, ample scrollback. Distinct markers on known rows.
        let mut g = Grid::new(20, 4, 100);
        g.feed(b"alpha needle one\r\n");   // row 0: has "needle"
        g.feed(b"beta filler line\r\n");   // row 1
        g.feed(b"gamma needle two\r\n");   // row 2: has "needle"
        g.feed(b"delta last line\r\n");    // row 3
        g
    }

    #[test]
    fn search_backward_finds_previous_match() {
        let g = search_grid();
        let mut cm = CopyMode::enter(&g);
        // Start at the bottom; search backward should land on row 2's "needle".
        cm.cursor = Pos { row: 3, col: 19 };
        assert!(cm.search("needle", SearchDir::Backward, &g));
        assert_eq!(cm.cursor().row, 2);
        assert_eq!(cm.cursor().col, "gamma ".chars().count());
    }

    #[test]
    fn search_forward_finds_next_match() {
        let g = search_grid();
        let mut cm = CopyMode::enter(&g);
        // Cursor sits just past row 0's "needle" (col 6..11), so forward search
        // skips it and lands on row 2's "needle".
        cm.cursor = Pos { row: 0, col: 12 };
        assert!(cm.search("needle", SearchDir::Forward, &g));
        assert_eq!(cm.cursor().row, 2);
        // And a match earlier on the SAME row is found when we start before it.
        cm.cursor = Pos { row: 0, col: 0 };
        assert!(cm.search("needle", SearchDir::Forward, &g));
        assert_eq!(cm.cursor().row, 0);
        assert_eq!(cm.cursor().col, "alpha ".chars().count());
    }

    #[test]
    fn search_repeat_advances_and_reverses() {
        let g = search_grid();
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 3, col: 19 };
        // Backward to row 2.
        assert!(cm.search("needle", SearchDir::Backward, &g));
        assert_eq!(cm.cursor().row, 2);
        // `n` repeats backward → row 0.
        assert!(cm.search_repeat(true, &g));
        assert_eq!(cm.cursor().row, 0);
        // `N` reverses → forward → back to row 2.
        assert!(cm.search_repeat(false, &g));
        assert_eq!(cm.cursor().row, 2);
    }

    #[test]
    fn search_miss_leaves_cursor_and_returns_false() {
        let g = search_grid();
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 3, col: 5 };
        assert!(!cm.search("zzzz-nope", SearchDir::Backward, &g));
        assert_eq!(cm.cursor().row, 3);
        assert_eq!(cm.cursor().col, 5);
    }

    #[test]
    fn search_remembers_query() {
        let g = search_grid();
        let mut cm = CopyMode::enter(&g);
        assert_eq!(cm.last_query(), None);
        cm.cursor = Pos { row: 3, col: 0 };
        cm.search("needle", SearchDir::Backward, &g);
        assert_eq!(cm.last_query(), Some("needle"));
    }

    #[test]
    fn empty_query_is_a_noop() {
        let g = search_grid();
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 2, col: 3 };
        assert!(!cm.search("", SearchDir::Forward, &g));
        assert_eq!(cm.cursor().row, 2);
        assert_eq!(cm.last_query(), None);
    }

    #[test]
    fn search_repeat_without_prior_search_is_noop() {
        let g = search_grid();
        let mut cm = CopyMode::enter(&g);
        assert!(!cm.search_repeat(true, &g));
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
