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
    /// When true, the selection is a column-bounded rectangle (tmux block /
    /// rectangle-toggle) rather than the default line-wise stream.
    rectangle: bool,
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
            rectangle: false,
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

    /// Whether the selection is in rectangle (block) mode.
    pub fn is_rectangle(&self) -> bool {
        self.rectangle
    }

    /// Toggle rectangle (block) selection (tmux `rectangle-toggle`, copy-mode
    /// `Ctrl-v` / `R`). Also begins a selection at the cursor if none is active,
    /// so a single keypress both starts and shapes a block — matching tmux, where
    /// rectangle-toggle in mid-air starts selecting. Returns the new state.
    pub fn toggle_rectangle(&mut self) -> bool {
        self.rectangle = !self.rectangle;
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
        self.rectangle
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
            CopyKey::StartSelection | CopyKey::Yank | CopyKey::RectangleToggle => {
                // Selection start / yank / rectangle-toggle are handled by the
                // daemon (which calls start_selection / toggle_rectangle), not
                // here. No cursor movement.
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
            CopyKey::LineStart => {
                self.cursor.col = 0;
            }
            CopyKey::LineFirstNonBlank => {
                self.cursor.col = first_non_blank(grid, self.cursor.row);
            }
            CopyKey::Top => {
                self.cursor.row = 0;
                self.cursor.col = 0;
            }
            CopyKey::Bottom => {
                self.cursor.row = max_row;
                self.cursor.col = 0;
            }
            CopyKey::WordForward => {
                let (r, c) = next_word_start(grid, self.cursor.row, self.cursor.col, max_row);
                self.cursor.row = r;
                self.cursor.col = c;
            }
            CopyKey::WordBackward => {
                let (r, c) = prev_word_start(grid, self.cursor.row, self.cursor.col);
                self.cursor.row = r;
                self.cursor.col = c;
            }
            CopyKey::WordEnd => {
                let (r, c) = next_word_end(grid, self.cursor.row, self.cursor.col, max_row);
                self.cursor.row = r;
                self.cursor.col = c;
            }
        }
        self.scroll_to_cursor(grid);
        true
    }

    /// Place the cursor at an absolute buffer position (row clamped to the last
    /// combined-buffer row, col left as-is) and scroll it into view. Unlike
    /// [`navigate`], which moves relative to the current cursor, this jumps to a
    /// point — used by the mouse to anchor/extend a selection under the pointer.
    pub fn set_cursor(&mut self, pos: Pos, grid: &Grid) {
        let max_row = grid.combined_len().saturating_sub(1);
        self.cursor = Pos {
            row: pos.row.min(max_row),
            col: pos.col,
        };
        self.scroll_to_cursor(grid);
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
    /// empty string when there is no selection. In rectangle mode the selection
    /// is a column-bounded block; otherwise it's the usual line-wise stream.
    pub fn selected_text(&self, grid: &Grid) -> String {
        let Some(anchor) = self.anchor else {
            return String::new();
        };
        if self.rectangle {
            return self.selected_block(anchor, grid);
        }
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

    /// The highlighted column range `[start, end)` on combined-buffer `row`, or
    /// `None` if `row` is outside the selection. `width` is the pane's column
    /// count, used to extend a stream selection's first/middle rows to the right
    /// edge (matching how `selected_text` takes the rest of the line). Pure — no
    /// grid needed — so the renderer can paint the reverse-video highlight and
    /// the yank stays the single source of truth for the actual text.
    pub fn selection_span(&self, row: usize, width: usize) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if self.rectangle {
            let r0 = anchor.row.min(self.cursor.row);
            let r1 = anchor.row.max(self.cursor.row);
            if row < r0 || row > r1 {
                return None;
            }
            let lo = anchor.col.min(self.cursor.col);
            let hi = anchor.col.max(self.cursor.col) + 1;
            return Some((lo, hi.min(width)));
        }
        let (start, end) = order(anchor, self.cursor);
        if row < start.row || row > end.row {
            return None;
        }
        // Column range mirrors `selected_text` exactly (exclusive end) so the
        // reverse-video highlight covers precisely what a yank would copy.
        let (c0, c1) = if start.row == end.row {
            (start.col, end.col)
        } else if row == start.row {
            (start.col, width)
        } else if row == end.row {
            (0, end.col)
        } else {
            (0, width)
        };
        Some((c0.min(width), c1.min(width)))
    }

    /// Extract a rectangular (block) selection: the same column range
    /// `[min(col), max(col))` taken from every row between the anchor and cursor.
    /// Each row's slice keeps its own trailing whitespace trimmed, and rows are
    /// newline-joined — matching tmux's block yank.
    fn selected_block(&self, anchor: Pos, grid: &Grid) -> String {
        let r0 = anchor.row.min(self.cursor.row);
        let r1 = anchor.row.max(self.cursor.row);
        // Column range is inclusive of the cursor cell, like the stream selection.
        let lo = anchor.col.min(self.cursor.col);
        let hi = anchor.col.max(self.cursor.col) + 1;
        let mut out = String::new();
        for row in r0..=r1 {
            if let Some(r) = grid.combined_row(row) {
                let text: Vec<char> = r.to_string_full().chars().collect();
                let c0 = lo.min(text.len());
                let c1 = hi.min(text.len());
                let slice: String = text[c0..c1].iter().collect();
                out.push_str(slice.trim_end());
            }
            if row != r1 {
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

/// Whether `c` is part of a "word" for copy-mode word motions. tmux treats
/// alphanumerics and underscore as word characters; everything else (spaces,
/// punctuation) is a separator. Blank rows contain only separators.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Column of the first non-blank character on `row` (0 if the row is blank or
/// missing). Backs the vi `^` motion.
fn first_non_blank(grid: &Grid, row: usize) -> usize {
    match row_chars(grid, row) {
        Some(chars) => chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0),
        None => 0,
    }
}

/// Next word-start at or after (`row`,`col+1`), scanning forward across rows up
/// to `max_row` (vi `w`). A word start is a word char whose predecessor on the
/// same row is a non-word char (or column 0). Returns the last position if none
/// is found, so the cursor advances to the buffer end rather than sticking.
fn next_word_start(grid: &Grid, row: usize, col: usize, max_row: usize) -> (usize, usize) {
    let mut r = row;
    let mut c = col + 1;
    while r <= max_row {
        let chars = row_chars(grid, r).unwrap_or_default();
        while c <= chars.len() {
            // At end-of-row, fall through to the next row's start.
            if c == chars.len() {
                break;
            }
            let here = is_word_char(chars[c]);
            let prev_sep = c == 0 || !is_word_char(chars[c - 1]);
            if here && prev_sep {
                return (r, c);
            }
            c += 1;
        }
        r += 1;
        c = 0;
        // A word can start at column 0 of the next row.
        if r <= max_row {
            let chars = row_chars(grid, r).unwrap_or_default();
            if chars.first().is_some_and(|&ch| is_word_char(ch)) {
                return (r, 0);
            }
        }
    }
    (max_row, row_chars(grid, max_row).map(|c| c.len()).unwrap_or(0))
}

/// Previous word-start strictly before (`row`,`col`), scanning backward across
/// rows (vi `b`). Returns (0,0) if there is no earlier word.
fn prev_word_start(grid: &Grid, row: usize, col: usize) -> (usize, usize) {
    let mut r = row;
    // Start just left of the cursor; if at column 0, drop to the previous row.
    let mut c = col;
    loop {
        let chars = row_chars(grid, r).unwrap_or_default();
        // Step left within this row looking for a word-start we can land on.
        let mut i = c;
        while i > 0 {
            i -= 1;
            let here = i < chars.len() && is_word_char(chars[i]);
            let prev_sep = i == 0 || i > chars.len() || !is_word_char(chars[i - 1]);
            if here && prev_sep {
                return (r, i);
            }
        }
        if r == 0 {
            return (0, 0);
        }
        r -= 1;
        c = row_chars(grid, r).map(|c| c.len()).unwrap_or(0);
    }
}

/// Next word-end at or after the cursor, scanning forward (vi `e`). A word end
/// is a word char whose successor is a non-word char (or end of row). Advances
/// past the current position so repeated `e` walks through words.
fn next_word_end(grid: &Grid, row: usize, col: usize, max_row: usize) -> (usize, usize) {
    let mut r = row;
    let mut c = col + 1;
    while r <= max_row {
        let chars = row_chars(grid, r).unwrap_or_default();
        while c < chars.len() {
            let here = is_word_char(chars[c]);
            let next_sep = c + 1 >= chars.len() || !is_word_char(chars[c + 1]);
            if here && next_sep {
                return (r, c);
            }
            c += 1;
        }
        r += 1;
        c = 0;
    }
    (max_row, row_chars(grid, max_row).map(|c| c.len().saturating_sub(1)).unwrap_or(0))
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

    #[test]
    fn rectangle_selection_takes_a_column_block() {
        // Three rows of equal-width content; a block selection of columns [1,3)
        // takes the same 2-char slice from each row.
        let mut g = Grid::new(20, 4, 50);
        g.feed(b"abcde\r\nfghij\r\nklmno");
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 1 };
        assert!(cm.toggle_rectangle()); // turns block mode on, anchors here
        assert!(cm.is_rectangle());
        // Extend to row 2, col 2 → columns 1..=2 of each row: "bc","gh","lm".
        cm.cursor = Pos { row: 2, col: 2 };
        assert_eq!(cm.selected_text(&g), "bc\ngh\nlm");
    }

    #[test]
    fn rectangle_is_independent_of_drag_direction() {
        // Selecting up-and-left yields the same block as down-and-right.
        let mut g = Grid::new(20, 4, 50);
        g.feed(b"abcde\r\nfghij\r\nklmno");
        let mut cm = CopyMode::enter(&g);
        // Anchor bottom-right (row 2, col 3), cursor top-left (row 0, col 1).
        cm.cursor = Pos { row: 2, col: 3 };
        cm.toggle_rectangle();
        cm.cursor = Pos { row: 0, col: 1 };
        // Columns 1..=3 of each row: "bcd","ghi","lmn".
        assert_eq!(cm.selected_text(&g), "bcd\nghi\nlmn");
    }

    #[test]
    fn stream_and_block_differ_across_rows() {
        // The same anchor/cursor yields different text in stream vs block mode.
        let mut g = Grid::new(20, 3, 50);
        g.feed(b"abcde\r\nfghij");
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 2 };
        cm.start_selection(); // stream mode
        cm.cursor = Pos { row: 1, col: 1 };
        // Stream: from (0,2) to (1,1). End col is exclusive of the cursor cell in
        // stream mode, so row 1 contributes cols 0..1 = "f".
        assert_eq!(cm.selected_text(&g), "cde\nf");
        // Toggle to block: columns 1..=2 of both rows = "bc","gh".
        cm.toggle_rectangle();
        assert_eq!(cm.selected_text(&g), "bc\ngh");
    }

    #[test]
    fn toggle_rectangle_starts_selection_when_none() {
        let g = grid_with_history();
        let mut cm = CopyMode::enter(&g);
        assert!(!cm.has_selection());
        cm.toggle_rectangle();
        assert!(cm.has_selection(), "toggling block mid-air begins a selection");
        assert!(cm.is_rectangle());
    }

    #[test]
    fn set_cursor_clamps_row_and_scrolls_into_view() {
        // Grid taller than the viewport so there's scrollback to clamp against.
        let mut g = Grid::new(20, 3, 50);
        for i in 0..10 {
            g.feed(format!("line{i}\r\n").as_bytes());
        }
        let mut cm = CopyMode::enter(&g);
        let max_row = g.combined_len().saturating_sub(1);
        // A row past the end clamps to the last combined-buffer row.
        cm.set_cursor(Pos { row: 999, col: 4 }, &g);
        assert_eq!(cm.cursor().row, max_row);
        assert_eq!(cm.cursor().col, 4);
        // Jumping to the top scrolls the view so the cursor is visible.
        cm.set_cursor(Pos { row: 0, col: 0 }, &g);
        assert_eq!(cm.cursor().row, 0);
        assert!(cm.top() <= cm.cursor().row);
    }

    #[test]
    fn selection_span_stream_matches_selected_text_columns() {
        // Single row: anchor col 0, cursor col 5 → highlight cols [0,5),
        // exactly the "hello" that selected_text yields (exclusive end).
        let mut g = Grid::new(20, 2, 50);
        g.feed(b"hello world");
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 0 };
        cm.start_selection();
        cm.cursor = Pos { row: 0, col: 5 };
        assert_eq!(cm.selection_span(0, 20), Some((0, 5)));
        // Rows outside the selection are not highlighted.
        assert_eq!(cm.selection_span(1, 20), None);
    }

    #[test]
    fn selection_span_multi_row_extends_to_width() {
        // Two rows: first row runs from the anchor col to the right edge, the
        // last row from col 0 up to (exclusive) the cursor col — mirroring the
        // stream text extraction.
        let mut g = Grid::new(20, 3, 50);
        g.feed(b"abcde\r\nfghij");
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 2 };
        cm.start_selection();
        cm.cursor = Pos { row: 1, col: 3 };
        assert_eq!(cm.selection_span(0, 20), Some((2, 20)));
        assert_eq!(cm.selection_span(1, 20), Some((0, 3)));
    }

    #[test]
    fn selection_span_rectangle_is_column_bounded_every_row() {
        // Block mode: the same inclusive column range on each row in range.
        let mut g = Grid::new(20, 4, 50);
        g.feed(b"abcde\r\nfghij\r\nklmno");
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 1 };
        cm.toggle_rectangle(); // anchors here, block mode on
        cm.cursor = Pos { row: 2, col: 2 };
        // Columns 1..=2 → span [1,3) on rows 0,1,2.
        for r in 0..=2 {
            assert_eq!(cm.selection_span(r, 20), Some((1, 3)), "row {r}");
        }
        assert_eq!(cm.selection_span(3, 20), None);
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

    // Grid with a known single line of words for motion tests. 40 wide so the
    // whole sentence fits on row 0 of a fresh (no-scrollback) grid.
    fn word_grid() -> Grid {
        let mut g = Grid::new(40, 3, 50);
        g.feed(b"foo bar_baz  qux.zap");
        g
    }

    #[test]
    fn word_forward_lands_on_word_starts() {
        let g = word_grid();
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 0 }; // on 'f' of foo
        cm.navigate(CopyKey::WordForward, &g);
        assert_eq!(cm.cursor().col, 4, "w -> start of bar_baz");
        cm.navigate(CopyKey::WordForward, &g);
        // "bar_baz" is one word (underscore is a word char); next is "qux".
        assert_eq!(cm.cursor().col, 13, "w -> start of qux");
        cm.navigate(CopyKey::WordForward, &g);
        // '.' separates qux and zap.
        assert_eq!(cm.cursor().col, 17, "w -> start of zap after the dot");
    }

    #[test]
    fn word_backward_lands_on_word_starts() {
        let g = word_grid();
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 17 }; // on 'z' of zap
        cm.navigate(CopyKey::WordBackward, &g);
        assert_eq!(cm.cursor().col, 13, "b -> start of qux");
        cm.navigate(CopyKey::WordBackward, &g);
        assert_eq!(cm.cursor().col, 4, "b -> start of bar_baz");
        cm.navigate(CopyKey::WordBackward, &g);
        assert_eq!(cm.cursor().col, 0, "b -> start of foo");
    }

    #[test]
    fn word_end_lands_on_word_ends() {
        let g = word_grid();
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 0 };
        cm.navigate(CopyKey::WordEnd, &g);
        assert_eq!(cm.cursor().col, 2, "e -> last char of foo");
        cm.navigate(CopyKey::WordEnd, &g);
        assert_eq!(cm.cursor().col, 10, "e -> last char of bar_baz");
    }

    #[test]
    fn line_start_and_first_non_blank() {
        let mut g = Grid::new(20, 2, 50);
        g.feed(b"   indented");
        let mut cm = CopyMode::enter(&g);
        cm.cursor = Pos { row: 0, col: 8 };
        cm.navigate(CopyKey::LineStart, &g);
        assert_eq!(cm.cursor().col, 0, "0 -> column 0");
        cm.navigate(CopyKey::LineFirstNonBlank, &g);
        assert_eq!(cm.cursor().col, 3, "^ -> first non-blank");
    }

    #[test]
    fn top_and_bottom_jump_to_buffer_ends() {
        let g = grid_with_history();
        let mut cm = CopyMode::enter(&g);
        cm.navigate(CopyKey::Top, &g);
        assert_eq!(cm.cursor().row, 0, "g -> top of buffer");
        cm.navigate(CopyKey::Bottom, &g);
        assert_eq!(cm.cursor().row, g.combined_len() - 1, "G -> bottom of buffer");
    }
}
