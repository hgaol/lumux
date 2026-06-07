//! Layout engine: turn a window's pane split tree into per-pane rectangles.
//!
//! Given a viewport (the effective session size — already reduced to the
//! smallest attached client by the model), assign each pane a [`Rect`]. Splits
//! reserve a one-cell divider between children, matching tmux's borders. The
//! computation is pure and deterministic, so it is fully golden-testable on any
//! platform.

use crate::model::{PaneId, PaneNode, SplitDir};
use std::collections::BTreeMap;

/// A rectangle in character cells. Origin is top-left (0,0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, cols: u16, rows: u16) -> Self {
        Self { x, y, cols, rows }
    }

    pub fn area(&self) -> u32 {
        self.cols as u32 * self.rows as u32
    }

    pub fn contains_point(&self, px: u16, py: u16) -> bool {
        px >= self.x && px < self.x + self.cols && py >= self.y && py < self.y + self.rows
    }
}

/// Width of the divider drawn between split children, in cells.
const DIVIDER: u16 = 1;

/// Compute the rectangle for every pane in `node`, packed into `viewport`.
///
/// Never panics on degenerate viewports: a child that cannot fit is clamped to
/// zero size (it still appears in the map so callers can detect "too small").
pub fn compute(node: &PaneNode, viewport: Rect) -> BTreeMap<PaneId, Rect> {
    let mut out = BTreeMap::new();
    place(node, viewport, &mut out);
    out
}

fn place(node: &PaneNode, area: Rect, out: &mut BTreeMap<PaneId, Rect>) {
    match node {
        PaneNode::Leaf(id) => {
            out.insert(*id, area);
        }
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_rect(area, *dir, *ratio);
            place(first, a, out);
            place(second, b, out);
        }
    }
}

/// Divide `area` into two sub-rectangles per `dir`/`ratio`, reserving a divider
/// cell between them. Saturating arithmetic keeps degenerate sizes safe.
fn split_rect(area: Rect, dir: SplitDir, ratio: f32) -> (Rect, Rect) {
    let ratio = ratio.clamp(0.0, 1.0);
    match dir {
        SplitDir::Horizontal => {
            // Side by side; divider is a vertical line costing 1 column.
            let usable = area.cols.saturating_sub(DIVIDER);
            let first_cols = ((usable as f32) * ratio).round() as u16;
            let first_cols = first_cols.min(usable);
            let second_cols = usable - first_cols;
            let first = Rect::new(area.x, area.y, first_cols, area.rows);
            let second = Rect::new(
                area.x + first_cols + DIVIDER,
                area.y,
                second_cols,
                area.rows,
            );
            (first, second)
        }
        SplitDir::Vertical => {
            // Stacked; divider is a horizontal line costing 1 row.
            let usable = area.rows.saturating_sub(DIVIDER);
            let first_rows = ((usable as f32) * ratio).round() as u16;
            let first_rows = first_rows.min(usable);
            let second_rows = usable - first_rows;
            let first = Rect::new(area.x, area.y, area.cols, first_rows);
            let second = Rect::new(
                area.x,
                area.y + first_rows + DIVIDER,
                area.cols,
                second_rows,
            );
            (first, second)
        }
    }
}

/// Locate the pane whose rectangle contains a point — used for click/selection
/// and directional pane navigation later.
pub fn pane_at(layout: &BTreeMap<PaneId, Rect>, x: u16, y: u16) -> Option<PaneId> {
    layout
        .iter()
        .find(|(_, r)| r.contains_point(x, y))
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PaneId;

    fn p(n: u32) -> PaneId {
        PaneId(n)
    }

    fn vp(cols: u16, rows: u16) -> Rect {
        Rect::new(0, 0, cols, rows)
    }

    #[test]
    fn single_pane_fills_viewport() {
        let t = PaneNode::leaf(p(1));
        let l = compute(&t, vp(80, 24));
        assert_eq!(l[&p(1)], Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn horizontal_split_reserves_divider() {
        // 80 cols, divider=1 => 79 usable, even => 40 / 39 (round).
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let l = compute(&t, vp(80, 24));
        let a = l[&p(1)];
        let b = l[&p(2)];
        assert_eq!(a, Rect::new(0, 0, 40, 24));
        assert_eq!(b, Rect::new(41, 0, 39, 24));
        // No overlap, divider gap of exactly 1 between them.
        assert_eq!(a.x + a.cols + DIVIDER, b.x);
        // Together with the divider they fill the width.
        assert_eq!(a.cols + DIVIDER + b.cols, 80);
    }

    #[test]
    fn vertical_split_reserves_divider() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Vertical);
        let l = compute(&t, vp(80, 24));
        let a = l[&p(1)];
        let b = l[&p(2)];
        // 24 rows, divider=1 => 23 usable => 12 / 11.
        assert_eq!(a, Rect::new(0, 0, 80, 12));
        assert_eq!(b, Rect::new(0, 13, 80, 11));
        assert_eq!(a.rows + DIVIDER + b.rows, 24);
    }

    #[test]
    fn nested_split_layout_is_deterministic() {
        // [1 | [2 / 3]] in an 81x24 viewport.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal); // [1|2]
        t.split_leaf(p(2), p(3), SplitDir::Vertical); // [1 | [2/3]]
        let l = compute(&t, vp(81, 24));
        // 81 cols, divider => 80 usable => 40/40.
        let r1 = l[&p(1)];
        let r2 = l[&p(2)];
        let r3 = l[&p(3)];
        assert_eq!(r1, Rect::new(0, 0, 40, 24));
        // Right half starts at x=41, width 40, split vertically: 24 rows -> 23
        // usable -> 12/11.
        assert_eq!(r2, Rect::new(41, 0, 40, 12));
        assert_eq!(r3, Rect::new(41, 13, 40, 11));
        // All three are disjoint.
        assert!(r1.area() + r2.area() + r3.area() <= vp(81, 24).area());
    }

    #[test]
    fn resize_recomputes_proportionally() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let small = compute(&t, vp(40, 10));
        let large = compute(&t, vp(120, 40));
        // Same structure, different sizes; first pane always ~half width.
        assert!(large[&p(1)].cols > small[&p(1)].cols);
        assert_eq!(large[&p(1)].rows, 40);
        assert_eq!(small[&p(1)].rows, 10);
    }

    #[test]
    fn degenerate_tiny_viewport_does_not_panic() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        t.split_leaf(p(2), p(3), SplitDir::Vertical);
        for (c, r) in [(1, 1), (0, 0), (2, 1), (1, 3), (3, 2)] {
            let l = compute(&t, vp(c, r));
            assert_eq!(l.len(), 3, "all panes present even when tiny");
            // Every rect fits within the viewport.
            for rect in l.values() {
                assert!(rect.x as u32 + rect.cols as u32 <= c as u32 + 1);
                assert!(rect.y as u32 + rect.rows as u32 <= r as u32 + 1);
            }
        }
    }

    #[test]
    fn ratio_extremes_are_clamped() {
        let t = PaneNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 2.5, // out of range, must clamp to 1.0
            first: Box::new(PaneNode::Leaf(p(1))),
            second: Box::new(PaneNode::Leaf(p(2))),
        };
        let l = compute(&t, vp(80, 24));
        // first gets all usable, second gets 0.
        assert_eq!(l[&p(1)].cols, 79);
        assert_eq!(l[&p(2)].cols, 0);
    }

    #[test]
    fn pane_at_point() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let l = compute(&t, vp(80, 24));
        assert_eq!(pane_at(&l, 0, 0), Some(p(1)));
        assert_eq!(pane_at(&l, 79, 23), Some(p(2)));
        // The divider column (x=40) belongs to no pane.
        assert_eq!(pane_at(&l, 40, 0), None);
    }
}
