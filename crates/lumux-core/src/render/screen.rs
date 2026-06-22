//! A composited screen: the full client viewport as one cell buffer.
//!
//! The daemon builds a `Screen` each render tick by blitting every visible
//! pane's grid into its layout rectangle, drawing split borders, and writing
//! the status bar. The differ then compares the new `Screen` to the client's
//! previous one and emits minimal VT. Keeping composition and diffing over a
//! single flat buffer keeps the renderer simple and fully testable.

use termwiz::cell::{Cell, CellAttributes};

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

    /// Set a single character with default attributes at (x,y). Convenience for
    /// callers that don't depend on termwiz types (e.g. the daemon's copy-mode
    /// overpaint), clipped to the screen bounds.
    pub fn set_char(&mut self, x: usize, y: usize, ch: char) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = Cell::new(ch, CellAttributes::default());
        }
    }

    /// Write a string with given attributes starting at (x,y), clipped to the
    /// row. Returns the next x after the written text.
    pub fn write_str(&mut self, x: usize, y: usize, s: &str, attrs: &CellAttributes) -> usize {
        let mut cx = x;
        for ch in s.chars() {
            if cx >= self.width {
                break;
            }
            self.set_cell(cx, y, Cell::new(ch, attrs.clone()));
            cx += 1;
        }
        cx
    }

    /// Write plain (default-attribute) text at (x,y). Convenience for callers
    /// that don't depend on termwiz types (e.g. the daemon's copy-mode view).
    pub fn write_plain(&mut self, x: usize, y: usize, s: &str) {
        self.write_str(x, y, s, &CellAttributes::default());
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
        self.write_str(0, y, text, &attrs);
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
        // Clip text to the segment width.
        let clipped: String = text.chars().take(width).collect();
        self.write_str(x, y, &clipped, &attrs);
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
