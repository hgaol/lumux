use super::*;
use crate::grid::Grid;
use crate::model::{PaneId, PaneNode, SplitDir};
use std::collections::BTreeMap;

fn p(n: u32) -> PaneId {
    PaneId(n)
}

fn grid_with(text: &str, w: usize, h: usize) -> Grid {
    let mut g = Grid::new(w, h, 50);
    g.feed(text.as_bytes());
    g
}

#[test]
fn single_pane_fills_content_area() {
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("hello", 20, 5));
    let view = WindowView {
        layout: &layout,
        grids: &grids,
        active_pane: p(1),
    };
    let screen = compose((20, 5), &view, None);
    assert_eq!(screen.row_string(0), "hello");
    // Cursor mapped from pane (after "hello" => col 5).
    assert_eq!(screen.cursor(), Some((5, 0)));
}

#[test]
fn horizontal_split_draws_vertical_border() {
    let mut layout = PaneNode::leaf(p(1));
    layout.split_leaf(p(1), p(2), SplitDir::Horizontal);
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("LEFT", 9, 3));
    grids.insert(p(2), grid_with("RIGHT", 9, 3));
    let view = WindowView {
        layout: &layout,
        grids: &grids,
        active_pane: p(1),
    };
    let screen = compose((20, 3), &view, None);
    let row0 = screen.row_string(0);
    // Both pane contents appear, separated by the │ border glyph.
    assert!(row0.contains("LEFT"));
    assert!(row0.contains("RIGHT"));
    assert!(row0.contains('│'), "expected a vertical border, got {row0:?}");
}

#[test]
fn status_bar_occupies_bottom_row() {
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("body", 20, 4));
    let view = WindowView {
        layout: &layout,
        grids: &grids,
        active_pane: p(1),
    };
    let status = StatusBar {
        left: "[work] 0:sh".into(),
        right: "12:00".into(),
    };
    let screen = compose((20, 5), &view, Some(&status));
    // Bottom row shows status; left text present, right-aligned clock present.
    let bottom = screen.row_string(4);
    assert!(bottom.contains("[work] 0:sh"));
    assert!(bottom.contains("12:00"));
    // Content area is rows 0..4; body is in row 0.
    assert_eq!(screen.row_string(0), "body");
}

#[test]
fn active_pane_cursor_wins() {
    let mut layout = PaneNode::leaf(p(1));
    layout.split_leaf(p(1), p(2), SplitDir::Horizontal);
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("a", 9, 3));
    grids.insert(p(2), grid_with("bb", 9, 3));
    let view = WindowView {
        layout: &layout,
        grids: &grids,
        active_pane: p(2),
    };
    let screen = compose((20, 3), &view, None);
    // 20 cols, divider 1 => 19 usable, ratio 0.5 => round(9.5)=10 left / 9 right.
    // Left pane width 10, border at x=10, right pane starts at x=11; cursor
    // after "bb" => local col 2 => screen col 13.
    assert_eq!(screen.cursor(), Some((13, 0)));
}

#[test]
fn client_renderer_full_then_incremental() {
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("frame one", 20, 3));
    let view = WindowView {
        layout: &layout,
        grids: &grids,
        active_pane: p(1),
    };
    let mut cr = ClientRenderer::new();
    let first = cr.render(compose((20, 3), &view, None));
    // First render is a full repaint.
    assert!(first.starts_with("\x1b[2J"));
    assert!(first.contains("frame one"));

    // Change one pane cell; second render should be incremental (no clear).
    let mut grids2 = BTreeMap::new();
    grids2.insert(p(1), grid_with("frame two", 20, 3));
    let view2 = WindowView {
        layout: &layout,
        grids: &grids2,
        active_pane: p(1),
    };
    let second = cr.render(compose((20, 3), &view2, None));
    assert!(!second.starts_with("\x1b[2J"), "second render must be a diff");
    assert!(second.contains('t')); // "one" -> "two"
}

#[test]
fn renderer_invalidate_forces_repaint() {
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("x", 10, 2));
    let view = WindowView {
        layout: &layout,
        grids: &grids,
        active_pane: p(1),
    };
    let mut cr = ClientRenderer::new();
    let _ = cr.render(compose((10, 2), &view, None));
    cr.invalidate();
    let again = cr.render(compose((10, 2), &view, None));
    assert!(again.starts_with("\x1b[2J"), "invalidate => full repaint");
}
