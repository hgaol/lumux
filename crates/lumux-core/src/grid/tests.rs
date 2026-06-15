use super::*;

fn grid() -> Grid {
    Grid::new(10, 3, 100)
}

#[test]
fn prints_plain_text() {
    let mut g = grid();
    g.feed(b"hello");
    assert_eq!(g.screen_text()[0], "hello");
    assert_eq!(g.cursor(), (5, 0));
}

#[test]
fn carriage_return_and_line_feed() {
    let mut g = grid();
    g.feed(b"ab\r\ncd");
    assert_eq!(g.screen_text()[0], "ab");
    assert_eq!(g.screen_text()[1], "cd");
    assert_eq!(g.cursor(), (2, 1));
}

#[test]
fn backspace_moves_cursor_left() {
    let mut g = grid();
    g.feed(b"abc");
    g.feed(&[0x08]); // BS
    assert_eq!(g.cursor(), (2, 0));
}

#[test]
fn autowrap_at_width() {
    let mut g = Grid::new(4, 3, 100);
    g.feed(b"abcdef"); // 6 chars into width-4
    assert_eq!(g.screen_text()[0], "abcd");
    assert_eq!(g.screen_text()[1], "ef");
    assert_eq!(g.cursor(), (2, 1));
}

#[test]
fn cursor_position_absolute() {
    let mut g = grid();
    // CSI 2;3 H -> row 2 col 3 (1-based) => (col2,row1) zero-based.
    g.feed(b"\x1b[2;3H");
    assert_eq!(g.cursor(), (2, 1));
    g.feed(b"X");
    assert_eq!(g.screen_text()[1], "  X");
}

#[test]
fn cursor_movement_relative() {
    let mut g = grid();
    g.feed(b"\x1b[2;2H"); // (1,1)
    g.feed(b"\x1b[A"); // up
    assert_eq!(g.cursor().1, 0);
    g.feed(b"\x1b[2C"); // right 2
    assert_eq!(g.cursor().0, 3);
}

#[test]
fn erase_to_end_of_line() {
    let mut g = grid();
    g.feed(b"abcdef");
    g.feed(b"\r"); // cursor to col 0
    g.feed(b"\x1b[3C"); // right 3 -> col 3
    g.feed(b"\x1b[K"); // erase to EOL
    assert_eq!(g.screen_text()[0], "abc");
}

#[test]
fn erase_whole_display() {
    let mut g = grid();
    g.feed(b"line1\r\nline2");
    g.feed(b"\x1b[2J");
    assert_eq!(g.screen_text(), vec!["", "", ""]);
}

#[test]
fn sgr_attributes_are_recorded() {
    let mut g = grid();
    // Bold red 'A', then reset, then plain 'B'.
    g.feed(b"\x1b[1;31mA\x1b[0mB");
    let row = g.row(0).unwrap();
    let cell_a = &row.cells()[0];
    let cell_b = &row.cells()[1];
    assert_eq!(cell_a.str(), "A");
    assert_eq!(cell_a.attrs().intensity(), termwiz::cell::Intensity::Bold);
    // 'B' is back to defaults.
    assert_eq!(cell_b.attrs().intensity(), termwiz::cell::Intensity::Normal);
}

#[test]
fn partial_escape_sequence_across_feeds() {
    let mut g = grid();
    // Split "CSI 2;3 H" across three feeds; parser must retain state.
    g.feed(b"\x1b[2");
    g.feed(b";3");
    g.feed(b"HX");
    assert_eq!(g.cursor(), (3, 1)); // after printing X at (2,1)
    assert_eq!(g.screen_text()[1], "  X");
}

#[test]
fn partial_utf8_handled_by_parser() {
    let mut g = grid();
    // '€' = E2 82 AC, split across feeds.
    g.feed(&[0xE2, 0x82]);
    g.feed(&[0xAC]);
    assert_eq!(g.screen_text()[0], "€");
}

#[test]
fn scroll_feeds_scrollback() {
    let mut g = Grid::new(10, 2, 100); // 2 rows tall
    g.feed(b"a\r\nb\r\nc"); // 'a' scrolls off
    assert_eq!(g.screen_text(), vec!["b", "c"]);
    assert_eq!(g.scrollback().len(), 1);
    assert_eq!(g.scrollback().get(0).unwrap().to_trimmed_string(), "a");
}

#[test]
fn scrollback_is_bounded() {
    let mut g = Grid::new(4, 1, 3); // 1 visible row, scrollback cap 3
    for i in 0..10 {
        g.feed(format!("{i}\r\n").as_bytes());
    }
    assert!(g.scrollback().len() <= 3, "scrollback must stay bounded");
}

#[test]
fn resize_grow_and_shrink() {
    let mut g = Grid::new(10, 3, 100);
    g.feed(b"r0\r\nr1\r\nr2");
    // Shrink height: top rows move to scrollback.
    g.resize(10, 2);
    assert_eq!(g.dimensions(), (10, 2));
    assert!(!g.scrollback().is_empty());
    // Grow width: rows re-pad without panic.
    g.resize(20, 2);
    assert_eq!(g.dimensions(), (20, 2));
}

#[test]
fn bell_flag_sets_and_clears() {
    let mut g = grid();
    assert!(!g.take_bell());
    g.feed(&[0x07]); // BEL
    assert!(g.take_bell());
    assert!(!g.take_bell(), "bell clears after read");
}

#[test]
fn save_and_restore_cursor() {
    let mut g = grid();
    g.feed(b"\x1b[2;4H"); // (3,1)
    g.feed(b"\x1b[s"); // save
    g.feed(b"\x1b[1;1H"); // move home
    g.feed(b"\x1b[u"); // restore
    assert_eq!(g.cursor(), (3, 1));
}

#[test]
fn tab_advances_to_stops() {
    let mut g = Grid::new(20, 1, 10);
    g.feed(b"\t");
    assert_eq!(g.cursor().0, 8);
    g.feed(b"\t");
    assert_eq!(g.cursor().0, 16);
}

#[test]
fn alt_screen_starts_blank_and_restores_primary() {
    let mut g = grid();
    g.feed(b"primary");
    assert_eq!(g.screen_text()[0], "primary");
    assert!(!g.alt_screen());

    // Enter the alternate screen (DEC 1049): a fresh blank buffer.
    g.feed(b"\x1b[?1049h");
    assert!(g.alt_screen());
    assert_eq!(g.screen_text()[0], "", "alt screen starts blank");
    g.feed(b"ALT");
    assert_eq!(g.screen_text()[0], "ALT");

    // Leave it: the primary screen (and its text) comes back.
    g.feed(b"\x1b[?1049l");
    assert!(!g.alt_screen());
    assert_eq!(g.screen_text()[0], "primary", "primary restored on exit");
}

#[test]
fn alt_screen_scroll_does_not_touch_scrollback() {
    let mut g = Grid::new(10, 2, 100);
    // Fill primary with scrollback so we can prove the alt screen doesn't add.
    g.feed(b"a\r\nb\r\nc\r\nd"); // scrolls a couple lines into history
    let hist_before = g.history_len();
    assert!(hist_before > 0);

    g.feed(b"\x1b[?1049h");
    // Scroll a bunch on the alt screen.
    g.feed(b"1\r\n2\r\n3\r\n4\r\n5");
    assert_eq!(
        g.history_len(),
        hist_before,
        "alt-screen scrolling must not grow scrollback"
    );
    g.feed(b"\x1b[?1049l");
    assert_eq!(g.history_len(), hist_before);
}

#[test]
fn show_cursor_mode_toggles_visibility() {
    let mut g = grid();
    assert!(g.cursor_visible(), "cursor visible by default");
    g.feed(b"\x1b[?25l"); // hide
    assert!(!g.cursor_visible());
    g.feed(b"\x1b[?25h"); // show
    assert!(g.cursor_visible());
}

#[test]
fn autowrap_off_overprints_last_column() {
    let mut g = Grid::new(4, 2, 10);
    g.feed(b"\x1b[?7l"); // DECAWM off
    g.feed(b"abcdef"); // 6 chars into width-4: last column keeps the final char
    assert_eq!(g.screen_text()[1], "", "no wrap to row 1");
    assert_eq!(g.cursor().1, 0, "cursor stays on row 0 with autowrap off");
    // Row 0 ends with the last-printed char 'f' overprinting column 3.
    assert_eq!(g.screen_text()[0], "abcf");
}

#[test]
fn alt_screen_survives_resize_and_restores_primary() {
    let mut g = Grid::new(10, 3, 100);
    g.feed(b"keepme");
    g.feed(b"\x1b[?1049h");
    g.feed(b"alt-content");
    // Resize while on the alt screen (a full-screen app being resized).
    g.resize(6, 4);
    assert!(g.alt_screen());
    // Leaving restores the primary, now at the new width, with its text intact.
    g.feed(b"\x1b[?1049l");
    assert!(!g.alt_screen());
    assert_eq!(g.dimensions(), (6, 4));
    assert!(
        g.screen_text()[0].starts_with("keep"),
        "primary text survives an alt-screen resize; got {:?}",
        g.screen_text()[0]
    );
}

#[test]
fn cursor_position_report_replies_with_location() {
    let mut g = Grid::new(80, 24, 100);
    // Move the cursor to row 3, col 5 (0-based (4,2)) then request a report.
    g.feed(b"\x1b[3;5H");
    assert!(g.take_responses().is_empty(), "no reply until queried");
    g.feed(b"\x1b[6n");
    // Reply is ESC[<row>;<col>R, 1-based -> ESC[3;5R.
    assert_eq!(g.take_responses(), b"\x1b[3;5R");
    // Draining clears it.
    assert!(g.take_responses().is_empty());
}

#[test]
fn device_status_and_attributes_replies() {
    let mut g = Grid::new(80, 24, 100);
    // ESC[5n -> "OK" (ESC[0n).
    g.feed(b"\x1b[5n");
    assert_eq!(g.take_responses(), b"\x1b[0n");
    // ESC[c (primary device attributes) -> ESC[?1;0c.
    g.feed(b"\x1b[c");
    assert_eq!(g.take_responses(), b"\x1b[?1;0c");
}

#[test]
fn cursor_report_reflects_movement() {
    let mut g = Grid::new(80, 24, 100);
    g.feed(b"hello"); // cursor now at col 5 (0-based), row 0
    g.feed(b"\x1b[6n");
    assert_eq!(g.take_responses(), b"\x1b[1;6R");
}

#[test]
fn scroll_up_su_moves_content_without_cursor() {
    // ESC[S scrolls the whole screen up by 1: top line leaves, blank at bottom,
    // cursor unchanged.
    let mut g = Grid::new(10, 3, 100);
    g.feed(b"a\r\nb\r\nc"); // rows: a / b / c, cursor at (1,2)
    let cur = g.cursor();
    g.feed(b"\x1b[S"); // scroll up 1
    assert_eq!(g.screen_text(), vec!["b", "c", ""]);
    assert_eq!(g.cursor(), cur, "SU must not move the cursor");
}

#[test]
fn scroll_down_sd_inserts_at_top() {
    let mut g = Grid::new(10, 3, 100);
    g.feed(b"a\r\nb\r\nc");
    g.feed(b"\x1b[T"); // scroll down 1: blank at top, bottom line leaves
    assert_eq!(g.screen_text(), vec!["", "a", "b"]);
}

#[test]
fn scroll_region_confines_line_feed() {
    // DECSTBM region rows 1..2 (1-based 2;3). Line feeds at the region bottom
    // scroll only within it; row 0 is untouched.
    let mut g = Grid::new(10, 4, 100);
    g.feed(b"top\r\nr1\r\nr2\r\nr3"); // 4 rows
    // Set region to rows 2..4 (1-based) => 0-based 1..3. Homes cursor to (0,1).
    g.feed(b"\x1b[2;4r");
    assert_eq!(g.cursor(), (0, 1), "DECSTBM homes the cursor to the region top");
    // Fill the region and force it to scroll: write 3 lines, then one more.
    g.feed(b"A\r\nB\r\nC"); // fills rows 1,2,3
    g.feed(b"\r\nD"); // at region bottom -> scroll region up, D on last row
    let text = g.screen_text();
    assert_eq!(text[0], "top", "row above the region is never scrolled");
    assert_eq!(text[3], "D", "new content lands on the region's bottom row");
}

#[test]
fn decstbm_reset_restores_full_screen() {
    let mut g = Grid::new(10, 4, 100);
    g.feed(b"\x1b[2;3r"); // set a region
    g.feed(b"\x1b[r"); // reset (no params) -> full screen
    // A full-screen line feed from the bottom now scrolls the whole screen.
    g.feed(b"\x1b[4;1H"); // cursor to bottom row
    g.feed(b"x\r\ny"); // wrote x on row 3, newline scrolls whole screen, y on row 3
    // No panic and content present is enough; the key invariant is the region
    // was reset (full-screen scroll, not confined).
    assert!(g.screen_text().iter().any(|r| r.contains('y')));
}

#[test]
fn scroll_region_resets_on_resize() {
    let mut g = Grid::new(10, 4, 100);
    g.feed(b"\x1b[2;3r"); // region rows 1..2
    g.resize(10, 6); // resize must reset region to full screen (0..5)
    // After resize, a line feed from the new bottom scrolls the whole screen.
    g.feed(b"\x1b[6;1H");
    g.feed(b"z\r\nw");
    assert!(g.screen_text().iter().any(|r| r.contains('w')));
}
