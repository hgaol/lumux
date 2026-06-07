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
    assert_eq!(
        cell_a.attrs().intensity(),
        termwiz::cell::Intensity::Bold
    );
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
