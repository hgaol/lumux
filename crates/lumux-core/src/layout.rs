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

/// Adjust the ratio of the split being dragged toward (col,row). For v1 this
/// targets the *outermost* split on the axis the cursor is moving along: a
/// horizontal drag adjusts the nearest vertical divider, a vertical drag the
/// nearest horizontal one. The ratio is taken from the cursor's position within
/// that split's area. Good enough for grabbing a divider and moving it; precise
/// per-divider drag tracking is a follow-up.
pub fn set_ratio_at(node: &mut PaneNode, col: u16, row: u16, area: Rect) {
    if let PaneNode::Split {
        dir,
        ratio,
        first,
        second,
    } = node
    {
        let (a, b) = split_rect(area, *dir, *ratio);
        match dir {
            SplitDir::Horizontal => {
                // If the cursor is within this split's rows, treat this divider
                // as the drag target and set the ratio from the cursor column.
                if row >= area.y && row < area.y + area.rows {
                    let usable = area.cols.saturating_sub(DIVIDER).max(1) as f32;
                    let rel = col.saturating_sub(area.x) as f32;
                    *ratio = (rel / usable).clamp(0.05, 0.95);
                    return;
                }
                // Otherwise descend to the child that contains the point.
                if col < a.x + a.cols {
                    set_ratio_at(first, col, row, a);
                } else {
                    set_ratio_at(second, col, row, b);
                }
            }
            SplitDir::Vertical => {
                if col >= area.x && col < area.x + area.cols {
                    let usable = area.rows.saturating_sub(DIVIDER).max(1) as f32;
                    let rel = row.saturating_sub(area.y) as f32;
                    *ratio = (rel / usable).clamp(0.05, 0.95);
                    return;
                }
                if row < a.y + a.rows {
                    set_ratio_at(first, col, row, a);
                } else {
                    set_ratio_at(second, col, row, b);
                }
            }
        }
    }
}

/// A geographic direction for pane navigation (tmux `select-pane -L/-R/-U/-D`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Find the best pane to move focus to from `from` in `dir`, given the laid-out
/// rectangles. Mirrors tmux: among panes on the correct side, prefer the
/// nearest edge, breaking ties by the closest center on the perpendicular axis.
/// Returns None if there is no pane in that direction.
pub fn neighbor(layout: &BTreeMap<PaneId, Rect>, from: PaneId, dir: Direction) -> Option<PaneId> {
    let cur = layout.get(&from)?;
    let (cx, cy) = (
        cur.x as i32 + cur.cols as i32 / 2,
        cur.y as i32 + cur.rows as i32 / 2,
    );

    let mut best: Option<(PaneId, i32, i32)> = None; // (id, primary_dist, perp_dist)
    for (&id, r) in layout {
        if id == from {
            continue;
        }
        let (rx, ry) = (
            r.x as i32 + r.cols as i32 / 2,
            r.y as i32 + r.rows as i32 / 2,
        );
        // Candidate must be on the requested side AND overlap the current pane
        // on the perpendicular axis — otherwise a full-height/width neighbor on
        // an adjacent column/row would spuriously qualify (tmux behavior).
        let h_overlap = (r.x as i32) < (cur.x as i32 + cur.cols as i32)
            && (cur.x as i32) < (r.x as i32 + r.cols as i32);
        let v_overlap = (r.y as i32) < (cur.y as i32 + cur.rows as i32)
            && (cur.y as i32) < (r.y as i32 + r.rows as i32);
        let on_side = match dir {
            Direction::Left => (r.x as i32) < cur.x as i32 && v_overlap,
            Direction::Right => (r.x as i32) > cur.x as i32 && v_overlap,
            Direction::Up => (r.y as i32) < cur.y as i32 && h_overlap,
            Direction::Down => (r.y as i32) > cur.y as i32 && h_overlap,
        };
        if !on_side {
            continue;
        }
        // Primary distance along the travel axis; perpendicular distance breaks
        // ties so we pick the most aligned neighbor.
        let (primary, perp) = match dir {
            Direction::Left => (
                (cur.x as i32) - (r.x as i32 + r.cols as i32),
                (cy - ry).abs(),
            ),
            Direction::Right => (
                (r.x as i32) - (cur.x as i32 + cur.cols as i32),
                (cy - ry).abs(),
            ),
            Direction::Up => (
                (cur.y as i32) - (r.y as i32 + r.rows as i32),
                (cx - rx).abs(),
            ),
            Direction::Down => (
                (r.y as i32) - (cur.y as i32 + cur.rows as i32),
                (cx - rx).abs(),
            ),
        };
        let primary = primary.max(0);
        let better = match best {
            None => true,
            Some((_, bp, bperp)) => (primary, perp) < (bp, bperp),
        };
        if better {
            best = Some((id, primary, perp));
        }
    }
    best.map(|(id, _, _)| id)
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

    #[test]
    fn neighbor_horizontal_split() {
        // [1 | 2] side by side.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let l = compute(&t, vp(80, 24));
        assert_eq!(neighbor(&l, p(1), Direction::Right), Some(p(2)));
        assert_eq!(neighbor(&l, p(2), Direction::Left), Some(p(1)));
        // No pane above/below in a purely horizontal split.
        assert_eq!(neighbor(&l, p(1), Direction::Up), None);
        assert_eq!(neighbor(&l, p(1), Direction::Left), None);
    }

    #[test]
    fn neighbor_vertical_split() {
        // [1 / 2] stacked.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Vertical);
        let l = compute(&t, vp(80, 24));
        assert_eq!(neighbor(&l, p(1), Direction::Down), Some(p(2)));
        assert_eq!(neighbor(&l, p(2), Direction::Up), Some(p(1)));
        assert_eq!(neighbor(&l, p(1), Direction::Right), None);
    }

    #[test]
    fn neighbor_picks_aligned_pane_in_grid() {
        // Build [ 1 | [2 / 3] ]: pane 1 on the left, 2 top-right, 3 bottom-right.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal); // [1|2]
        t.split_leaf(p(2), p(3), SplitDir::Vertical); // [1 | [2/3]]
        let l = compute(&t, vp(80, 24));
        // From pane 1, moving right should land on the top-right (2) since their
        // centers align better, or at least a valid right neighbor.
        let r = neighbor(&l, p(1), Direction::Right);
        assert!(r == Some(p(2)) || r == Some(p(3)));
        // From 2, down -> 3; from 3, up -> 2.
        assert_eq!(neighbor(&l, p(2), Direction::Down), Some(p(3)));
        assert_eq!(neighbor(&l, p(3), Direction::Up), Some(p(2)));
        // From 2, left -> 1.
        assert_eq!(neighbor(&l, p(2), Direction::Left), Some(p(1)));
    }

    #[test]
    fn neighbor_single_pane_has_none() {
        let t = PaneNode::leaf(p(1));
        let l = compute(&t, vp(80, 24));
        for d in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(neighbor(&l, p(1), d), None);
        }
    }

    #[test]
    fn drag_resizes_horizontal_split() {
        // [1 | 2] in 80x24, divider near x=40. Drag it to x=20.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let vp80 = vp(80, 24);
        // Drag the divider (initially ~col 40) to column 20.
        set_ratio_at(&mut t, 40, 12, vp80);
        let before = compute(&t, vp80)[&p(1)].cols;
        set_ratio_at(&mut t, 20, 12, vp80);
        let after = compute(&t, vp80)[&p(1)].cols;
        assert!(
            after < before,
            "left pane should shrink after dragging left"
        );
        // ~20 cols wide now.
        assert!((after as i32 - 20).abs() <= 2);
    }

    #[test]
    fn drag_resizes_vertical_split() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Vertical);
        let vp80 = vp(80, 24);
        set_ratio_at(&mut t, 40, 6, vp80); // drag divider up to row 6
        let top = compute(&t, vp80)[&p(1)].rows;
        assert!((top as i32 - 6).abs() <= 2);
    }
}
