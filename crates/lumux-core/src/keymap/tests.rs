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

#[test]
fn root_binding_fires_without_prefix() {
    let mut k = km();
    // Bind Alt-Left as a root (no-prefix) binding to select-pane-left.
    k.bindings_mut().bind_root(
        Key {
            code: crate::keymap::KeyCode::Left,
            ctrl: false,
            alt: true,
        },
        Action::SelectPaneLeft,
    );
    // Alt-Left arrives as ESC[1;3D and should fire immediately, no prefix.
    let r = k.feed(b"\x1b[1;3D");
    assert_eq!(r, vec![Reaction::Do(Action::SelectPaneLeft)]);
    // A plain (unbound) key still passes through.
    let r = k.feed(b"z");
    assert_eq!(r, vec![Reaction::PassThrough(b"z".to_vec())]);
}

#[test]
fn prefixed_arrow_selects_pane_directionally() {
    let mut k = km();
    // Ctrl-b then Right -> select-pane-right (tmux default).
    let r = k.feed(b"\x02\x1b[C");
    assert_eq!(r, vec![Reaction::Do(Action::SelectPaneRight)]);
}

#[test]
fn prefix_question_shows_help_and_any_key_dismisses() {
    let mut k = km();
    // Ctrl-b ? opens help.
    let r = k.feed(&[0x02, b'?']);
    assert_eq!(r, vec![Reaction::Do(Action::ShowHelp)]);
    assert_eq!(k.mode(), Mode::Help);
    // Any key dismisses (re-emits ShowHelp to toggle off) and swallows the key.
    let r = k.feed(b"x");
    assert_eq!(r, vec![Reaction::Do(Action::ShowHelp)]);
    assert_eq!(k.mode(), Mode::Normal);
    // Normal typing resumes.
    let r = k.feed(b"y");
    assert_eq!(r, vec![Reaction::PassThrough(b"y".to_vec())]);
}

#[test]
fn help_entries_lists_bindings() {
    let b = crate::keymap::Bindings::tmux_defaults();
    let entries = b.help_entries();
    // The help lists the split and detach bindings with the prefix prefixed.
    assert!(entries
        .iter()
        .any(|(k, d)| k == "C-b |" && d.contains("split")));
    assert!(entries.iter().any(|(k, _)| k == "C-b ?"));
    assert!(entries
        .iter()
        .any(|(k, d)| k == "C-b d" && d.contains("detach")));
    // The literal send-prefix entry is omitted.
    assert!(!entries.iter().any(|(_, d)| d.contains("send the prefix")));
}

#[test]
fn prefix_s_opens_session_switcher() {
    use crate::keymap::SessionKey;
    let mut k = km();
    // Ctrl-b s opens the switcher.
    let r = k.feed(&[0x02, b's']);
    assert_eq!(r, vec![Reaction::Do(Action::ChooseSession)]);
    assert_eq!(k.mode(), Mode::ChooseSession);
    // Down/Up navigate; a digit jumps; Enter confirms; Esc cancels.
    assert_eq!(k.feed(b"\x1b[B"), vec![Reaction::Session(SessionKey::Down)]);
    assert_eq!(k.feed(b"2"), vec![Reaction::Session(SessionKey::Index(2))]);
    let r = k.feed(b"\r");
    assert_eq!(r, vec![Reaction::Session(SessionKey::Confirm)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn session_switcher_cancel_returns_to_normal() {
    use crate::keymap::SessionKey;
    let mut k = km();
    k.feed(&[0x02, b's']);
    assert_eq!(k.mode(), Mode::ChooseSession);
    let r = k.feed(b"\x1b"); // Escape
    assert_eq!(r, vec![Reaction::Session(SessionKey::Cancel)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn prefix_comma_opens_rename_window_prompt() {
    use crate::keymap::PromptKey;
    let mut k = km();
    // Ctrl-b , opens the rename-window prompt.
    let r = k.feed(&[0x02, b',']);
    assert_eq!(r, vec![Reaction::Do(Action::RenameWindow)]);
    assert_eq!(k.mode(), Mode::Prompt);
    // Typing extends the buffer; backspace deletes; Enter confirms.
    assert_eq!(k.feed(b"a"), vec![Reaction::Prompt(PromptKey::Char('a'))]);
    assert_eq!(k.feed(b"b"), vec![Reaction::Prompt(PromptKey::Char('b'))]);
    assert_eq!(k.feed(b"\x7f"), vec![Reaction::Prompt(PromptKey::Backspace)]);
    let r = k.feed(b"\r");
    assert_eq!(r, vec![Reaction::Prompt(PromptKey::Confirm)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn prefix_dollar_opens_rename_session_prompt_and_cancels() {
    use crate::keymap::PromptKey;
    let mut k = km();
    let r = k.feed(&[0x02, b'$']);
    assert_eq!(r, vec![Reaction::Do(Action::RenameSession)]);
    assert_eq!(k.mode(), Mode::Prompt);
    // Escape cancels and returns to Normal.
    let r = k.feed(b"\x1b");
    assert_eq!(r, vec![Reaction::Prompt(PromptKey::Cancel)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn prompt_space_is_captured_as_char() {
    use crate::keymap::PromptKey;
    let mut k = km();
    k.feed(&[0x02, b',']);
    assert_eq!(k.feed(b" "), vec![Reaction::Prompt(PromptKey::Char(' '))]);
}
