//! The per-pane terminal emulator: VT bytes in, cell grid + scrollback out.
//!
//! `termwiz`'s [`Parser`] does the genuinely hard part — the byte-level VT/ANSI
//! state machine, including holding partial escape sequences across `feed`
//! calls. We interpret the decoded [`Action`]s into a flat cell grid plus a
//! scrollback ring, which is the substrate the renderer (Phase 5) and copy-mode
//! (Phase 8) build on.
//!
//! Scope is the subset a multiplexer needs: printable text, the common C0
//! controls, cursor positioning, line/screen erase, and SGR attributes. Exotic
//! modes (sixel, kitty graphics, full DEC private-mode soup) are intentionally
//! ignored for v1 rather than half-implemented.

use termwiz::cell::{Cell, CellAttributes};
use termwiz::escape::csi::{Cursor, Edit, EraseInDisplay, EraseInLine, Sgr, CSI};
use termwiz::escape::parser::Parser;
use termwiz::escape::{Action, ControlCode};

use super::row::Row;
use super::scrollback::Scrollback;

pub struct Grid {
    width: usize,
    height: usize,
    rows: Vec<Row>,
    scrollback: Scrollback,
    cursor_x: usize,
    cursor_y: usize,
    pen: CellAttributes,
    parser: Parser,
    /// Saved cursor (DECSC/DECRC).
    saved_cursor: Option<(usize, usize)>,
    /// Pending bell since last drain.
    bell: bool,
}

impl std::fmt::Debug for Grid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grid")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("cursor", &(self.cursor_x, self.cursor_y))
            .field("scrollback", &self.scrollback.len())
            .finish()
    }
}

impl Grid {
    pub fn new(width: usize, height: usize, scrollback_lines: usize) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            rows: (0..height).map(|_| Row::blank(width)).collect(),
            scrollback: Scrollback::new(scrollback_lines),
            cursor_x: 0,
            cursor_y: 0,
            pen: CellAttributes::default(),
            parser: Parser::new(),
            saved_cursor: None,
            bell: false,
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_x, self.cursor_y)
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, y: usize) -> Option<&Row> {
        self.rows.get(y)
    }

    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    /// Total number of history lines (scrollback) above the visible screen.
    pub fn history_len(&self) -> usize {
        self.scrollback.len()
    }

    /// A row from the combined history+visible buffer, where index 0 is the
    /// oldest scrollback line and `history_len()+height-1` is the bottom visible
    /// row. Used by copy-mode to scroll back through history seamlessly.
    pub fn combined_row(&self, index: usize) -> Option<&Row> {
        let hist = self.scrollback.len();
        if index < hist {
            self.scrollback.get(index)
        } else {
            self.rows.get(index - hist)
        }
    }

    /// Total rows in the combined history+visible buffer.
    pub fn combined_len(&self) -> usize {
        self.scrollback.len() + self.height
    }

    /// Take and clear the pending-bell flag.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Visible screen as plain strings (trailing blanks trimmed). For tests.
    pub fn screen_text(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.to_trimmed_string()).collect()
    }

    /// Feed raw PTY output bytes. Partial sequences are retained internally.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Collect actions first to avoid borrowing self in the parser closure.
        let mut actions = Vec::new();
        self.parser.parse(bytes, |a| actions.push(a));
        for action in actions {
            self.apply(action);
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Print(c) => self.print_char(c),
            Action::PrintString(s) => {
                for c in s.chars() {
                    self.print_char(c);
                }
            }
            Action::Control(c) => self.control(c),
            Action::CSI(csi) => self.csi(csi),
            Action::Esc(esc) => self.esc(esc),
            // Ignored for v1: device control, OSC (title handled minimally),
            // sixel, kitty images, xtgettcap.
            _ => {}
        }
    }

    fn print_char(&mut self, c: char) {
        if self.cursor_x >= self.width {
            // Autowrap to next line.
            self.cursor_x = 0;
            self.line_feed();
        }
        let cell = Cell::new(c, self.pen.clone());
        let advanced = self.rows[self.cursor_y].set_cell(self.cursor_x, cell);
        self.cursor_x += advanced;
    }

    fn control(&mut self, code: ControlCode) {
        match code {
            ControlCode::LineFeed | ControlCode::VerticalTab | ControlCode::FormFeed => {
                self.line_feed()
            }
            ControlCode::CarriageReturn => self.cursor_x = 0,
            ControlCode::Backspace => {
                self.cursor_x = self.cursor_x.saturating_sub(1);
            }
            ControlCode::HorizontalTab => {
                // Advance to next multiple-of-8 tab stop.
                let next = ((self.cursor_x / 8) + 1) * 8;
                self.cursor_x = next.min(self.width - 1);
            }
            ControlCode::Bell => self.bell = true,
            _ => {}
        }
    }

    /// Move cursor down one line, scrolling (and feeding scrollback) at bottom.
    fn line_feed(&mut self) {
        if self.cursor_y + 1 >= self.height {
            // Scroll: top line goes to scrollback, append a fresh blank line.
            let scrolled = std::mem::replace(&mut self.rows[0], Row::blank(self.width));
            self.scrollback.push(scrolled);
            self.rows.remove(0);
            self.rows.push(Row::blank(self.width));
        } else {
            self.cursor_y += 1;
        }
    }

    fn csi(&mut self, csi: CSI) {
        match csi {
            CSI::Cursor(c) => self.cursor_csi(c),
            CSI::Edit(e) => self.edit_csi(e),
            CSI::Sgr(s) => self.sgr(s),
            _ => {}
        }
    }

    fn cursor_csi(&mut self, c: Cursor) {
        match c {
            Cursor::Up(n) => self.cursor_y = self.cursor_y.saturating_sub(n as usize),
            Cursor::Down(n) => {
                self.cursor_y = (self.cursor_y + n as usize).min(self.height - 1)
            }
            Cursor::Left(n) => self.cursor_x = self.cursor_x.saturating_sub(n as usize),
            Cursor::Right(n) => {
                self.cursor_x = (self.cursor_x + n as usize).min(self.width - 1)
            }
            Cursor::Position { line, col } => {
                // OneBased -> zero-based, clamped.
                self.cursor_y = (line.as_zero_based() as usize).min(self.height - 1);
                self.cursor_x = (col.as_zero_based() as usize).min(self.width - 1);
            }
            Cursor::CharacterAbsolute(col) | Cursor::CharacterPositionAbsolute(col) => {
                self.cursor_x = (col.as_zero_based() as usize).min(self.width - 1);
            }
            Cursor::LinePositionAbsolute(line) => {
                self.cursor_y = ((line as usize).saturating_sub(1)).min(self.height - 1);
            }
            Cursor::NextLine(n) => {
                self.cursor_x = 0;
                self.cursor_y = (self.cursor_y + n as usize).min(self.height - 1);
            }
            Cursor::PrecedingLine(n) => {
                self.cursor_x = 0;
                self.cursor_y = self.cursor_y.saturating_sub(n as usize);
            }
            Cursor::SaveCursor => self.saved_cursor = Some((self.cursor_x, self.cursor_y)),
            Cursor::RestoreCursor => {
                if let Some((x, y)) = self.saved_cursor {
                    self.cursor_x = x.min(self.width - 1);
                    self.cursor_y = y.min(self.height - 1);
                }
            }
            _ => {}
        }
    }

    fn edit_csi(&mut self, e: Edit) {
        match e {
            Edit::EraseInLine(mode) => {
                let (from, to) = match mode {
                    EraseInLine::EraseToEndOfLine => (self.cursor_x, self.width),
                    EraseInLine::EraseToStartOfLine => (0, self.cursor_x + 1),
                    EraseInLine::EraseLine => (0, self.width),
                };
                let pen = self.pen.clone();
                self.rows[self.cursor_y].erase_range(from, to, &pen);
            }
            Edit::EraseInDisplay(mode) => self.erase_in_display(mode),
            Edit::EraseCharacter(n) => {
                let from = self.cursor_x;
                let to = (self.cursor_x + n as usize).min(self.width);
                let pen = self.pen.clone();
                self.rows[self.cursor_y].erase_range(from, to, &pen);
            }
            Edit::DeleteLine(n) => self.delete_lines(n as usize),
            Edit::InsertLine(n) => self.insert_lines(n as usize),
            _ => {}
        }
    }

    fn erase_in_display(&mut self, mode: EraseInDisplay) {
        let pen = self.pen.clone();
        match mode {
            EraseInDisplay::EraseToEndOfDisplay => {
                self.rows[self.cursor_y].erase_range(self.cursor_x, self.width, &pen);
                for y in (self.cursor_y + 1)..self.height {
                    self.rows[y] = Row::blank(self.width);
                }
            }
            EraseInDisplay::EraseToStartOfDisplay => {
                for y in 0..self.cursor_y {
                    self.rows[y] = Row::blank(self.width);
                }
                self.rows[self.cursor_y].erase_range(0, self.cursor_x + 1, &pen);
            }
            EraseInDisplay::EraseDisplay => {
                for y in 0..self.height {
                    self.rows[y] = Row::blank(self.width);
                }
            }
            EraseInDisplay::EraseScrollback => {
                self.scrollback.clear();
            }
        }
    }

    fn delete_lines(&mut self, n: usize) {
        let n = n.min(self.height - self.cursor_y);
        for _ in 0..n {
            self.rows.remove(self.cursor_y);
            self.rows.push(Row::blank(self.width));
        }
    }

    fn insert_lines(&mut self, n: usize) {
        let n = n.min(self.height - self.cursor_y);
        for _ in 0..n {
            self.rows.insert(self.cursor_y, Row::blank(self.width));
            self.rows.pop();
        }
    }

    fn sgr(&mut self, sgr: Sgr) {
        match sgr {
            Sgr::Reset => self.pen = CellAttributes::default(),
            Sgr::Intensity(i) => {
                self.pen.set_intensity(i);
            }
            Sgr::Underline(u) => {
                self.pen.set_underline(u);
            }
            Sgr::Italic(on) => {
                self.pen.set_italic(on);
            }
            Sgr::Inverse(on) => {
                self.pen.set_reverse(on);
            }
            Sgr::Invisible(on) => {
                self.pen.set_invisible(on);
            }
            Sgr::StrikeThrough(on) => {
                self.pen.set_strikethrough(on);
            }
            Sgr::Foreground(c) => {
                self.pen.set_foreground(c);
            }
            Sgr::Background(c) => {
                self.pen.set_background(c);
            }
            _ => {}
        }
    }

    fn esc(&mut self, esc: termwiz::escape::Esc) {
        use termwiz::escape::{Esc, EscCode};
        if let Esc::Code(code) = esc {
            match code {
                EscCode::DecSaveCursorPosition => {
                    self.saved_cursor = Some((self.cursor_x, self.cursor_y))
                }
                EscCode::DecRestoreCursorPosition => {
                    if let Some((x, y)) = self.saved_cursor {
                        self.cursor_x = x.min(self.width - 1);
                        self.cursor_y = y.min(self.height - 1);
                    }
                }
                EscCode::Index => self.line_feed(),
                EscCode::NextLine => {
                    self.cursor_x = 0;
                    self.line_feed();
                }
                _ => {}
            }
        }
    }

    /// Resize the screen. Width change re-pads rows; height change adds/removes
    /// rows from the top (feeding scrollback when shrinking).
    pub fn resize(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        let height = height.max(1);
        if width != self.width {
            for r in &mut self.rows {
                r.resize(width);
            }
        }
        if height < self.height {
            // Remove from the top, pushing into scrollback.
            let remove = self.height - height;
            for _ in 0..remove {
                let r = self.rows.remove(0);
                self.scrollback.push(r);
            }
            self.cursor_y = self.cursor_y.saturating_sub(remove);
        } else if height > self.height {
            for _ in 0..(height - self.height) {
                self.rows.push(Row::blank(width));
            }
        }
        self.width = width;
        self.height = height;
        self.cursor_x = self.cursor_x.min(width - 1);
        self.cursor_y = self.cursor_y.min(height - 1);
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
