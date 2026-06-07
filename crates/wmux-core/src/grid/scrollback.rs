//! Bounded scrollback ring buffer.
//!
//! Lines evicted off the top of the live screen flow into here. Capacity is in
//! lines; when full, the oldest line is dropped. Copy-mode (Phase 8) navigates
//! this buffer joined with the live screen.

use super::row::Row;
use std::collections::VecDeque;

#[derive(Debug)]
pub struct Scrollback {
    lines: VecDeque<Row>,
    capacity: usize,
}

impl Scrollback {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Push a line that scrolled off the top of the screen. Drops the oldest
    /// line if at capacity.
    pub fn push(&mut self, row: Row) {
        if self.capacity == 0 {
            return;
        }
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(row);
    }

    /// Index from the oldest (0) to newest (len-1).
    pub fn get(&self, idx: usize) -> Option<&Row> {
        self.lines.get(idx)
    }

    /// Iterate oldest -> newest.
    pub fn iter(&self) -> impl Iterator<Item = &Row> {
        self.lines.iter()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut sb = Scrollback::new(3);
        for i in 0..5 {
            let mut r = Row::blank(4);
            r.set_cell(0, termwiz::cell::Cell::new((b'0' + i) as char, Default::default()));
            sb.push(r);
        }
        assert_eq!(sb.len(), 3);
        // Oldest two ('0','1') evicted; remaining are '2','3','4'.
        assert_eq!(sb.get(0).unwrap().to_trimmed_string(), "2");
        assert_eq!(sb.get(2).unwrap().to_trimmed_string(), "4");
    }

    #[test]
    fn zero_capacity_never_stores() {
        let mut sb = Scrollback::new(0);
        sb.push(Row::blank(4));
        assert!(sb.is_empty());
    }
}
