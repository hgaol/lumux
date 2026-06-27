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
use termwiz::escape::csi::{
    Cursor, DecPrivateMode, DecPrivateModeCode, Edit, EraseInDisplay, EraseInLine, Mode, Sgr, CSI,
};
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
    /// Saved primary screen while the alternate screen is active (DEC 1049 / 47
    /// / 1047). `None` means we're on the normal/primary screen. Full-screen apps
    /// (vim, less, htop) draw on the alt screen and the primary is restored when
    /// they exit — without this they paint over the shell and never restore it.
    primary: Option<PrimaryScreen>,
    /// DECAWM autowrap (mode 7). On by default.
    autowrap: bool,
    /// DEC text-cursor-enable (mode 25). Visible by default; apps hide it while
    /// repainting so the renderer must honor it to avoid a flickering cursor.
    cursor_visible: bool,
    /// Bytes the terminal must send back to the shell in reply to status queries
    /// (e.g. ESC[6n cursor-position report, ESC[5n device-status, ESC[c device
    /// attributes). PSReadLine and other line editors query the cursor position
    /// and stall/garble their redraw if there is no reply. The daemon drains this
    /// after feeding output and writes it to the pane's PTY (the shell's stdin).
    responses: Vec<u8>,
    /// Vertical scroll region (DECSTBM) as inclusive 0-based rows `(top, bottom)`.
    /// Line feeds and SU/SD scroll only within this region. Defaults to the full
    /// screen `(0, height-1)`. PSReadLine's ListView prediction sets a region (or
    /// uses SU) to make room for the dropdown at the bottom line; without honoring
    /// it the list draws in the wrong place and the prompt desyncs.
    scroll_top: usize,
    scroll_bottom: usize,
    /// The last grapheme printed, for REP (CSI Ps b) which repeats it.
    last_print: Option<char>,
    /// Whether the app running in this pane has enabled mouse reporting (DEC
    /// modes 1000 normal / 1002 button-event / 1003 any-event). When set, the
    /// daemon forwards raw mouse events to the app (re-encoded with pane-relative
    /// coordinates) instead of using them for its own scroll/copy-mode — matching
    /// tmux, so the wheel/clicks work inside a TUI like vim or Claude Code.
    mouse_tracking: bool,
    /// The most recent window title set by the app via OSC 0/2 (`ESC]2;…BEL`).
    /// Used by the daemon for automatic-rename. None until the app sets one.
    title: Option<String>,
}

/// The primary-screen state stashed while the alternate screen is shown.
struct PrimaryScreen {
    rows: Vec<Row>,
    cursor_x: usize,
    cursor_y: usize,
    pen: CellAttributes,
    saved_cursor: Option<(usize, usize)>,
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
            primary: None,
            autowrap: true,
            cursor_visible: true,
            responses: Vec::new(),
            scroll_top: 0,
            scroll_bottom: height - 1,
            last_print: None,
            mouse_tracking: false,
            title: None,
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_x, self.cursor_y)
    }

    /// Whether the alternate screen is currently active (a full-screen app like
    /// vim/less is running). Copy-mode uses this to suppress scrollback, since
    /// the alt screen has none.
    pub fn alt_screen(&self) -> bool {
        self.primary.is_some()
    }

    /// Whether the app in this pane has turned on mouse reporting (DEC 1000/1002/
    /// 1003). When true, the daemon forwards raw mouse events to it instead of
    /// using them for scroll/copy-mode (tmux behavior).
    pub fn wants_mouse(&self) -> bool {
        self.mouse_tracking
    }

    /// Whether the text cursor should be shown (DEC mode 25). Apps hide it while
    /// repainting; the renderer honors this so the client cursor doesn't flicker.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
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

    /// Take the bytes the terminal owes the shell in reply to status queries
    /// (cursor-position / device-status / device-attributes). Empty if none are
    /// pending. The daemon writes these to the pane's PTY after feeding output.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Visible screen as plain strings (trailing blanks trimmed). For tests.
    pub fn screen_text(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.to_trimmed_string()).collect()
    }

    /// The most recent OSC window title the app set, if any (for automatic-rename).
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
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
            // Capture window-title OSC (0 = icon+title, 2 = title) for tmux-style
            // automatic-rename; other OSCs and DCS/sixel/kitty are ignored.
            Action::OperatingSystemCommand(osc) => {
                use termwiz::escape::OperatingSystemCommand as Osc;
                match *osc {
                    Osc::SetWindowTitle(t)
                    | Osc::SetIconNameAndWindowTitle(t)
                    | Osc::SetWindowTitleSun(t) => {
                        self.title = Some(t);
                    }
                    _ => {}
                }
            }
            // Ignored for v1: device control, sixel, kitty images, xtgettcap.
            _ => {}
        }
    }

    fn print_char(&mut self, c: char) {
        if self.cursor_x >= self.width {
            if self.autowrap {
                // Autowrap to next line.
                self.cursor_x = 0;
                self.line_feed();
            } else {
                // DECAWM off: keep overprinting the last column.
                self.cursor_x = self.width - 1;
            }
        }
        let cell = Cell::new(c, self.pen.clone());
        let advanced = self.rows[self.cursor_y].set_cell(self.cursor_x, cell);
        self.cursor_x += advanced;
        self.last_print = Some(c);
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

    /// Move cursor down one line, scrolling at the bottom of the scroll region.
    fn line_feed(&mut self) {
        if self.cursor_y == self.scroll_bottom {
            // At the region's bottom edge: scroll the region up by one.
            self.scroll_up(1);
        } else if self.cursor_y + 1 < self.height {
            self.cursor_y += 1;
        }
        // If the cursor is below the region (unusual), a line feed just clamps.
    }

    /// Scroll the active region up by `n` lines: the top `n` lines leave (the
    /// region's top line is fed to scrollback only when the region spans the full
    /// screen and we're on the primary buffer — partial regions and the alt
    /// screen discard, matching real terminals), and `n` blank lines appear at
    /// the bottom. The cursor does not move.
    fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom.min(self.height - 1);
        if top > bottom {
            return;
        }
        let region_h = bottom - top + 1;
        let n = n.min(region_h);
        let full_screen = top == 0 && bottom == self.height - 1;
        for _ in 0..n {
            let scrolled = std::mem::replace(&mut self.rows[top], Row::blank(self.width));
            if full_screen && self.primary.is_none() {
                self.scrollback.push(scrolled);
            }
            self.rows.remove(top);
            self.rows.insert(bottom, Row::blank(self.width));
        }
    }

    /// Scroll the active region down by `n` lines: `n` blank lines appear at the
    /// top of the region and the bottom `n` lines leave. The cursor does not move.
    /// Reverse-index / SD; content is never fed to scrollback (it's pushed down).
    fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom.min(self.height - 1);
        if top > bottom {
            return;
        }
        let region_h = bottom - top + 1;
        let n = n.min(region_h);
        for _ in 0..n {
            self.rows.remove(bottom);
            self.rows.insert(top, Row::blank(self.width));
        }
    }

    fn csi(&mut self, csi: CSI) {
        match csi {
            CSI::Cursor(c) => self.cursor_csi(c),
            CSI::Edit(e) => self.edit_csi(e),
            CSI::Sgr(s) => self.sgr(s),
            CSI::Mode(m) => self.mode_csi(m),
            CSI::Device(d) => self.device_csi(*d),
            _ => {}
        }
    }

    /// Reply to device queries the shell sends, so line editors (PSReadLine)
    /// don't stall waiting for an answer. We answer device-status with "OK" and
    /// device-attributes with a minimal VT100 identity; cursor-position reports
    /// are produced in `cursor_csi` where the cursor is known.
    fn device_csi(&mut self, dev: termwiz::escape::csi::Device) {
        use termwiz::escape::csi::Device;
        match dev {
            // ESC[5n -> "I am OK": ESC[0n.
            Device::StatusReport => self.responses.extend_from_slice(b"\x1b[0n"),
            // ESC[c / ESC[0c -> primary device attributes. Report a basic VT100
            // with no options (ESC[?1;0c), which is what most apps expect.
            Device::RequestPrimaryDeviceAttributes => {
                self.responses.extend_from_slice(b"\x1b[?1;0c")
            }
            _ => {}
        }
    }

    /// DEC private modes we care about: alternate screen (1049/47/1047), text
    /// cursor visibility (25), and autowrap (7). Everything else is ignored.
    fn mode_csi(&mut self, mode: Mode) {
        let (set, code) = match mode {
            Mode::SetDecPrivateMode(DecPrivateMode::Code(c)) => (true, c),
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(c)) => (false, c),
            _ => return,
        };
        match code {
            DecPrivateModeCode::ClearAndEnableAlternateScreen
            | DecPrivateModeCode::EnableAlternateScreen
            | DecPrivateModeCode::OptEnableAlternateScreen => {
                if set {
                    self.enter_alt_screen();
                } else {
                    self.leave_alt_screen();
                }
            }
            DecPrivateModeCode::ShowCursor => self.cursor_visible = set,
            DecPrivateModeCode::AutoWrap => self.autowrap = set,
            // Mouse reporting: 1000 (normal), 1002 (button-event), 1003 (any-
            // event). lumux only needs to know whether the app wants mouse events
            // at all, so all three toggle one flag. (1006 SGR is just the encoding
            // and doesn't itself turn tracking on.)
            DecPrivateModeCode::MouseTracking
            | DecPrivateModeCode::ButtonEventMouse
            | DecPrivateModeCode::AnyEventMouse => self.mouse_tracking = set,
            _ => {}
        }
    }

    /// Switch to a fresh, blank alternate screen, stashing the primary one. A
    /// no-op if already on the alt screen (apps may send the sequence twice).
    fn enter_alt_screen(&mut self) {
        if self.primary.is_some() {
            return;
        }
        let saved_rows =
            std::mem::replace(&mut self.rows, (0..self.height).map(|_| Row::blank(self.width)).collect());
        self.primary = Some(PrimaryScreen {
            rows: saved_rows,
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            pen: self.pen.clone(),
            saved_cursor: self.saved_cursor,
        });
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.pen = CellAttributes::default();
        // The alt screen starts with a full-screen scroll region.
        self.scroll_top = 0;
        self.scroll_bottom = self.height - 1;
    }

    /// Restore the primary screen saved on the last `enter_alt_screen`. A no-op
    /// if we're already on the primary screen.
    fn leave_alt_screen(&mut self) {
        if let Some(p) = self.primary.take() {
            self.rows = p.rows;
            self.cursor_x = p.cursor_x.min(self.width - 1);
            self.cursor_y = p.cursor_y.min(self.height - 1);
            self.pen = p.pen;
            self.saved_cursor = p.saved_cursor;
            // Restore a full-screen region; the shell re-establishes its own.
            self.scroll_top = 0;
            self.scroll_bottom = self.height - 1;
        }
    }

    fn cursor_csi(&mut self, c: Cursor) {
        match c {
            Cursor::Up(n) => self.cursor_y = self.cursor_y.saturating_sub(n as usize),
            Cursor::Down(n) => self.cursor_y = (self.cursor_y + n as usize).min(self.height - 1),
            Cursor::Left(n) => self.cursor_x = self.cursor_x.saturating_sub(n as usize),
            Cursor::Right(n) => self.cursor_x = (self.cursor_x + n as usize).min(self.width - 1),
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
            // DECSTBM: set the vertical scroll region (1-based inclusive) and home
            // the cursor. An inverted/degenerate range resets to the full screen.
            Cursor::SetTopAndBottomMargins { top, bottom } => {
                let t = (top.as_zero_based() as usize).min(self.height - 1);
                let b = (bottom.as_zero_based() as usize).min(self.height - 1);
                if t < b {
                    self.scroll_top = t;
                    self.scroll_bottom = b;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = self.height - 1;
                }
                // DECSTBM moves the cursor to the home position of the region.
                self.cursor_x = 0;
                self.cursor_y = self.scroll_top;
            }
            Cursor::SaveCursor => self.saved_cursor = Some((self.cursor_x, self.cursor_y)),
            Cursor::RestoreCursor => {
                if let Some((x, y)) = self.saved_cursor {
                    self.cursor_x = x.min(self.width - 1);
                    self.cursor_y = y.min(self.height - 1);
                }
            }
            // ESC[6n: report the cursor position as ESC[<row>;<col>R (1-based).
            // PSReadLine and other line editors rely on this to place their
            // redraw; without a reply they hang or garble the prompt.
            Cursor::RequestActivePositionReport => {
                let row = self.cursor_y + 1;
                let col = self.cursor_x + 1;
                self.responses
                    .extend_from_slice(format!("\x1b[{row};{col}R").as_bytes());
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
            // ICH — insert blanks at the cursor, shifting the rest of the line
            // right. DCH — delete cells at the cursor, shifting the rest left.
            // Line editors (PSReadLine) use these to redraw mid-line edits; if we
            // drop them the grid's columns drift out of sync with the shell.
            Edit::InsertCharacter(n) => {
                let pen = self.pen.clone();
                self.rows[self.cursor_y].insert_blanks(self.cursor_x, n as usize, &pen);
            }
            Edit::DeleteCharacter(n) => {
                let pen = self.pen.clone();
                self.rows[self.cursor_y].delete_chars(self.cursor_x, n as usize, &pen);
            }
            // REP — repeat the last printed grapheme n times.
            Edit::Repeat(n) => {
                if let Some(c) = self.last_print {
                    for _ in 0..n {
                        self.print_char(c);
                    }
                }
            }
            // SU / SD: scroll the active region without moving the cursor.
            Edit::ScrollUp(n) => self.scroll_up(n as usize),
            Edit::ScrollDown(n) => self.scroll_down(n as usize),
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
    /// rows from the top (feeding scrollback when shrinking). The stashed primary
    /// screen (if we're on the alt screen) is resized in lockstep so it restores
    /// at the right size when the full-screen app exits.
    pub fn resize(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        let height = height.max(1);
        if width != self.width {
            for r in &mut self.rows {
                r.resize(width);
            }
        }
        let on_alt = self.primary.is_some();
        if height < self.height {
            // Remove from the top. The primary screen feeds scrollback; the alt
            // screen has no history, so its excess top rows are just dropped.
            let remove = self.height - height;
            for _ in 0..remove {
                let r = self.rows.remove(0);
                if !on_alt {
                    self.scrollback.push(r);
                }
            }
            self.cursor_y = self.cursor_y.saturating_sub(remove);
        } else if height > self.height {
            for _ in 0..(height - self.height) {
                self.rows.push(Row::blank(width));
            }
        }
        // Keep the stashed primary buffer dimensionally consistent.
        if let Some(p) = self.primary.as_mut() {
            if width != self.width {
                for r in &mut p.rows {
                    r.resize(width);
                }
            }
            if height < p.rows.len() {
                let remove = p.rows.len() - height;
                p.rows.drain(0..remove);
                p.cursor_y = p.cursor_y.saturating_sub(remove);
            } else {
                while p.rows.len() < height {
                    p.rows.push(Row::blank(width));
                }
            }
            p.cursor_x = p.cursor_x.min(width - 1);
            p.cursor_y = p.cursor_y.min(height - 1);
        }
        self.width = width;
        self.height = height;
        self.cursor_x = self.cursor_x.min(width - 1);
        self.cursor_y = self.cursor_y.min(height - 1);
        // A resize resets the scroll region to the full screen (DECSTBM is
        // viewport-relative; keeping a stale region after a resize corrupts
        // scrolling). Apps re-establish their region after a SIGWINCH.
        self.scroll_top = 0;
        self.scroll_bottom = height - 1;
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
