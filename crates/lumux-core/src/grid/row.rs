//! A single row of cells plus helpers.
//!
//! Rows store [`termwiz::cell::Cell`] so attributes/colors survive into the
//! renderer (Phase 5) without a lossy intermediate representation. A row is a
//! fixed width; cells beyond what was written are blanks.

use termwiz::cell::{Cell, CellAttributes};

#[derive(Debug, Clone)]
pub struct Row {
    cells: Vec<Cell>,
}

impl Row {
    pub fn blank(width: usize) -> Self {
        Self {
            cells: vec![Cell::blank(); width.max(1)],
        }
    }

    pub fn width(&self) -> usize {
        self.cells.len()
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Write a grapheme cell at column `x`, growing/clamping as needed. Returns
    /// the number of columns advanced (1 for normal, 2 for wide glyphs).
    pub fn set_cell(&mut self, x: usize, cell: Cell) -> usize {
        let width = cell.width().max(1);
        if x < self.cells.len() {
            self.cells[x] = cell;
            // A wide glyph blanks the spacer cell to its right.
            if width == 2 {
                if let Some(next) = self.cells.get_mut(x + 1) {
                    *next = Cell::blank();
                }
            }
        }
        width
    }

    /// Resize to a new width, padding with blanks or truncating.
    pub fn resize(&mut self, width: usize) {
        self.cells.resize(width.max(1), Cell::blank());
    }

    /// Erase columns `[from, to)` to blanks with the given attributes.
    pub fn erase_range(&mut self, from: usize, to: usize, attrs: &CellAttributes) {
        let blank = Cell::new(' ', attrs.clone());
        for x in from..to.min(self.cells.len()) {
            self.cells[x] = blank.clone();
        }
    }

    /// ICH — insert `n` blank cells at column `x`, shifting the cells from `x`
    /// onward to the right. Cells pushed past the right edge are dropped; the row
    /// width is unchanged. New blanks take `attrs`.
    pub fn insert_blanks(&mut self, x: usize, n: usize, attrs: &CellAttributes) {
        let w = self.cells.len();
        if x >= w || n == 0 {
            return;
        }
        let n = n.min(w - x);
        // Shift right: move [x .. w-n) to [x+n .. w).
        for dst in (x + n..w).rev() {
            self.cells[dst] = self.cells[dst - n].clone();
        }
        let blank = Cell::new(' ', attrs.clone());
        for cell in self.cells[x..x + n].iter_mut() {
            *cell = blank.clone();
        }
    }

    /// DCH — delete `n` cells at column `x`, shifting the cells after them left to
    /// close the gap and filling `n` blanks at the right edge. Row width unchanged.
    pub fn delete_chars(&mut self, x: usize, n: usize, attrs: &CellAttributes) {
        let w = self.cells.len();
        if x >= w || n == 0 {
            return;
        }
        let n = n.min(w - x);
        // Shift left: move [x+n .. w) to [x .. w-n).
        for dst in x..w - n {
            self.cells[dst] = self.cells[dst + n].clone();
        }
        let blank = Cell::new(' ', attrs.clone());
        for cell in self.cells[w - n..w].iter_mut() {
            *cell = blank.clone();
        }
    }

    /// The row as a plain string (trailing blanks trimmed), for tests/snapshots.
    pub fn to_trimmed_string(&self) -> String {
        let s: String = self.cells.iter().map(|c| c.str()).collect();
        s.trim_end().to_string()
    }

    /// The full row as a string including trailing blanks (for copy-mode column
    /// math, where positions must line up with on-screen columns).
    pub fn to_string_full(&self) -> String {
        self.cells.iter().map(|c| c.str()).collect()
    }
}
