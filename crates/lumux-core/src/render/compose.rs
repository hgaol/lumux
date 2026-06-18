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

    /// Paint `spans` starting at column `x`, clipping at `limit` (exclusive) as
    /// well as the screen edge. Returns the column just past the last cell
    /// written. `limit` lets the caller stop a segment before it collides with a
    /// following one, so the status row is always exactly one line.
    fn paint(
        screen: &mut Screen,
        y: usize,
        mut x: usize,
        spans: &[crate::status::Span],
        base: &CellAttributes,
        limit: usize,
    ) -> usize {
        let (w, _) = screen.dimensions();
        let stop = limit.min(w);
        for span in spans {
            // Span attrs override the base, but inherit the base background when
            // the span doesn't set one (so the bar color fills behind text).
            for ch in span.text.chars() {
                if x >= stop {
                    return x;
                }
                let mut a = span.attrs.clone();
                if a.background() == termwiz::color::ColorAttribute::Default {
                    a.set_background(base.background());
                }
                screen.set_cell(x, y, Cell::new(ch, a));
                x += 1;
            }
        }
        x
    }

    /// Compute the three segment columns for width `w`: where the left segment
    /// ends, where the right segment starts, and where the centre is painted.
    /// Shared by [`render`] and [`centre_start`] so click hit-testing matches the
    /// drawn layout exactly. Guarantees `left_end <= centre_x <= right_start <= w`,
    /// which is what keeps the row a single non-overlapping line.
    fn layout_columns(&self, w: usize) -> StatusColumns {
        let lw = Self::span_width(&self.left);
        let rw = Self::span_width(&self.right);
        let cw = Self::span_width(&self.centre);
        let left_end = lw.min(w);
        let right_start = w.saturating_sub(rw).max(left_end);
        let centre_x = if right_start > left_end {
            let ideal = match self.justify {
                Justify::Left => left_end,
                Justify::Right => right_start.saturating_sub(cw),
                Justify::Centre => w.saturating_sub(cw) / 2,
            };
            ideal.clamp(left_end, right_start)
        } else {
            left_end
        };
        StatusColumns {
            left_end,
            centre_x,
            right_start,
        }
    }

    /// Render into the bottom row of `screen`. The three segments are laid out so
    /// the row is ALWAYS a single line, even when their combined width exceeds the
    /// terminal: left is clipped before right, right is clipped to the space after
    /// left, and centre fills only the gap between them. Nothing ever wraps.
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

        let cols = self.layout_columns(w);
        // Left at column 0, clipped where the right segment begins.
        Self::paint(screen, y, 0, &self.left, &self.base, cols.right_start);
        // Right, right-aligned, never before the left segment's end.
        Self::paint(screen, y, cols.right_start, &self.right, &self.base, w);
        // Centre fills only the gap [left_end, right_start); dropped if empty.
        if cols.right_start > cols.left_end {
            Self::paint(
                screen,
                y,
                cols.centre_x,
                &self.centre,
                &self.base,
                cols.right_start,
            );
        }
    }

    /// Starting column where the centre segment (the window list) is painted,
    /// for a status bar of width `w`. Mirrors the justification math in `render`
    /// so click hit-testing lines up exactly with what was drawn.
    pub fn centre_start(&self, w: usize) -> usize {
        self.layout_columns(w).centre_x
    }
}

/// The three computed column boundaries of a styled status row (see
/// [`StyledStatus::layout_columns`]).
struct StatusColumns {
    left_end: usize,
    centre_x: usize,
    right_start: usize,
}

/// Compose `view` into a Screen of `size`. The bottom row is reserved for the
/// status bar whenever `status` is supplied OR `reserve_status_row` is set — the
/// latter lets a caller (the daemon) paint its own styled status afterward while
/// still keeping panes out of that row. Pane cursor (of the active pane) maps to
/// the screen.
pub fn compose(
    size: (usize, usize),
    view: &WindowView,
    status: Option<&StatusBar>,
    reserve_status_row: bool,
) -> Screen {
    let (w, h) = size;
    let mut screen = Screen::new(w, h);
    let content_rows = if status.is_some() || reserve_status_row {
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
