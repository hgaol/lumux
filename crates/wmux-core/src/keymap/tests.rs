use crate::keymap::{Action, CopyKey, Key, Keymap, Mode, Reaction};

fn km() -> Keymap {
    Keymap::with_defaults()
}

#[test]
fn plain_typing_passes_through() {
    let mut k = km();
    let r = k.feed(b"ls -la\r");
    assert_eq!(r, vec![Reaction::PassThrough(b"ls -la\r".to_vec())]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn prefix_then_command_triggers_action() {
    let mut k = km();
    // Ctrl-b then '|'
    let r = k.feed(&[0x02, b'|']);
    assert_eq!(r, vec![Reaction::Do(Action::SplitHorizontal)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn prefix_and_command_split_across_feeds() {
    let mut k = km();
    let r1 = k.feed(&[0x02]); // prefix alone
    assert!(r1.is_empty(), "prefix alone yields nothing yet");
    assert_eq!(k.mode(), Mode::AwaitingCommand);
    let r2 = k.feed(b"c"); // command next feed
    assert_eq!(r2, vec![Reaction::Do(Action::NewWindow)]);
}

#[test]
fn passthrough_then_prefix_command_in_one_chunk() {
    let mut k = km();
    // "ab" passes through, then Ctrl-b '|' triggers split.
    let r = k.feed(&[b'a', b'b', 0x02, b'|']);
    assert_eq!(
        r,
        vec![
            Reaction::PassThrough(b"ab".to_vec()),
            Reaction::Do(Action::SplitHorizontal),
        ]
    );
}

#[test]
fn unknown_command_is_noop() {
    let mut k = km();
    // Ctrl-b then 'Z' (unbound) -> nothing, back to Normal.
    let r = k.feed(&[0x02, b'Z']);
    assert!(r.is_empty());
    assert_eq!(k.mode(), Mode::Normal);
    // And subsequent typing passes through normally.
    let r2 = k.feed(b"x");
    assert_eq!(r2, vec![Reaction::PassThrough(b"x".to_vec())]);
}

#[test]
fn double_prefix_sends_literal() {
    let mut k = km();
    // Ctrl-b Ctrl-b -> literal Ctrl-b to the pane.
    let r = k.feed(&[0x02, 0x02]);
    assert_eq!(r, vec![Reaction::PassThrough(vec![0x02])]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn select_window_by_digit() {
    let mut k = km();
    let r = k.feed(&[0x02, b'3']);
    assert_eq!(r, vec![Reaction::Do(Action::SelectWindow(3))]);
}

#[test]
fn enter_and_exit_copy_mode() {
    let mut k = km();
    // Ctrl-b [ enters copy mode and emits the EnterCopyMode action so the
    // daemon can set up copy state.
    let r = k.feed(&[0x02, b'[']);
    assert_eq!(r, vec![Reaction::Do(Action::EnterCopyMode)]);
    assert_eq!(k.mode(), Mode::Copy);
    // Arrow keys become copy navigation.
    let r = k.feed(b"\x1b[A");
    assert_eq!(r, vec![Reaction::Copy(CopyKey::Up)]);
    // vi-style 'j' too.
    let r = k.feed(b"j");
    assert_eq!(r, vec![Reaction::Copy(CopyKey::Down)]);
    // 'q' quits copy mode.
    let r = k.feed(b"q");
    assert_eq!(r, vec![Reaction::Copy(CopyKey::Quit)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn copy_mode_page_navigation() {
    let mut k = km();
    k.feed(&[0x02, b'[']);
    assert_eq!(k.feed(b"\x1b[5~"), vec![Reaction::Copy(CopyKey::PageUp)]);
    assert_eq!(k.feed(b"\x1b[6~"), vec![Reaction::Copy(CopyKey::PageDown)]);
}

#[test]
fn copy_mode_ignores_non_nav_keys() {
    let mut k = km();
    k.feed(&[0x02, b'[']);
    let r = k.feed(b"z"); // not a nav key
    assert!(r.is_empty());
    assert_eq!(k.mode(), Mode::Copy, "stays in copy mode");
}

#[test]
fn rebound_prefix_works() {
    let mut k = km();
    k.bindings_mut().set_prefix(Key::ctrl('a'));
    // Now Ctrl-a is the prefix; Ctrl-b passes through.
    let r = k.feed(&[0x02]);
    assert_eq!(r, vec![Reaction::PassThrough(vec![0x02])]);
    let r = k.feed(&[0x01, b'c']);
    assert_eq!(r, vec![Reaction::Do(Action::NewWindow)]);
}

#[test]
fn reset_exits_copy_mode() {
    let mut k = km();
    k.feed(&[0x02, b'[']);
    assert_eq!(k.mode(), Mode::Copy);
    k.reset();
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn detach_binding() {
    let mut k = km();
    assert_eq!(k.feed(&[0x02, b'd']), vec![Reaction::Do(Action::Detach)]);
}
