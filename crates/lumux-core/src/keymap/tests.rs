use crate::keymap::{Action, CopyKey, Key, Keymap, Mode, Reaction, SearchKey};

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
fn paste_ending_mid_escape_is_not_truncated() {
    // Regression: a large paste is split into chunks at the client's read buffer
    // boundary. If a chunk ends mid-escape-sequence (a bare ESC[ needing a third
    // byte), decode_key returns None — and feed() used to `break`, silently
    // dropping the rest of the chunk. Every byte fed must reach the pane.
    let mut k = km();
    // "echo hi" then a truncated CSI ("\x1b[") at the very end of the chunk.
    let chunk = b"echo hi\x1b[";
    let r = k.feed(chunk);
    let got: Vec<u8> = r
        .into_iter()
        .flat_map(|x| match x {
            Reaction::PassThrough(b) => b,
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(got, chunk, "no bytes may be dropped, even a partial escape tail");
}

#[test]
fn large_paste_chunk_ending_mid_escape_survives_intact() {
    // Mirror the real failure: the client splits a big paste at its 4096-byte
    // read buffer, and a chunk happens to end mid-escape-sequence. The whole
    // chunk — including the truncated escape tail — must reach the pane.
    let mut k = km();
    let mut chunk = Vec::new();
    for i in 0..5000u32 {
        chunk.extend_from_slice(format!("line {i}\n").as_bytes());
    }
    // End the chunk on a bare CSI introducer (needs a third byte to decode).
    chunk.extend_from_slice(b"\x1b[");
    let r = k.feed(&chunk);
    let out_len: usize = r
        .iter()
        .map(|x| match x {
            Reaction::PassThrough(b) => b.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(out_len, chunk.len(), "every pasted byte must reach the pane");
}

fn passthrough_bytes(reactions: Vec<Reaction>) -> Vec<u8> {
    reactions
        .into_iter()
        .flat_map(|x| match x {
            Reaction::PassThrough(b) => b,
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn bracketed_paste_body_is_forwarded_verbatim() {
    use crate::keymap::{Mode, PASTE_END, PASTE_START};
    let mut k = km();
    // A paste whose BODY contains the prefix byte (Ctrl-b = 0x02) and a binding
    // char ('%'). None of it may be interpreted — it must all reach the pane,
    // markers included, and the keymap must return to Normal afterward.
    let mut input = Vec::new();
    input.extend_from_slice(PASTE_START);
    input.extend_from_slice(b"a\x02b % c\n");
    input.extend_from_slice(PASTE_END);
    let out = passthrough_bytes(k.feed(&input));
    assert_eq!(out, input, "paste body (and markers) must pass through untouched");
    assert_eq!(k.mode(), Mode::Normal, "keymap returns to Normal after the end marker");

    // After the paste, the prefix works normally again.
    let r = k.feed(&[0x02, b'%']);
    assert_eq!(r, vec![Reaction::Do(Action::SplitHorizontal)]);
}

#[test]
fn bracketed_paste_split_across_feeds() {
    use crate::keymap::{Mode, PASTE_END, PASTE_START};
    let mut k = km();
    // Start marker + first half of the body in one chunk...
    let mut a = Vec::new();
    a.extend_from_slice(PASTE_START);
    a.extend_from_slice(b"first\x02half");
    let out_a = passthrough_bytes(k.feed(&a));
    assert_eq!(out_a, a, "first chunk forwarded verbatim");
    assert_eq!(k.mode(), Mode::Paste, "still inside the paste between chunks");

    // ...rest of the body + end marker in the next chunk.
    let mut b = Vec::new();
    b.extend_from_slice(b" second % half\n");
    b.extend_from_slice(PASTE_END);
    let out_b = passthrough_bytes(k.feed(&b));
    assert_eq!(out_b, b, "second chunk forwarded verbatim incl. end marker");
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn prefix_then_command_triggers_action() {
    let mut k = km();
    // Ctrl-b then '%'
    let r = k.feed(&[0x02, b'%']);
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
    // "ab" passes through, then Ctrl-b '%' triggers split.
    let r = k.feed(&[b'a', b'b', 0x02, b'%']);
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
fn copy_mode_vi_word_and_goto_motions() {
    let mut k = km();
    k.feed(&[0x02, b'[']); // enter copy-mode
    assert_eq!(k.feed(b"w"), vec![Reaction::Copy(CopyKey::WordForward)]);
    assert_eq!(k.feed(b"b"), vec![Reaction::Copy(CopyKey::WordBackward)]);
    assert_eq!(k.feed(b"e"), vec![Reaction::Copy(CopyKey::WordEnd)]);
    assert_eq!(k.feed(b"0"), vec![Reaction::Copy(CopyKey::LineStart)]);
    assert_eq!(k.feed(b"^"), vec![Reaction::Copy(CopyKey::LineFirstNonBlank)]);
    assert_eq!(k.feed(b"$"), vec![Reaction::Copy(CopyKey::End)]);
    assert_eq!(k.feed(b"g"), vec![Reaction::Copy(CopyKey::Top)]);
    assert_eq!(k.feed(b"G"), vec![Reaction::Copy(CopyKey::Bottom)]);
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
fn copy_mode_search_open_type_and_run() {
    let mut k = km();
    k.feed(&[0x02, b'[']);
    // `/` opens a forward search and switches to Search mode.
    let r = k.feed(b"/");
    assert_eq!(r, vec![Reaction::Copy(CopyKey::SearchForward)]);
    assert_eq!(k.mode(), Mode::Search);
    // Typed chars become SearchKey::Char (NOT pane passthrough or nav keys).
    assert_eq!(
        k.feed(b"ab"),
        vec![
            Reaction::Search(SearchKey::Char('a')),
            Reaction::Search(SearchKey::Char('b')),
        ]
    );
    // Enter runs the search and returns to copy-mode navigation (not Normal).
    let r = k.feed(b"\r");
    assert_eq!(r, vec![Reaction::Search(SearchKey::Confirm)]);
    assert_eq!(k.mode(), Mode::Copy);
}

#[test]
fn copy_mode_search_cancel_returns_to_copy() {
    let mut k = km();
    k.feed(&[0x02, b'[']);
    k.feed(b"?"); // backward search
    assert_eq!(k.mode(), Mode::Search);
    let r = k.feed(b"\x1b"); // Escape
    assert_eq!(r, vec![Reaction::Search(SearchKey::Cancel)]);
    assert_eq!(k.mode(), Mode::Copy, "cancel returns to copy nav, not Normal");
}

#[test]
fn copy_mode_repeat_search_keys() {
    let mut k = km();
    k.feed(&[0x02, b'[']);
    assert_eq!(k.feed(b"n"), vec![Reaction::Copy(CopyKey::RepeatSearch)]);
    assert_eq!(k.feed(b"N"), vec![Reaction::Copy(CopyKey::RepeatSearchRev)]);
    // n/N don't leave copy-mode.
    assert_eq!(k.mode(), Mode::Copy);
}

#[test]
fn copy_mode_rectangle_toggle_keys() {
    let mut k = km();
    k.feed(&[0x02, b'[']);
    // Ctrl-v (byte 0x16) toggles rectangle/block selection.
    assert_eq!(k.feed(&[0x16]), vec![Reaction::Copy(CopyKey::RectangleToggle)]);
    // Capital R does too (vi copy-mode alias).
    assert_eq!(k.feed(b"R"), vec![Reaction::Copy(CopyKey::RectangleToggle)]);
    assert_eq!(k.mode(), Mode::Copy, "toggling block stays in copy-mode");
}

#[test]
fn emacs_mode_keys_in_copy_mode() {
    use crate::keymap::ModeKeys;
    let mut k = km();
    k.set_mode_keys(ModeKeys::Emacs);
    k.feed(&[0x02, b'[']);
    // Ctrl-n/p/f/b move; Ctrl-v pages down; Ctrl-a/e are line ends.
    assert_eq!(k.feed(&[0x0e]), vec![Reaction::Copy(CopyKey::Down)]); // C-n
    assert_eq!(k.feed(&[0x10]), vec![Reaction::Copy(CopyKey::Up)]); // C-p
    assert_eq!(k.feed(&[0x06]), vec![Reaction::Copy(CopyKey::Right)]); // C-f
    assert_eq!(k.feed(&[0x02]), vec![Reaction::Copy(CopyKey::Left)]); // C-b
    assert_eq!(k.feed(&[0x16]), vec![Reaction::Copy(CopyKey::PageDown)]); // C-v
    assert_eq!(k.feed(&[0x01]), vec![Reaction::Copy(CopyKey::Home)]); // C-a
    assert_eq!(k.feed(&[0x05]), vec![Reaction::Copy(CopyKey::End)]); // C-e
    // C-s / C-r open search.
    assert_eq!(k.feed(&[0x13]), vec![Reaction::Copy(CopyKey::SearchForward)]); // C-s
    assert_eq!(k.mode(), Mode::Search, "C-s opens the search input");
    k.feed(b"\x1b"); // cancel search, back to copy
    // C-w copies (yank). The keymap emits the Yank reaction; the daemon is what
    // exits copy-mode on yank (via reset), so the keymap itself stays in Copy —
    // same as vi `y`.
    assert_eq!(k.feed(&[0x17]), vec![Reaction::Copy(CopyKey::Yank)]); // C-w
}

#[test]
fn vi_keys_still_work_with_default_mode() {
    // The default (vi) mode keeps hjkl working; emacs ctrl keys are NOT bound.
    let mut k = km();
    k.feed(&[0x02, b'[']);
    assert_eq!(k.feed(b"j"), vec![Reaction::Copy(CopyKey::Down)]);
    // Arrow keys work regardless of mode-keys.
    assert_eq!(k.feed(b"\x1b[A"), vec![Reaction::Copy(CopyKey::Up)]);
}

#[test]
fn buffer_chooser_opens_and_navigates() {
    use crate::keymap::BufferKey;
    let mut k = km();
    // Prefix = opens the buffer chooser (emits the action AND switches mode).
    let r = k.feed(&[0x02, b'=']);
    assert_eq!(r, vec![Reaction::Do(Action::ChooseBuffer)]);
    assert_eq!(k.mode(), Mode::ChooseBuffer);
    // Navigation keys become Buffer reactions; the chooser stays open.
    assert_eq!(k.feed(b"j"), vec![Reaction::Buffer(BufferKey::Down)]);
    assert_eq!(k.feed(b"k"), vec![Reaction::Buffer(BufferKey::Up)]);
    assert_eq!(k.feed(b"2"), vec![Reaction::Buffer(BufferKey::Index(2))]);
    assert_eq!(k.feed(b"d"), vec![Reaction::Buffer(BufferKey::Delete)]);
    assert_eq!(k.mode(), Mode::ChooseBuffer, "delete keeps the chooser open");
    // Enter pastes and closes.
    assert_eq!(k.feed(b"\r"), vec![Reaction::Buffer(BufferKey::Confirm)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn buffer_chooser_cancel_closes() {
    use crate::keymap::BufferKey;
    let mut k = km();
    k.feed(&[0x02, b'=']);
    assert_eq!(k.feed(b"\x1b"), vec![Reaction::Buffer(BufferKey::Cancel)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn paste_buffer_action_binding() {
    let mut k = km();
    // Prefix ] is a one-shot action (paste); no mode change.
    let r = k.feed(&[0x02, b']']);
    assert_eq!(r, vec![Reaction::Do(Action::PasteBuffer)]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn break_and_swap_pane_bindings() {
    let mut k = km();
    // Prefix ! breaks the pane out; {/} swap with prev/next. All one-shot.
    assert_eq!(k.feed(&[0x02, b'!']), vec![Reaction::Do(Action::BreakPane)]);
    assert_eq!(k.feed(&[0x02, b'{']), vec![Reaction::Do(Action::SwapPanePrev)]);
    assert_eq!(k.feed(&[0x02, b'}']), vec![Reaction::Do(Action::SwapPaneNext)]);
    assert_eq!(k.mode(), Mode::Normal);
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
fn prefix_question_shows_help_and_scrolls_or_closes() {
    use crate::keymap::HelpKey;
    let mut k = km();
    // Ctrl-b ? opens help.
    let r = k.feed(&[0x02, b'?']);
    assert_eq!(r, vec![Reaction::Do(Action::ShowHelp)]);
    assert_eq!(k.mode(), Mode::Help);
    // Movement keys scroll the list (and stay in Help). Down arrow = ESC[B.
    let r = k.feed(b"\x1b[B");
    assert_eq!(r, vec![Reaction::Help(HelpKey::Down)]);
    assert_eq!(k.mode(), Mode::Help);
    // vi 'j' also scrolls down; 'k' up.
    assert_eq!(k.feed(b"j"), vec![Reaction::Help(HelpKey::Down)]);
    assert_eq!(k.feed(b"k"), vec![Reaction::Help(HelpKey::Up)]);
    // An unrecognized key is ignored (overlay stays open, key swallowed).
    assert_eq!(k.feed(b"x"), vec![]);
    assert_eq!(k.mode(), Mode::Help);
    // 'q' closes (re-emits ShowHelp to toggle off) and swallows the key.
    let r = k.feed(b"q");
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
        .any(|(k, d)| k == "C-b %" && d.contains("split")));
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
fn session_switcher_expand_collapse_keys() {
    use crate::keymap::SessionKey;
    let mut k = km();
    k.feed(&[0x02, b's']);
    // Right / l expand the highlighted session; Left / h collapse. The chooser
    // stays open (only Enter/Esc close it).
    assert_eq!(k.feed(b"\x1b[C"), vec![Reaction::Session(SessionKey::Expand)]); // Right
    assert_eq!(k.feed(b"l"), vec![Reaction::Session(SessionKey::Expand)]);
    assert_eq!(k.feed(b"\x1b[D"), vec![Reaction::Session(SessionKey::Collapse)]); // Left
    assert_eq!(k.feed(b"h"), vec![Reaction::Session(SessionKey::Collapse)]);
    assert_eq!(k.mode(), Mode::ChooseSession, "expand/collapse keep the chooser open");
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

#[test]
fn repeatable_binding_repeats_without_reprefixing() {
    use crate::keymap::KeyCode;
    let mut k = km();
    k.feed(&[0x02]); // prefix
    // Ctrl-Left (ESC[1;5D) is resize-pane-left, bound with tmux's default -r.
    let r = k.feed(b"\x1b[1;5D");
    assert_eq!(r, vec![Reaction::Do(Action::ResizePaneLeft)]);
    let rep_key = Key { code: KeyCode::Left, ctrl: true, alt: false };
    assert_eq!(k.mode(), Mode::Repeat(rep_key));

    // Same key again, WITHOUT the prefix: repeats.
    let r2 = k.feed(b"\x1b[1;5D");
    assert_eq!(r2, vec![Reaction::Do(Action::ResizePaneLeft)]);
    assert_eq!(k.mode(), Mode::Repeat(rep_key), "still repeating");

    // A different key exits repeat mode and is reprocessed as ordinary Normal
    // input (here: plain passthrough text, since 'x' isn't a root binding).
    let r3 = k.feed(b"x");
    assert_eq!(r3, vec![Reaction::PassThrough(b"x".to_vec())]);
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn non_repeatable_binding_returns_straight_to_normal() {
    let mut k = km();
    // 'c' (new-window) is NOT marked repeatable.
    let r = k.feed(&[0x02, b'c']);
    assert_eq!(r, vec![Reaction::Do(Action::NewWindow)]);
    assert_eq!(k.mode(), Mode::Normal, "non-repeatable bindings don't arm Repeat");
}

#[test]
fn cancel_repeat_forces_normal_mode() {
    let mut k = km();
    k.feed(&[0x02]);
    k.feed(b"\x1b[1;5D");
    assert!(matches!(k.mode(), Mode::Repeat(_)));
    k.cancel_repeat();
    assert_eq!(k.mode(), Mode::Normal);
    // A no-op when already Normal.
    k.cancel_repeat();
    assert_eq!(k.mode(), Mode::Normal);
}

#[test]
fn clock_mode_opens_and_any_key_closes_it() {
    let mut k = km();
    let r = k.feed(&[0x02, b't']);
    assert_eq!(r, vec![Reaction::Do(Action::ClockMode)]);
    assert_eq!(k.mode(), Mode::Clock);
    // ANY key closes it — even an unrelated letter — by toggling ClockMode off.
    let r2 = k.feed(b"x");
    assert_eq!(r2, vec![Reaction::Do(Action::ClockMode)]);
    assert_eq!(k.mode(), Mode::Normal);
}
