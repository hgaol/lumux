//! The pane split tree inside a window.
//!
//! A window's panes form a binary tree: each node is either a `Leaf` holding a
//! [`PaneId`], or a `Split` of two child subtrees in a direction. Geometry
//! (turning this into rectangles) is the layout engine's job (Phase 2); here we
//! only model structure and the structural edits: split a pane, and remove a
//! pane (collapsing its parent split so the sibling takes its place).

use super::id::PaneId;

/// Orientation of a split. `Horizontal` stacks panes left|right (a vertical
/// divider); `Vertical` stacks them top/bottom (a horizontal divider). Naming
/// follows tmux's `split-window -h` (left|right) / `-v` (top/bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Panes side by side, divided by a vertical line. `-h`.
    Horizontal,
    /// Panes stacked, divided by a horizontal line. `-v`.
    Vertical,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaneNode {
    Leaf(PaneId),
    Split {
        dir: SplitDir,
        /// Fraction [0,1] of the space given to `first`. Even split = 0.5.
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    pub fn leaf(id: PaneId) -> Self {
        PaneNode::Leaf(id)
    }

    /// In-order list of pane ids (left-to-right, top-to-bottom-ish).
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            PaneNode::Leaf(id) => out.push(*id),
            PaneNode::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    pub fn contains(&self, target: PaneId) -> bool {
        match self {
            PaneNode::Leaf(id) => *id == target,
            PaneNode::Split { first, second, .. } => {
                first.contains(target) || second.contains(target)
            }
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// Replace the leaf `target` with a split of `target` and `new_pane`.
    /// `new_pane` becomes the `second` child (the conventional "new pane is to
    /// the right/below"). Returns true if the target existed.
    pub fn split_leaf(&mut self, target: PaneId, new_pane: PaneId, dir: SplitDir) -> bool {
        match self {
            PaneNode::Leaf(id) if *id == target => {
                *self = PaneNode::Split {
                    dir,
                    ratio: 0.5,
                    first: Box::new(PaneNode::Leaf(target)),
                    second: Box::new(PaneNode::Leaf(new_pane)),
                };
                true
            }
            PaneNode::Leaf(_) => false,
            PaneNode::Split { first, second, .. } => {
                first.split_leaf(target, new_pane, dir) || second.split_leaf(target, new_pane, dir)
            }
        }
    }

    /// Nudge the divider of the nearest split (on the matching axis) that encloses
    /// `target` by `step`. `step` is signed in *ratio* space: a positive step moves
    /// the divider toward the second child (right for a horizontal split, down for a
    /// vertical one), a negative step moves it the other way — matching tmux's
    /// resize-pane -R/-D (positive) and -L/-U (negative). Returns true if a divider
    /// was adjusted.
    ///
    /// Walks from the leaf upward (deepest matching split wins) so resizing acts on
    /// the divider closest to the active pane, matching tmux.
    pub fn resize_pane(&mut self, target: PaneId, axis: SplitDir, step: f32) -> bool {
        match self {
            PaneNode::Leaf(_) => false,
            PaneNode::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                // Try the children first (deepest split closest to the pane wins).
                if first.resize_pane(target, axis, step) || second.resize_pane(target, axis, step) {
                    return true;
                }
                // No deeper split on this axis handled it; if this split is on the
                // requested axis and contains the target, move its divider here.
                if *dir == axis && (first.contains(target) || second.contains(target)) {
                    *ratio = (*ratio + step).clamp(0.05, 0.95);
                    return true;
                }
                false
            }
        }
    }

    /// Remove pane `target`, collapsing its parent split so the sibling takes
    /// the split's place. Returns:
    /// - `Removed::Gone` if the whole subtree was just that leaf (caller must
    ///   handle window emptiness),
    /// - `Removed::Collapsed` if a split collapsed into its sibling,
    /// - `Removed::NotFound` otherwise.
    pub fn remove_pane(&mut self, target: PaneId) -> Removed {
        // If self is the split whose direct child is the target leaf, collapse.
        if let PaneNode::Split { first, second, .. } = self {
            match (first.as_ref(), second.as_ref()) {
                (PaneNode::Leaf(a), _) if *a == target => {
                    *self = (**second).clone();
                    return Removed::Collapsed;
                }
                (_, PaneNode::Leaf(b)) if *b == target => {
                    *self = (**first).clone();
                    return Removed::Collapsed;
                }
                _ => {}
            }
        }
        match self {
            PaneNode::Leaf(id) if *id == target => Removed::Gone,
            PaneNode::Leaf(_) => Removed::NotFound,
            PaneNode::Split { first, second, .. } => match first.remove_pane(target) {
                Removed::NotFound => second.remove_pane(target),
                other => other,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    Gone,
    Collapsed,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u32) -> PaneId {
        PaneId(n)
    }

    #[test]
    fn single_leaf_basics() {
        let t = PaneNode::leaf(p(1));
        assert_eq!(t.pane_ids(), vec![p(1)]);
        assert_eq!(t.pane_count(), 1);
        assert!(t.contains(p(1)));
        assert!(!t.contains(p(2)));
    }

    #[test]
    fn split_replaces_leaf_with_split() {
        let mut t = PaneNode::leaf(p(1));
        assert!(t.split_leaf(p(1), p(2), SplitDir::Horizontal));
        assert_eq!(t.pane_ids(), vec![p(1), p(2)]);
        assert_eq!(t.pane_count(), 2);
    }

    #[test]
    fn split_nonexistent_leaf_is_noop() {
        let mut t = PaneNode::leaf(p(1));
        assert!(!t.split_leaf(p(99), p(2), SplitDir::Vertical));
        assert_eq!(t.pane_ids(), vec![p(1)]);
    }

    #[test]
    fn nested_splits_traverse_in_order() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal); // [1|2]
        t.split_leaf(p(2), p(3), SplitDir::Vertical); // [1 | [2/3]]
        assert_eq!(t.pane_ids(), vec![p(1), p(2), p(3)]);
    }

    #[test]
    fn remove_collapses_sibling_into_parent() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        assert_eq!(t.remove_pane(p(1)), Removed::Collapsed);
        assert_eq!(t, PaneNode::Leaf(p(2)));
    }

    #[test]
    fn remove_deep_pane_collapses_locally() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal); // [1|2]
        t.split_leaf(p(2), p(3), SplitDir::Vertical); // [1 | [2/3]]
        assert_eq!(t.remove_pane(p(3)), Removed::Collapsed);
        assert_eq!(t.pane_ids(), vec![p(1), p(2)]);
    }

    #[test]
    fn remove_last_pane_reports_gone() {
        let mut t = PaneNode::leaf(p(1));
        assert_eq!(t.remove_pane(p(1)), Removed::Gone);
    }

    #[test]
    fn remove_missing_pane_not_found() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        assert_eq!(t.remove_pane(p(42)), Removed::NotFound);
        assert_eq!(t.pane_ids(), vec![p(1), p(2)]);
    }

    fn ratio_of(node: &PaneNode) -> f32 {
        match node {
            PaneNode::Split { ratio, .. } => *ratio,
            _ => panic!("not a split"),
        }
    }

    #[test]
    fn resize_moves_divider_on_matching_axis() {
        // [1 | 2] horizontal split at 0.5. Positive step moves divider right.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        assert!(t.resize_pane(p(1), SplitDir::Horizontal, 0.1));
        assert!((ratio_of(&t) - 0.6).abs() < 1e-6);
        // From the other pane, the same direction moves the same divider.
        assert!(t.resize_pane(p(2), SplitDir::Horizontal, -0.2));
        assert!((ratio_of(&t) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn resize_ignores_wrong_axis() {
        // A horizontal split can't be resized vertically.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        assert!(!t.resize_pane(p(1), SplitDir::Vertical, 0.1));
        assert!((ratio_of(&t) - 0.5).abs() < 1e-6, "ratio unchanged");
    }

    #[test]
    fn resize_clamps_to_bounds() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        // Push far past 1.0; must clamp at 0.95.
        for _ in 0..50 {
            t.resize_pane(p(1), SplitDir::Horizontal, 0.1);
        }
        assert!((ratio_of(&t) - 0.95).abs() < 1e-6);
    }

    #[test]
    fn resize_targets_deepest_split_near_pane() {
        // [1 | [2 / 3]] — resizing pane 2 vertically hits the inner 2/3 divider,
        // not the outer horizontal one.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal); // [1|2]
        t.split_leaf(p(2), p(3), SplitDir::Vertical); // [1 | [2/3]]
        assert!(t.resize_pane(p(2), SplitDir::Vertical, 0.1));
        // The outer split's ratio is untouched (still 0.5).
        if let PaneNode::Split { ratio, second, .. } = &t {
            assert!((ratio - 0.5).abs() < 1e-6, "outer ratio unchanged");
            assert!((ratio_of(second) - 0.6).abs() < 1e-6, "inner divider moved");
        } else {
            panic!("expected a split");
        }
    }
}
