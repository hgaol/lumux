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

/// Build a borrowed-grid map (the shape WindowView expects) from an owned one.
fn refs(owned: &BTreeMap<PaneId, Grid>) -> BTreeMap<PaneId, &Grid> {
    owned.iter().map(|(k, v)| (*k, v)).collect()
}

#[test]
fn single_pane_fills_content_area() {
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("hello", 20, 5));
    let view = WindowView {
        layout: &layout,
        grids: &refs(&grids),
        active_pane: p(1),
    };
    let screen = compose((20, 5), &view, None, false);
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
        grids: &refs(&grids),
        active_pane: p(1),
    };
    let screen = compose((20, 3), &view, None, false);
    let row0 = screen.row_string(0);
    // Both pane contents appear, separated by the │ border glyph.
    assert!(row0.contains("LEFT"));
    assert!(row0.contains("RIGHT"));
    assert!(
        row0.contains('│'),
        "expected a vertical border, got {row0:?}"
    );
}

#[test]
fn status_bar_occupies_bottom_row() {
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("body", 20, 4));
    let view = WindowView {
        layout: &layout,
        grids: &refs(&grids),
        active_pane: p(1),
    };
    let status = StatusBar {
        left: "[work] 0:sh".into(),
        right: "12:00".into(),
    };
    let screen = compose((20, 5), &view, Some(&status), false);
    // Bottom row shows status; left text present, right-aligned clock present.
    let bottom = screen.row_string(4);
    assert!(bottom.contains("[work] 0:sh"));
    assert!(bottom.contains("12:00"));
    // Content area is rows 0..4; body is in row 0.
    assert_eq!(screen.row_string(0), "body");
}

#[test]
fn reserved_status_row_keeps_panes_out_of_bottom_line() {
    // Regression: the daemon composes with status=None but reserve_status_row=true
    // (it paints its own styled bar). The pane must be laid out into rows 0..h-1
    // so its content never lands on the bottom row that the status bar will use —
    // otherwise the last line of pane output overlaps the status bar.
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    // A grid as tall as the FULL screen (5 rows), every row non-blank.
    let mut g = grid_with("row0", 20, 5);
    g.feed(b"\r\n");
    // Fill several rows so content would reach the bottom if not constrained.
    g.feed(b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
    grids.insert(p(1), g);
    let view = WindowView {
        layout: &layout,
        grids: &refs(&grids),
        active_pane: p(1),
    };
    let screen = compose((20, 5), &view, None, true);
    // The bottom row (index 4) must be blank — reserved for the status bar.
    assert_eq!(
        screen.row_string(4),
        "",
        "bottom row must be reserved (blank), not pane content"
    );
    // And the cursor must never be parked on the reserved row.
    if let Some((_, cy)) = screen.cursor() {
        assert!(cy < 4, "cursor must stay above the reserved status row");
    }
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
        grids: &refs(&grids),
        active_pane: p(2),
    };
    let screen = compose((20, 3), &view, None, false);
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
        grids: &refs(&grids),
        active_pane: p(1),
    };
    let mut cr = ClientRenderer::new();
    let first = cr.render(compose((20, 3), &view, None, false));
    // First render is a full repaint.
    assert!(first.starts_with("\x1b[2J"));
    assert!(first.contains("frame one"));

    // Change one pane cell; second render should be incremental (no clear).
    let mut grids2 = BTreeMap::new();
    grids2.insert(p(1), grid_with("frame two", 20, 3));
    let view2 = WindowView {
        layout: &layout,
        grids: &refs(&grids2),
        active_pane: p(1),
    };
    let second = cr.render(compose((20, 3), &view2, None, false));
    assert!(
        !second.starts_with("\x1b[2J"),
        "second render must be a diff"
    );
    assert!(second.contains('t')); // "one" -> "two"
}

#[test]
fn renderer_invalidate_forces_repaint() {
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("x", 10, 2));
    let view = WindowView {
        layout: &layout,
        grids: &refs(&grids),
        active_pane: p(1),
    };
    let mut cr = ClientRenderer::new();
    let _ = cr.render(compose((10, 2), &view, None, false));
    cr.invalidate();
    let again = cr.render(compose((10, 2), &view, None, false));
    assert!(again.starts_with("\x1b[2J"), "invalidate => full repaint");
}

#[test]
fn repaint_roundtrip_preserves_text_with_inline_sgr() {
    // PSReadLine emits text with mid-line SGR color changes and cursor moves.
    // Reproduce a line like "lm new -s aaa" where color toggles mid-word, render
    // the grid, full_repaint it to VT, replay that VT into a fresh grid, and
    // confirm the text is identical — i.e. the diff/repaint doesn't shift columns.
    use crate::grid::Grid;
    use crate::render::{compose, full_repaint, WindowView};
    use crate::model::{PaneId, PaneNode};
    use std::collections::BTreeMap;

    // Source grid: type "lm new -s aaa" with an SGR color flip before "new"
    // (like PSReadLine highlighting). 80x4.
    let mut src = Grid::new(80, 4, 100);
    src.feed(b"PS> \x1b[93mlm\x1b[m new -s aaa");

    // Compose into a screen (single pane, no status reserve for simplicity).
    let layout = PaneNode::leaf(PaneId(1));
    let mut grids: BTreeMap<PaneId, &Grid> = BTreeMap::new();
    grids.insert(PaneId(1), &src);
    let view = WindowView { layout: &layout, grids: &grids, active_pane: PaneId(1) };
    let screen = compose((80, 4), &view, None, false);

    // The composed screen's row 0 must read the literal text (no shift).
    assert_eq!(
        screen.row_string(0),
        "PS> lm new -s aaa",
        "composed row must preserve exact columns"
    );

    // Now full_repaint to VT, then replay that VT into a fresh 80x4 grid and check
    // the text survives the round-trip unshifted.
    let vt = full_repaint(&screen);
    let mut replay = Grid::new(80, 4, 100);
    replay.feed(vt.as_bytes());
    assert_eq!(
        replay.screen_text()[0], "PS> lm new -s aaa",
        "repaint VT must reproduce the same text without column shift; got {:?}",
        replay.screen_text()[0]
    );
}

#[test]
fn repaint_roundtrip_with_unicode_arrow_header() {
    // PSReadLine's ListView header is like "←/5>" with the rest of the row padded
    // and "<History(5)>" right-aligned. The "←" (U+2190) is a 1-wide non-ASCII
    // glyph; if cell-width accounting treats it as 2 (or the repaint emits a
    // spacer), every following column shifts right — exactly the screenshot bug
    // (lm -> lmm, stray leading chars).
    use crate::grid::Grid;
    use crate::render::full_repaint;
    use crate::render::Screen;

    // Build the line directly into a screen, then repaint + replay.
    let mut screen = Screen::new(40, 2);
    screen.write_plain(0, 0, "\u{2190}/5> abc");
    // Sanity: the composed row reads the literal text.
    assert_eq!(screen.row_string(0), "\u{2190}/5> abc");

    let vt = full_repaint(&screen);
    let mut replay = Grid::new(40, 2, 100);
    replay.feed(vt.as_bytes());
    assert_eq!(
        replay.screen_text()[0],
        "\u{2190}/5> abc",
        "arrow-header row must survive repaint without column shift; got {:?}",
        replay.screen_text()[0]
    );
}

#[test]
fn diff_wide_char_does_not_shift_columns() {
    // A CJK wide char occupies 2 grid columns (glyph + blanked spacer). The diff
    // must emit the glyph once (it advances the terminal 2 cols) and NOT also
    // emit the blank spacer, or everything after shifts right — the PSReadLine
    // ListView corruption. Assert the emitted VT contains "中x" with no spacer
    // space between them.
    use crate::grid::Grid;
    use crate::render::full_repaint;
    use crate::render::Screen;
    let mut g = Grid::new(10, 1, 10);
    g.feed("中x".as_bytes());
    let mut screen = Screen::new(10, 1);
    let rows: Vec<&[_]> = g.rows().iter().map(|r| r.cells()).collect();
    screen.blit_cells(0, 0, 10, 1, &rows);
    let vt = full_repaint(&screen);
    assert!(
        vt.contains("中x"),
        "wide glyph must be emitted without a trailing spacer space; got VT {vt:?}"
    );
    assert!(
        !vt.contains("中 x"),
        "the blank spacer after a wide glyph must NOT be emitted; got VT {vt:?}"
    );
}
