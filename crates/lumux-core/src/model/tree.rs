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

    /// Swap the positions of leaves `a` and `b` in the tree (tmux swap-pane).
    /// Pane ids are kept (so each pane's grid follows it); only their slots in
    /// the layout exchange. Returns true only if BOTH leaves were found.
    pub fn swap_ids(&mut self, a: PaneId, b: PaneId) -> bool {
        if a == b {
            return false;
        }
        // Two passes: a swap needs both present. Check first, then mutate.
        if !(self.contains(a) && self.contains(b)) {
            return false;
        }
        self.relabel(a, b);
        true
    }

    /// Replace every leaf equal to `a` with `b` and vice-versa. Caller guarantees
    /// both exist (each appears exactly once, since pane ids are unique).
    fn relabel(&mut self, a: PaneId, b: PaneId) {
        match self {
            PaneNode::Leaf(id) => {
                if *id == a {
                    *id = b;
                } else if *id == b {
                    *id = a;
                }
            }
            PaneNode::Split { first, second, .. } => {
                first.relabel(a, b);
                second.relabel(a, b);
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

/// A tmux preset layout. `next` cycles through them in tmux's order; the daemon
/// rebuilds the active window's pane tree to the chosen arrangement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    /// All panes in a single row, left to right.
    EvenHorizontal,
    /// All panes in a single column, top to bottom.
    EvenVertical,
    /// First pane fills the left half; the rest stack in the right half.
    MainVertical,
    /// First pane fills the top half; the rest sit in a row below.
    MainHorizontal,
    /// Roughly-square grid.
    Tiled,
}

impl LayoutKind {
    /// The cycle order for tmux's `next-layout` (prefix Space).
    pub const CYCLE: [LayoutKind; 5] = [
        LayoutKind::EvenHorizontal,
        LayoutKind::EvenVertical,
        LayoutKind::MainVertical,
        LayoutKind::MainHorizontal,
        LayoutKind::Tiled,
    ];

    /// The next layout in the cycle.
    pub fn next(self) -> LayoutKind {
        let pos = Self::CYCLE.iter().position(|&l| l == self).unwrap_or(0);
        Self::CYCLE[(pos + 1) % Self::CYCLE.len()]
    }
}

impl PaneNode {
    /// Rebuild this tree as `kind` over the given panes (in order), with even
    /// ratios. Pane ids are preserved — only the arrangement changes. `panes`
    /// must be non-empty (callers pass the window's existing `pane_ids`).
    pub fn arrange(kind: LayoutKind, panes: &[PaneId]) -> PaneNode {
        debug_assert!(!panes.is_empty());
        match kind {
            LayoutKind::EvenHorizontal => chain(panes, SplitDir::Horizontal),
            LayoutKind::EvenVertical => chain(panes, SplitDir::Vertical),
            LayoutKind::MainVertical => main_and_stack(panes, SplitDir::Horizontal),
            LayoutKind::MainHorizontal => main_and_stack(panes, SplitDir::Vertical),
            LayoutKind::Tiled => tiled(panes),
        }
    }
}

/// A balanced-ish chain of single-direction splits over all panes, even ratios.
/// e.g. [1,2,3] horizontal -> [1 | [2 | 3]] with ratios 1/3, 1/2.
fn chain(panes: &[PaneId], dir: SplitDir) -> PaneNode {
    let n = panes.len();
    if n == 1 {
        return PaneNode::Leaf(panes[0]);
    }
    // First pane gets 1/n of the space; the remainder holds the rest.
    PaneNode::Split {
        dir,
        ratio: 1.0 / n as f32,
        first: Box::new(PaneNode::Leaf(panes[0])),
        second: Box::new(chain(&panes[1..], dir)),
    }
}

/// "main + stack": the first pane fills half along `dir`; the rest fill the
/// other half, stacked on the perpendicular axis. tmux main-vertical uses a
/// horizontal outer split (main on the left); main-horizontal a vertical one.
fn main_and_stack(panes: &[PaneId], dir: SplitDir) -> PaneNode {
    if panes.len() == 1 {
        return PaneNode::Leaf(panes[0]);
    }
    let stack_dir = match dir {
        SplitDir::Horizontal => SplitDir::Vertical, // main left, others stacked vertically
        SplitDir::Vertical => SplitDir::Horizontal, // main top, others in a row
    };
    PaneNode::Split {
        dir,
        ratio: 0.5,
        first: Box::new(PaneNode::Leaf(panes[0])),
        second: Box::new(chain(&panes[1..], stack_dir)),
    }
}

/// Roughly-square grid: split into `rows` rows (ceil(sqrt(n))), each a row of
/// panes. Built as an outer vertical chain of rows, each an inner horizontal
/// chain of its panes.
fn tiled(panes: &[PaneId]) -> PaneNode {
    let n = panes.len();
    if n == 1 {
        return PaneNode::Leaf(panes[0]);
    }
    // rows = ceil(sqrt(n)); distribute panes across rows as evenly as possible.
    let rows = (n as f64).sqrt().ceil() as usize;
    let per_row = n.div_ceil(rows); // ceil(n/rows) panes per row (front-loaded)
    let row_nodes: Vec<PaneNode> = panes
        .chunks(per_row)
        .map(|chunk| chain(chunk, SplitDir::Horizontal))
        .collect();
    // Stack the rows vertically with even ratios.
    stack_even(&row_nodes, SplitDir::Vertical)
}

/// Combine already-built nodes into an even single-direction chain.
fn stack_even(nodes: &[PaneNode], dir: SplitDir) -> PaneNode {
    let n = nodes.len();
    if n == 1 {
        return nodes[0].clone();
    }
    PaneNode::Split {
        dir,
        ratio: 1.0 / n as f32,
        first: Box::new(nodes[0].clone()),
        second: Box::new(stack_even(&nodes[1..], dir)),
    }
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

    #[test]
    fn swap_ids_exchanges_two_leaves() {
        // [1 | [2 / 3]] — swapping 1 and 3 exchanges their slots, keeping ids.
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal); // [1|2]
        t.split_leaf(p(2), p(3), SplitDir::Vertical); // [1 | [2/3]]
        assert!(t.swap_ids(p(1), p(3)));
        // Traversal order was [1,2,3]; after swapping 1<->3 it's [3,2,1].
        assert_eq!(t.pane_ids(), vec![p(3), p(2), p(1)]);
    }

    #[test]
    fn swap_ids_requires_both_present_and_distinct() {
        let mut t = PaneNode::leaf(p(1));
        t.split_leaf(p(1), p(2), SplitDir::Horizontal);
        // Self-swap is a no-op false.
        assert!(!t.swap_ids(p(1), p(1)));
        // Missing partner: nothing changes.
        assert!(!t.swap_ids(p(1), p(99)));
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

    // ----- preset layouts -----

    fn ids(n: u32) -> Vec<PaneId> {
        (1..=n).map(p).collect()
    }

    #[test]
    fn arrange_preserves_all_panes_in_order() {
        // Every layout must keep exactly the same panes, in the same order.
        let panes = ids(5);
        for kind in LayoutKind::CYCLE {
            let tree = PaneNode::arrange(kind, &panes);
            assert_eq!(
                tree.pane_ids(),
                panes,
                "layout {kind:?} must preserve panes in order"
            );
            assert_eq!(tree.pane_count(), 5);
        }
    }

    #[test]
    fn arrange_single_pane_is_a_leaf() {
        for kind in LayoutKind::CYCLE {
            assert_eq!(PaneNode::arrange(kind, &[p(1)]), PaneNode::Leaf(p(1)));
        }
    }

    #[test]
    fn even_horizontal_is_all_horizontal_splits() {
        // Every split in even-horizontal is Horizontal (side by side).
        let tree = PaneNode::arrange(LayoutKind::EvenHorizontal, &ids(4));
        assert!(all_splits_are(&tree, SplitDir::Horizontal));
    }

    #[test]
    fn even_vertical_is_all_vertical_splits() {
        let tree = PaneNode::arrange(LayoutKind::EvenVertical, &ids(4));
        assert!(all_splits_are(&tree, SplitDir::Vertical));
    }

    #[test]
    fn main_vertical_main_pane_fills_left_half() {
        // Outer split is Horizontal at 0.5 with pane 1 on the left.
        let tree = PaneNode::arrange(LayoutKind::MainVertical, &ids(3));
        match &tree {
            PaneNode::Split { dir, ratio, first, .. } => {
                assert_eq!(*dir, SplitDir::Horizontal);
                assert!((ratio - 0.5).abs() < 1e-6);
                assert_eq!(**first, PaneNode::Leaf(p(1)));
            }
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn tiled_uses_both_axes_for_a_grid() {
        // A 4-pane tiled layout should be a 2x2 grid: an outer vertical split of
        // two rows, each an inner horizontal split.
        let tree = PaneNode::arrange(LayoutKind::Tiled, &ids(4));
        match &tree {
            PaneNode::Split { dir, first, second, .. } => {
                assert_eq!(*dir, SplitDir::Vertical, "rows stacked vertically");
                assert!(matches!(**first, PaneNode::Split { dir: SplitDir::Horizontal, .. }));
                assert!(matches!(**second, PaneNode::Split { dir: SplitDir::Horizontal, .. }));
            }
            _ => panic!("expected a split"),
        }
        assert_eq!(tree.pane_ids(), ids(4));
    }

    #[test]
    fn layout_kind_cycles_through_all_five() {
        let mut k = LayoutKind::CYCLE[0];
        let mut seen = vec![k];
        for _ in 0..4 {
            k = k.next();
            seen.push(k);
        }
        // Five distinct layouts, then wraps back to the first.
        assert_eq!(seen.len(), 5);
        assert_eq!(k.next(), LayoutKind::CYCLE[0]);
    }

    /// True if every Split node in the tree has direction `dir`.
    fn all_splits_are(node: &PaneNode, dir: SplitDir) -> bool {
        match node {
            PaneNode::Leaf(_) => true,
            PaneNode::Split { dir: d, first, second, .. } => {
                *d == dir && all_splits_are(first, dir) && all_splits_are(second, dir)
            }
        }
    }
}
