//! A composited screen: the full client viewport as one cell buffer.
//!
//! The daemon builds a `Screen` each render tick by blitting every visible
//! pane's grid into its layout rectangle, drawing split borders, and writing
//! the status bar. The differ then compares the new `Screen` to the client's
//! previous one and emits minimal VT. Keeping composition and diffing over a
//! single flat buffer keeps the renderer simple and fully testable.

use termwiz::cell::{grapheme_column_width, Cell, CellAttributes};
use unicode_segmentation::UnicodeSegmentation;

/// Terminal display width of `text`, measured in cells rather than Unicode
/// scalar values. Standalone zero-width graphemes occupy one model cell so the
/// result matches [`Screen::write_str`].
pub fn display_width(text: &str) -> usize {
    text.graphemes(true).fold(0, |width, grapheme| {
        width.saturating_add(grapheme_column_width(grapheme, None).max(1))
    })
}

#[derive(Clone, PartialEq)]
pub struct Screen {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    cursor: Option<(usize, usize)>,
}

impl Screen {
    pub fn new(width: usize, height: usize) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            cells: vec![Cell::blank(); width * height],
            cursor: None,
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn cursor(&self) -> Option<(usize, usize)> {
        self.cursor
    }

    pub fn set_cursor(&mut self, pos: Option<(usize, usize)>) {
        self.cursor = pos;
    }

    pub fn cell(&self, x: usize, y: usize) -> Option<&Cell> {
        if x < self.width && y < self.height {
            Some(&self.cells[y * self.width + x])
        } else {
            None
        }
    }

    pub fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = cell;
        }
    }

    /// Turn on reverse-video for the cell at (x,y), leaving its glyph and colors
    /// intact. Used to paint a selection highlight over already-blitted content
    /// (copy-mode drag/keyboard selection) without rebuilding the cell.
    pub fn reverse_cell(&mut self, x: usize, y: usize) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x].attrs_mut().set_reverse(true);
        }
    }

    /// Write graphemes with given attributes starting at (x,y), clipped to the
    /// row by terminal display width. Returns the next x after the written text.
    pub fn write_str(&mut self, x: usize, y: usize, s: &str, attrs: &CellAttributes) -> usize {
        self.write_str_clipped(x, y, s, attrs, self.width.saturating_sub(x))
    }

    /// Like [`write_str`](Self::write_str), but writes at most `max_width`
    /// display cells. Wide graphemes are written atomically and never cross the
    /// segment or row boundary.
    pub fn write_str_clipped(
        &mut self,
        x: usize,
        y: usize,
        s: &str,
        attrs: &CellAttributes,
        max_width: usize,
    ) -> usize {
        let mut cx = x;
        let end = x.saturating_add(max_width).min(self.width);
        for grapheme in s.graphemes(true) {
            if cx >= end {
                break;
            }

            let cell_width = display_width(grapheme);
            if cell_width > end - cx {
                break;
            }

            let cell = Cell::new_grapheme(grapheme, attrs.clone(), None);
            self.set_cell(cx, y, cell);
            for spacer_x in cx + 1..cx + cell_width {
                self.set_cell(spacer_x, y, Cell::blank_with_attrs(attrs.clone()));
            }
            cx += cell_width;
        }
        cx
    }

    /// Write plain (default-attribute) text at (x,y). Convenience for callers
    /// that don't depend on termwiz types (e.g. the daemon's copy-mode view).
    pub fn write_plain(&mut self, x: usize, y: usize, s: &str) {
        self.write_str(x, y, s, &CellAttributes::default());
    }

    /// Write default-styled text within a display-cell segment.
    pub fn write_plain_clipped(&mut self, x: usize, y: usize, s: &str, max_width: usize) {
        self.write_str_clipped(x, y, s, &CellAttributes::default(), max_width);
    }

    /// Fill row `y` with a reverse-video bar and write `text` left-aligned into
    /// it. Used for status / mode lines without exposing termwiz to callers.
    pub fn status_line(&mut self, y: usize, text: &str) {
        if y >= self.height {
            return;
        }
        let mut attrs = CellAttributes::default();
        attrs.set_reverse(true);
        for x in 0..self.width {
            self.set_cell(x, y, Cell::new(' ', attrs.clone()));
        }
        self.write_str(0, y, text, &attrs);
    }

    /// Like [`status_line`] but the reverse-video highlight spans only the first
    /// `width` columns (the rest of the row is left untouched). Used to highlight
    /// a selected row in a side-by-side list without painting over a neighboring
    /// pane/preview column.
    pub fn status_line_width(&mut self, y: usize, text: &str, width: usize) {
        if y >= self.height {
            return;
        }
        let mut attrs = CellAttributes::default();
        attrs.set_reverse(true);
        for x in 0..width.min(self.width) {
            self.set_cell(x, y, Cell::new(' ', attrs.clone()));
        }
        self.write_str_clipped(0, y, text, &attrs, width);
    }

    /// Write a reverse-video label spanning columns `[x, x+width)` of row `y`,
    /// clipping the text to that width. Used for per-window header bars in the
    /// session-switcher preview without painting the whole row.
    pub fn label_segment(&mut self, x: usize, y: usize, width: usize, text: &str) {
        if y >= self.height {
            return;
        }
        let mut attrs = CellAttributes::default();
        attrs.set_reverse(true);
        let end = (x + width).min(self.width);
        for cx in x..end {
            self.set_cell(cx, y, Cell::new(' ', attrs.clone()));
        }
        self.write_str_clipped(x, y, text, &attrs, width);
    }

    /// Blit a pane's visible cells into the rectangle at (ox,oy) of size
    /// (cols,rows). Rows/cols beyond the source are left blank.
    pub fn blit_cells(
        &mut self,
        ox: usize,
        oy: usize,
        cols: usize,
        rows: usize,
        src_rows: &[&[Cell]],
    ) {
        for ry in 0..rows {
            let row = src_rows.get(ry);
            for rx in 0..cols {
                let cell = row
                    .and_then(|r| r.get(rx))
                    .cloned()
                    .unwrap_or_else(Cell::blank);
                self.set_cell(ox + rx, oy + ry, cell);
            }
        }
    }

    /// Draw a vertical border line at column `x` spanning rows `[y0,y1)`.
    pub fn vline(&mut self, x: usize, y0: usize, y1: usize, attrs: &CellAttributes) {
        for y in y0..y1 {
            self.set_cell(x, y, Cell::new('│', attrs.clone()));
        }
    }

    /// Blit the top-left `cols` x `rows` cells of a [`Grid`] into the rectangle at
    /// (ox,oy), clipped to that size — used to preview another session's pane in
    /// the session switcher. Keeps grid/cell types inside the core crate so
    /// callers (the daemon) don't need to depend on termwiz.
    pub fn blit_grid(
        &mut self,
        ox: usize,
        oy: usize,
        cols: usize,
        rows: usize,
        grid: &crate::grid::Grid,
    ) {
        for (gy, row) in grid.rows().iter().take(rows).enumerate() {
            let cells = row.cells();
            for gx in 0..cols {
                let cell = cells.get(gx).cloned().unwrap_or_else(Cell::blank);
                self.set_cell(ox + gx, oy + gy, cell);
            }
        }
    }

    /// Blit a `cols` x `rows` window of a grid's *combined* (history + visible)
    /// buffer, starting at combined-row `top`, into the rectangle at (ox,oy).
    /// Used for the copy-mode scrolled view. Copies real cells (not a re-derived
    /// string) so wide glyphs keep their two-column layout and attributes/colors
    /// survive; rows past the end of the buffer are filled blank.
    pub fn blit_grid_scrolled(
        &mut self,
        ox: usize,
        oy: usize,
        cols: usize,
        rows: usize,
        grid: &crate::grid::Grid,
        top: usize,
    ) {
        for gy in 0..rows {
            let row = grid.combined_row(top + gy);
            for gx in 0..cols {
                let cell = row
                    .and_then(|r| r.cells().get(gx))
                    .cloned()
                    .unwrap_or_else(Cell::blank);
                self.set_cell(ox + gx, oy + gy, cell);
            }
        }
    }

    /// Draw a horizontal border line at row `y` spanning cols `[x0,x1)`.
    pub fn hline(&mut self, y: usize, x0: usize, x1: usize, attrs: &CellAttributes) {
        for x in x0..x1 {
            self.set_cell(x, y, Cell::new('─', attrs.clone()));
        }
    }

    /// Rows as cell slices, for diffing/tests.
    pub fn rows(&self) -> Vec<&[Cell]> {
        (0..self.height)
            .map(|y| &self.cells[y * self.width..(y + 1) * self.width])
            .collect()
    }

    /// A row as a trimmed string (tests).
    pub fn row_string(&self, y: usize) -> String {
        if y >= self.height {
            return String::new();
        }
        let s: String = self.cells[y * self.width..(y + 1) * self.width]
            .iter()
            .map(|c| c.str())
            .collect();
        s.trim_end().to_string()
    }
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Screen {}x{}:", self.width, self.height)?;
        for y in 0..self.height {
            writeln!(f, "  |{}|", self.row_string(y))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{diff, full_repaint};

    #[test]
    fn write_str_places_and_renders_graphemes_by_display_width() {
        let mut attrs = CellAttributes::default();
        attrs.set_reverse(true);
        let mut screen = Screen::new(7, 2);
        let label = "界👩‍💻e\u{301}Z";

        assert_eq!(screen.write_str(0, 0, label, &attrs), 6);

        let expected = [
            ("界", 2),
            (" ", 1),
            ("👩‍💻", 2),
            (" ", 1),
            ("e\u{301}", 1),
            ("Z", 1),
        ];
        for (x, (text, width)) in expected.into_iter().enumerate() {
            let cell = screen.cell(x, 0).expect("expected an in-bounds cell");
            assert_eq!((cell.str(), cell.width()), (text, width));
            assert_eq!(cell.attrs(), &attrs);
        }
        assert_eq!(screen.cell(6, 0), Some(&Cell::blank()));

        // A wide grapheme must not be partially written at the right edge.
        assert_eq!(screen.write_str(6, 1, "界!", &attrs), 6);
        assert_eq!(screen.cell(6, 1), Some(&Cell::blank()));

        let repaint = full_repaint(&screen);
        assert_eq!(repaint.matches(label).count(), 1, "{repaint:?}");

        let delta = diff(&Screen::new(7, 2), &screen);
        assert_eq!(delta.matches(label).count(), 1, "{delta:?}");
    }

    #[test]
    fn write_str_clipped_keeps_wide_graphemes_inside_the_segment() {
        let attrs = CellAttributes::default();
        let mut screen = Screen::new(4, 1);
        screen.set_cell(2, 0, Cell::new('|', attrs.clone()));

        assert_eq!(screen.write_str_clipped(0, 0, "A界", &attrs, 2), 1);
        assert_eq!(screen.cell(0, 0).map(Cell::str), Some("A"));
        assert_eq!(screen.cell(1, 0), Some(&Cell::blank()));
        assert_eq!(screen.cell(2, 0).map(Cell::str), Some("|"));
    }
}
