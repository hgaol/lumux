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

/// Identify which split divider the point (col,row) is on, if any, returning a
/// path to that Split node: a sequence of `false`(first)/`true`(second) choices
/// from the root. Used on mouse-press to decide whether a divider was grabbed —
/// only then does a subsequent drag resize. Returns None when the point is in
/// open pane area (not on a divider line), so a plain click-drag does nothing.
pub fn divider_at(node: &PaneNode, col: u16, row: u16, area: Rect) -> Option<Vec<bool>> {
    let PaneNode::Split {
        dir,
        ratio,
        first,
        second,
    } = node
    else {
        return None;
    };
    let (a, b) = split_rect(area, *dir, *ratio);
    // Is the cursor on THIS split's divider line?
    let on_this = match dir {
        SplitDir::Horizontal => {
            let divider_col = a.x + a.cols;
            row >= area.y && row < area.y + area.rows && near(col, divider_col)
        }
        SplitDir::Vertical => {
            let divider_row = a.y + a.rows;
            col >= area.x && col < area.x + area.cols && near(row, divider_row)
        }
    };
    if on_this {
        return Some(Vec::new());
    }
    // Otherwise descend into whichever child contains the cursor; prefix the
    // path with the branch taken. This is what makes an inner divider reachable
    // even when it shares an axis with an outer split.
    if a.contains_point(col, row) {
        let mut path = divider_at(first, col, row, a)?;
        path.insert(0, false);
        Some(path)
    } else if b.contains_point(col, row) {
        let mut path = divider_at(second, col, row, b)?;
        path.insert(0, true);
        Some(path)
    } else {
        None
    }
}

/// Move the divider named by `path` (from [`divider_at`]) so its line follows
/// the cursor at (col,row). Unlike grabbing, this does NOT require the cursor to
/// still be near the divider — that's the whole point of remembering the grabbed
/// divider for the duration of a drag, so the divider tracks the pointer even as
/// it moves far away. Returns true if the path resolved to a split.
pub fn set_ratio_by_path(
    node: &mut PaneNode,
    path: &[bool],
    col: u16,
    row: u16,
    area: Rect,
) -> bool {
    let PaneNode::Split {
        dir,
        ratio,
        first,
        second,
    } = node
    else {
        return false;
    };
    match path.split_first() {
        // End of path: this is the grabbed divider — set its ratio from the
        // cursor position within this area.
        None => {
            match dir {
                SplitDir::Horizontal => {
                    let usable = area.cols.saturating_sub(DIVIDER).max(1) as f32;
                    let rel = col.saturating_sub(area.x).min(area.cols.saturating_sub(1)) as f32;
                    *ratio = (rel / usable).clamp(0.05, 0.95);
                }
                SplitDir::Vertical => {
                    let usable = area.rows.saturating_sub(DIVIDER).max(1) as f32;
                    let rel = row.saturating_sub(area.y).min(area.rows.saturating_sub(1)) as f32;
                    *ratio = (rel / usable).clamp(0.05, 0.95);
                }
            }
            true
        }
        // Descend into the recorded branch with that child's sub-rectangle.
        Some((&branch, tail)) => {
            let (a, b) = split_rect(area, *dir, *ratio);
            if branch {
                set_ratio_by_path(second, tail, col, row, b)
            } else {
                set_ratio_by_path(first, tail, col, row, a)
            }
        }
    }
}

/// How many cells away from a divider line still counts as grabbing it. One
/// cell of slack on each side makes the thin divider easy to catch with a mouse.
const GRAB: u16 = 1;

/// True if `pos` is within [`GRAB`] cells of `target`.
fn near(pos: u16, target: u16) -> bool {
    pos.abs_diff(target) <= GRAB
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
        // [1 | 2] in 80x24. Press on the divider to grab it, then drag far left
        // to ~col 20 — the divider must track the cursor even though it's now far
        // from where it started.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let vp80 = vp(80, 24);
        let divider = compute(&t, vp80)[&p(1)].cols; // divider column
        let before = compute(&t, vp80)[&p(1)].cols;
        let path = divider_at(&t, divider, 12, vp80).expect("press should grab the divider");
        assert!(set_ratio_by_path(&mut t, &path, 20, 12, vp80));
        let after = compute(&t, vp80)[&p(1)].cols;
        assert!(after < before, "left pane should shrink after dragging left");
        assert!((after as i32 - 20).abs() <= 2);
    }

    #[test]
    fn drag_resizes_vertical_split() {
        // [1 / 2] stacked. Grab the horizontal divider on its row, drag up to 6.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Vertical);
        let vp80 = vp(80, 24);
        let divider_row = compute(&t, vp80)[&p(1)].rows;
        let path = divider_at(&t, 40, divider_row, vp80).expect("press should grab the divider");
        assert!(set_ratio_by_path(&mut t, &path, 40, 6, vp80));
        let top = compute(&t, vp80)[&p(1)].rows;
        assert!((top as i32 - 6).abs() <= 2);
    }

    #[test]
    fn press_off_divider_grabs_nothing() {
        // Regression: pressing in the open area of a pane (not on a divider) must
        // grab no divider, so a subsequent drag does not resize. Previously any
        // drag inside a split resized it.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let vp80 = vp(80, 24);
        // Cursor deep inside the left pane (col 10), far from the ~col-40 divider.
        assert!(
            divider_at(&t, 10, 12, vp80).is_none(),
            "an off-divider press must not grab a divider"
        );
    }

    #[test]
    fn drag_tracks_divider_across_repeated_events() {
        // Regression: a real drag sends many motion events. Once grabbed, the
        // divider must keep following the cursor even as it moves well past the
        // divider's current position (the old code re-tested proximity each event
        // and lost the divider after the first move).
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        let vp80 = vp(80, 24);
        let divider = compute(&t, vp80)[&p(1)].cols;
        let path = divider_at(&t, divider, 12, vp80).expect("grab");
        for target in [50u16, 30, 60, 15] {
            assert!(set_ratio_by_path(&mut t, &path, target, 12, vp80));
            let cols = compute(&t, vp80)[&p(1)].cols;
            assert!((cols as i32 - target as i32).abs() <= 2, "divider should follow to {target}, got {cols}");
        }
    }

    #[test]
    fn drag_resizes_inner_horizontal_divider() {
        // Regression: a horizontal (side-by-side) divider nested inside an outer
        // vertical split must be grabbable and draggable. Layout: top pane (1),
        // bottom is a left|right split (2 | 3).
        //   +----------------+
        //   |       1        |
        //   +-------+--------+
        //   |   2   |    3   |
        //   +-------+--------+
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Vertical); // 1 over 2
        t.split_leaf(p(2), p(3), SplitDir::Horizontal); // split 2 into 2|3
        let vp80 = vp(80, 24);
        let inner_divider_col = compute(&t, vp80)[&p(2)].cols; // x of the inner | divider
        let bottom_row = compute(&t, vp80)[&p(2)].y + 1; // a row inside the bottom band
        let before = compute(&t, vp80)[&p(2)].cols;
        let path = divider_at(&t, inner_divider_col, bottom_row, vp80)
            .expect("should grab the inner horizontal-split divider");
        assert!(set_ratio_by_path(&mut t, &path, inner_divider_col + 15, bottom_row, vp80));
        let after = compute(&t, vp80)[&p(2)].cols;
        assert!(after > before, "inner-left pane should grow after dragging the inner divider right");
    }
}
