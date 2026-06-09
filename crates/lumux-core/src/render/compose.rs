//! Compose a window's panes + borders + status bar into a [`Screen`], and
//! track per-client last-sent state to drive incremental diffs.

use std::collections::BTreeMap;
use termwiz::cell::{Cell, CellAttributes};

use super::diff::{diff, full_repaint};
use super::screen::Screen;
use crate::grid::Grid;
use crate::layout::{self, Rect};
use crate::model::{PaneId, PaneNode};

/// Inputs needed to compose one window's screen. Borrowed from daemon state.
pub struct WindowView<'a> {
    pub layout: &'a PaneNode,
    /// Each pane's emulator grid, by id (borrowed — no per-frame clone).
    pub grids: &'a BTreeMap<PaneId, &'a Grid>,
    /// The focused pane (its cursor becomes the screen cursor).
    pub active_pane: PaneId,
}

/// A single-line status bar description.
pub struct StatusBar {
    pub left: String,
    pub right: String,
}

impl StatusBar {
    /// Render into the bottom row of `screen` with reverse-video attributes.
    pub fn render(&self, screen: &mut Screen) {
        let (w, h) = screen.dimensions();
        if h == 0 {
            return;
        }
        let y = h - 1;
        let mut attrs = CellAttributes::default();
        attrs.set_reverse(true);
        // Fill the row.
        for x in 0..w {
            screen.set_cell(x, y, Cell::new(' ', attrs.clone()));
        }
        screen.write_str(0, y, &self.left, &attrs);
        // Right-align the right segment.
        let rx = w.saturating_sub(self.right.chars().count());
        screen.write_str(rx, y, &self.right, &attrs);
    }
}

/// Where the centre segment of a styled status bar is justified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Justify {
    Left,
    Centre,
    Right,
}

/// A fully styled status bar: left / centre / right span lists over a base
/// background, with the centre segment justified per [`Justify`]. Built by the
/// daemon from the config's format strings and a [`StatusContext`].
pub struct StyledStatus {
    pub left: Vec<crate::status::Span>,
    pub centre: Vec<crate::status::Span>,
    pub right: Vec<crate::status::Span>,
    pub base: CellAttributes,
    pub justify: Justify,
}

impl StyledStatus {
    /// Build a base [`CellAttributes`] from tmux color-name strings (e.g.
    /// "colour24", "white", "default") without callers depending on termwiz.
    pub fn base_attrs(bg: &str, fg: &str) -> CellAttributes {
        let mut a = CellAttributes::default();
        a.set_background(crate::status::parse_color(bg));
        a.set_foreground(crate::status::parse_color(fg));
        a
    }

    fn span_width(spans: &[crate::status::Span]) -> usize {
        spans.iter().map(|s| s.text.chars().count()).sum()
    }

    fn paint(
        screen: &mut Screen,
        y: usize,
        mut x: usize,
        spans: &[crate::status::Span],
        base: &CellAttributes,
    ) {
        let (w, _) = screen.dimensions();
        for span in spans {
            // Span attrs override the base, but inherit the base background when
            // the span doesn't set one (so the bar color fills behind text).
            for ch in span.text.chars() {
                if x >= w {
                    return;
                }
                let mut a = span.attrs.clone();
                if a.background() == termwiz::color::ColorAttribute::Default {
                    a.set_background(base.background());
                }
                screen.set_cell(x, y, Cell::new(ch, a));
                x += 1;
            }
        }
    }

    /// Render into the bottom row of `screen`.
    pub fn render(&self, screen: &mut Screen) {
        let (w, h) = screen.dimensions();
        if h == 0 {
            return;
        }
        let y = h - 1;
        // Fill the row with the base background.
        for x in 0..w {
            screen.set_cell(x, y, Cell::new(' ', self.base.clone()));
        }
        // Left segment at column 0.
        Self::paint(screen, y, 0, &self.left, &self.base);
        // Right segment right-aligned.
        let rw = Self::span_width(&self.right);
        Self::paint(screen, y, w.saturating_sub(rw), &self.right, &self.base);
        // Centre segment per justification.
        let cw = Self::span_width(&self.centre);
        let cx = match self.justify {
            Justify::Left => Self::span_width(&self.left),
            Justify::Right => w.saturating_sub(rw + cw),
            Justify::Centre => w.saturating_sub(cw) / 2,
        };
        Self::paint(screen, y, cx, &self.centre, &self.base);
    }
}

/// Compose `view` into a Screen of `size`, reserving the bottom row for
/// `status` when present. Pane cursor (of the active pane) maps to the screen.
pub fn compose(size: (usize, usize), view: &WindowView, status: Option<&StatusBar>) -> Screen {
    let (w, h) = size;
    let mut screen = Screen::new(w, h);
    let content_rows = if status.is_some() {
        h.saturating_sub(1)
    } else {
        h
    };

    let viewport = Rect::new(0, 0, w as u16, content_rows as u16);
    let rects = layout::compute(view.layout, viewport);

    let border_attrs = CellAttributes::default();
    for (&pid, rect) in &rects {
        if let Some(grid) = view.grids.get(&pid) {
            blit_pane(&mut screen, *rect, grid);
        }
        // Draw a right border if this pane doesn't reach the screen edge.
        let right = rect.x + rect.cols;
        if (right as usize) < w {
            screen.vline(
                right as usize,
                rect.y as usize,
                (rect.y + rect.rows) as usize,
                &border_attrs,
            );
        }
        // Draw a bottom border if it doesn't reach the content bottom.
        let bottom = rect.y + rect.rows;
        if (bottom as usize) < content_rows {
            screen.hline(
                bottom as usize,
                rect.x as usize,
                (rect.x + rect.cols) as usize,
                &border_attrs,
            );
        }
    }

    // Map the active pane's cursor into screen space, honoring DEC mode 25:
    // a hidden cursor (full-screen apps hide it while repainting) stays None so
    // the differ emits a hide rather than parking a stray block on screen.
    if let (Some(rect), Some(grid)) = (
        rects.get(&view.active_pane),
        view.grids.get(&view.active_pane),
    ) {
        if grid.cursor_visible() {
            let (cx, cy) = grid.cursor();
            let sx = rect.x as usize + cx;
            let sy = rect.y as usize + cy;
            if sx < w && sy < content_rows {
                screen.set_cursor(Some((sx, sy)));
            }
        }
    }

    if let Some(status) = status {
        status.render(&mut screen);
    }
    screen
}

fn blit_pane(screen: &mut Screen, rect: Rect, grid: &Grid) {
    let rows: Vec<&[Cell]> = grid.rows().iter().map(|r| r.cells()).collect();
    screen.blit_cells(
        rect.x as usize,
        rect.y as usize,
        rect.cols as usize,
        rect.rows as usize,
        &rows,
    );
}

/// Per-client renderer: remembers the last screen sent so the next frame is a
/// minimal diff. The daemon holds one of these per attached client.
pub struct ClientRenderer {
    last: Option<Screen>,
}

impl Default for ClientRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientRenderer {
    pub fn new() -> Self {
        Self { last: None }
    }

    /// Produce the VT bytes to bring this client from its last state to
    /// `next`. First call (or after [`invalidate`]) is a full repaint.
    pub fn render(&mut self, next: Screen) -> String {
        let out = match &self.last {
            Some(prev) if prev.dimensions() == next.dimensions() => diff(prev, &next),
            _ => full_repaint(&next),
        };
        self.last = Some(next);
        out
    }

    /// Force the next render to be a full repaint (e.g. after a resize or the
    /// client's terminal was disturbed).
    pub fn invalidate(&mut self) {
        self.last = None;
    }
}
