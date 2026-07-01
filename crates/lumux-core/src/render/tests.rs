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
        active_border: None,
        inactive_border: None,
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
        active_border: None,
        inactive_border: None,
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
        active_border: None,
        inactive_border: None,
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
        active_border: None,
        inactive_border: None,
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
        active_border: None,
        inactive_border: None,
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
        active_border: None,
        inactive_border: None,
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
        active_border: None,
        inactive_border: None,
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
        active_border: None,
        inactive_border: None,
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
    let view = WindowView { layout: &layout, grids: &grids, active_pane: PaneId(1), active_border: None, inactive_border: None };
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

/// Build a one-span segment of plain text for status-bar layout tests.
fn span(text: &str) -> Vec<crate::status::Span> {
    vec![crate::status::Span {
        text: text.to_string(),
        attrs: termwiz::cell::CellAttributes::default(),
    }]
}

/// The status row must occupy exactly one line and never overflow the width,
/// at every terminal width — including widths far too small to fit all three
/// segments. This is the regression guard for the clock wrapping to a second
/// line on resize.
#[test]
fn styled_status_is_always_one_line_at_any_width() {
    let base = StyledStatus::base_attrs("colour236", "white");
    let make = || StyledStatus {
        left: span(" work "),
        centre: span("1:zsh 2:vim* 3:top"),
        right: span(" 07:20 18-Jun "),
        base: base.clone(),
        justify: Justify::Centre,
    };
    // Sweep from comfortably wide down to absurdly narrow.
    for w in (1..=80).rev() {
        let h = 2;
        let mut screen = Screen::new(w, h);
        make().render(&mut screen);
        // Row above the status row must be untouched (all blank) — i.e. nothing
        // wrapped upward.
        let above = screen.row_string(0);
        assert!(
            above.trim().is_empty(),
            "width {w}: status content leaked onto the row above: {above:?}"
        );
        // The status row itself must contain no cell past column w-1. row_string
        // returns exactly the row's cells; assert its display width never exceeds
        // w (chars().count(), since the test text is all single-width).
        let row = screen.row_string(h - 1);
        assert!(
            row.chars().count() <= w,
            "width {w}: status row is wider than the terminal: {} > {w} ({row:?})",
            row.chars().count()
        );
    }
}

/// Segments must not overwrite each other: when everything fits, left, centre,
/// and right are all present and in order; when the centre can't fit it is
/// dropped rather than colliding with its neighbours.
#[test]
fn styled_status_segments_do_not_overlap() {
    let base = StyledStatus::base_attrs("colour236", "white");
    let s = StyledStatus {
        left: span("L"),
        centre: span("CC"),
        right: span("R"),
        base: base.clone(),
        justify: Justify::Centre,
    };
    // Wide enough for all three.
    let mut screen = Screen::new(20, 1);
    s.render(&mut screen);
    let row = screen.row_string(0);
    assert!(row.starts_with('L'), "left segment at column 0: {row:?}");
    assert!(row.contains("CC"), "centre present: {row:?}");
    assert!(row.ends_with('R'), "right segment right-aligned: {row:?}");

    // Too narrow for the centre: left + right only, no overlap, still one line.
    let mut tiny = Screen::new(2, 1);
    s.render(&mut tiny);
    let row = tiny.row_string(0);
    assert_eq!(row.chars().count(), 2, "row exactly fills width 2: {row:?}");
    assert!(!row.contains("CC"), "centre dropped when it can't fit: {row:?}");
}

/// `centre_start` (used for click hit-testing) must equal the column where
/// `render` actually paints the centre, at every width — otherwise mouse clicks
/// on the window list land on the wrong window.
#[test]
fn centre_start_matches_rendered_centre() {
    let base = StyledStatus::base_attrs("colour236", "white");
    let s = StyledStatus {
        left: span("[work] "),
        centre: span("1:a 2:b"),
        right: span(" 12:00 "),
        base,
        justify: Justify::Centre,
    };
    for w in 1..=60 {
        let mut screen = Screen::new(w, 1);
        s.render(&mut screen);
        let cx = s.centre_start(w);
        // centre_start must be within the row.
        assert!(cx <= w, "width {w}: centre_start {cx} exceeds width");
        // If the centre was drawn, its first char must appear at cx.
        let row = screen.row_string(0);
        let chars: Vec<char> = row.chars().collect();
        if cx < w && cx < chars.len() && !s.centre.is_empty() {
            let first = s.centre[0].text.chars().next().unwrap();
            // Only assert when there was room for the centre (gap non-empty).
            let left_w = s.left[0].text.chars().count().min(w);
            let right_w = s.right[0].text.chars().count();
            let right_start = w.saturating_sub(right_w).max(left_w);
            if right_start > left_w {
                assert_eq!(
                    chars[cx], first,
                    "width {w}: centre_start {cx} doesn't point at the centre's first char in {row:?}"
                );
            }
        }
    }
}

#[test]
fn blit_grid_scrolled_keeps_wide_char_columns() {
    // Regression: the copy-mode scrolled overpaint used to re-derive a string and
    // write it char-by-char, so a wide (CJK) glyph drifted every following column.
    // blit_grid_scrolled copies real cells, so columns stay aligned and the wide
    // glyph occupies two columns with the ASCII after it at the right place.
    let mut g = Grid::new(20, 3, 50);
    // A wide char (中, 2 cols) then "AB". Push it into history with newlines so
    // it's reachable via the combined buffer at row 0.
    g.feed("中AB\r\nx\r\ny".as_bytes());

    let mut screen = Screen::new(20, 3);
    // Blit the combined buffer from the top (row 0 holds "中AB").
    screen.blit_grid_scrolled(0, 0, 20, 3, &g, 0);

    let row0 = screen.row_string(0);
    // The wide glyph occupies two columns (中 + a spacer rendered as a space),
    // with "AB" right after — no dropped or shifted columns.
    assert!(row0.starts_with("中 AB"), "wide char + following ASCII must stay aligned; got {row0:?}");
    // Column 0 is the wide glyph; column 1 is its spacer; 'A' sits at column 2.
    assert_eq!(screen.cell(0, 0).map(|c| c.str()), Some("中"));
    assert_eq!(screen.cell(2, 0).map(|c| c.str()), Some("A"));
    assert_eq!(screen.cell(3, 0).map(|c| c.str()), Some("B"));
}

#[test]
fn blit_grid_scrolled_preserves_color() {
    // Regression: copy-mode scroll used to re-derive a plain string and write it
    // with default attributes, so colored history rendered as white-on-black.
    // blit_grid_scrolled copies real cells, so colors (and bold etc.) survive —
    // matching tmux, whose copy-mode keeps the original colors.
    use termwiz::color::ColorAttribute;
    let mut g = Grid::new(20, 3, 50);
    // Red "RED" then default "ok", pushed into history.
    g.feed(b"\x1b[31mRED\x1b[m ok\r\nx\r\ny");

    let mut screen = Screen::new(20, 3);
    screen.blit_grid_scrolled(0, 0, 20, 3, &g, 0);

    // The "RED" cells must keep their red foreground (palette index 1), not the
    // default attribute the old char-by-char copy produced.
    let red_cell = screen.cell(0, 0).expect("cell 0,0");
    assert_eq!(red_cell.str(), "R");
    assert_eq!(
        red_cell.attrs().foreground(),
        ColorAttribute::PaletteIndex(1),
        "scrolled copy-mode must preserve cell colors; got {:?}",
        red_cell.attrs().foreground()
    );
    // And a default cell after the reset stays default.
    let ok_cell = screen.cell(4, 0).expect("cell 4,0");
    assert_eq!(ok_cell.str(), "o");
    assert_eq!(ok_cell.attrs().foreground(), ColorAttribute::Default);
}

#[test]
fn active_pane_border_is_highlighted() {
    use termwiz::color::ColorAttribute;
    // [LEFT | RIGHT] split in 20x3, active = LEFT (p1). The shared divider is the
    // right edge of the active pane, so it must carry the highlight color; the
    // active pane's own content is unaffected.
    let mut layout = PaneNode::leaf(p(1));
    layout.split_leaf(p(1), p(2), SplitDir::Horizontal);
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("L", 9, 3));
    grids.insert(p(2), grid_with("R", 9, 3));
    let green = border_attrs("green");
    assert!(green.is_some());
    let view = WindowView {
        layout: &layout,
        grids: &refs(&grids),
        active_pane: p(1),
        active_border: green,
        inactive_border: None,
    };
    let screen = compose((20, 3), &view, None, false);

    // Find the divider column (the │ in row 0) and check it's green.
    let row0 = screen.row_string(0);
    let div_col = row0.find('│').expect("a divider should be drawn");
    let div_cell = screen.cell(div_col, 0).expect("divider cell");
    assert_eq!(div_cell.str(), "│");
    assert_eq!(
        div_cell.attrs().foreground(),
        ColorAttribute::PaletteIndex(2), // green
        "active pane's border must be highlighted green"
    );
}

#[test]
fn inactive_pane_border_is_not_highlighted() {
    use termwiz::color::ColorAttribute;
    // Same split but active = RIGHT (p2). Now the LEFT pane's own right-edge
    // divider (which is also the active pane's left edge) is highlighted, but a
    // 3-pane check is cleaner: with no active_border set, NO divider is colored.
    let mut layout = PaneNode::leaf(p(1));
    layout.split_leaf(p(1), p(2), SplitDir::Horizontal);
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("L", 9, 3));
    grids.insert(p(2), grid_with("R", 9, 3));
    let view = WindowView {
        layout: &layout,
        grids: &refs(&grids),
        active_pane: p(1),
        active_border: None, // highlight disabled
        inactive_border: None,
    };
    let screen = compose((20, 3), &view, None, false);
    let row0 = screen.row_string(0);
    let div_col = row0.find('│').expect("a divider should be drawn");
    let div_cell = screen.cell(div_col, 0).expect("divider cell");
    assert_eq!(
        div_cell.attrs().foreground(),
        ColorAttribute::Default,
        "with no active_border, dividers stay default-colored"
    );
}

#[test]
fn blit_window_layout_draws_all_panes_and_dividers() {
    // A [LEFT | RIGHT] split blitted into a 20x5 sub-region at origin (0,0) must
    // show BOTH panes' content and a vertical divider between them — proving the
    // chooser preview renders the whole layout, not just one pane.
    let mut layout = PaneNode::leaf(p(1));
    layout.split_leaf(p(1), p(2), SplitDir::Horizontal);
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("LEFT", 9, 5));
    grids.insert(p(2), grid_with("RIGHT", 9, 5));

    let mut screen = Screen::new(20, 5);
    blit_window_layout(&mut screen, 0, 0, 20, 5, &layout, &refs(&grids));

    let row0 = screen.row_string(0);
    assert!(row0.contains("LEFT"), "left pane content should render; got {row0:?}");
    assert!(row0.contains("RIGHT"), "right pane content should render; got {row0:?}");
    assert!(row0.contains('\u{2502}'), "a vertical divider should separate the panes; got {row0:?}");
}

#[test]
fn blit_window_layout_offsets_into_subregion() {
    // Blit a single pane into a sub-region that does NOT start at the origin;
    // content must land at the offset, and the area above/left stays blank.
    let layout = PaneNode::leaf(p(1));
    let mut grids = BTreeMap::new();
    grids.insert(p(1), grid_with("HI", 10, 3));

    let mut screen = Screen::new(20, 6);
    blit_window_layout(&mut screen, 5, 2, 10, 3, &layout, &refs(&grids));

    // Row 0 (above the sub-region) is blank; the content sits at row 2, col 5.
    assert_eq!(screen.row_string(0).trim_end(), "");
    assert_eq!(screen.cell(5, 2).map(|c| c.str().to_string()), Some("H".to_string()));
    assert_eq!(screen.cell(6, 2).map(|c| c.str().to_string()), Some("I".to_string()));
}
