//! The prefix state machine.
//!
//! Feeds on raw input bytes from the client and produces [`Reaction`]s. In
//! `Normal` state, the prefix key arms the machine (→ `AwaitingCommand`) and
//! all other input passes through to the focused pane verbatim. The next key
//! after the prefix is matched against the bindings: a hit yields an
//! [`Action`]; a miss returns to Normal (tmux behavior — an unknown command is
//! a no-op). `EnterCopyMode` transitions to `Copy`, where navigation keys are
//! surfaced as [`Reaction::Copy`] until the user exits (Phase 8 interprets
//! those).

use super::bindings::{encode_key, Action, Bindings};
use super::key::{decode_key, Key, KeyCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    AwaitingCommand,
    Copy,
    /// Showing the key-binding help overlay; any key dismisses it.
    Help,
    /// Showing the session switcher; navigation keys pick a session.
    ChooseSession,
    /// Capturing text for a prompt (rename-window/-session): printable keys
    /// extend the buffer, Enter commits, Escape cancels.
    Prompt,
}

/// What the daemon should do in response to a chunk of decoded input.
#[derive(Debug, Clone, PartialEq)]
pub enum Reaction {
    /// Forward these raw bytes to the focused pane's PTY.
    PassThrough(Vec<u8>),
    /// Execute a bound command.
    Do(Action),
    /// A copy-mode navigation key (Phase 8 acts on it).
    Copy(CopyKey),
    /// A session-switcher key (daemon moves the selection / confirms / cancels).
    Session(SessionKey),
    /// A prompt edit key (text entry for rename); the daemon edits its buffer.
    Prompt(PromptKey),
    /// A help-overlay navigation key (scroll the binding list, or close it).
    Help(HelpKey),
}

/// Keys handled while a text prompt is open (rename-window/-session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKey {
    /// Append a typed character to the buffer.
    Char(char),
    /// Delete the last character (Backspace).
    Backspace,
    /// Commit the buffer (Enter).
    Confirm,
    /// Abandon the prompt (Escape).
    Cancel,
}

/// Keys handled while the session switcher is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKey {
    Up,
    Down,
    /// Jump to the Nth session in the list (digit key).
    Index(u32),
    Confirm,
    Cancel,
}

/// Keys handled while the help overlay is open: scroll the binding list, or
/// close it (tmux shows key bindings in a scrollable view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    /// Close the overlay (q / Escape / ?).
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKey {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    /// Half-page scroll (tmux copy-mode-vi u/d).
    HalfPageUp,
    HalfPageDown,
    Home,
    End,
    /// Begin/extend a selection at the cursor (Space / 'v').
    StartSelection,
    /// Copy the selection and exit copy-mode (Enter / 'y').
    Yank,
    Quit,
}

pub struct Keymap {
    bindings: Bindings,
    mode: Mode,
}

impl Keymap {
    pub fn new(bindings: Bindings) -> Self {
        Self {
            bindings,
            mode: Mode::Normal,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(Bindings::default())
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn bindings_mut(&mut self) -> &mut Bindings {
        &mut self.bindings
    }

    pub fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    /// Feed a chunk of raw client input. Returns the reactions in order. A chunk
    /// may contain several keys (paste, or fast typing), so we loop.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Reaction> {
        let mut reactions = Vec::new();
        let mut i = 0;
        // Bytes destined for the pane are coalesced into one PassThrough.
        let mut passthrough: Vec<u8> = Vec::new();

        macro_rules! flush_passthrough {
            () => {
                if !passthrough.is_empty() {
                    reactions.push(Reaction::PassThrough(std::mem::take(&mut passthrough)));
                }
            };
        }

        while i < input.len() {
            let Some((key, consumed)) = decode_key(&input[i..]) else {
                // decode_key only fails on an escape sequence truncated at the
                // end of this chunk (e.g. a large paste split at the client's
                // read-buffer boundary). In Normal mode the remaining bytes are
                // real input bound for the pane — pass them straight through
                // rather than dropping them, which would silently truncate the
                // paste. In the overlay modes (copy/help/prompt/chooser) a single
                // key is expected, so an undecodable tail is just swallowed.
                if matches!(self.mode, Mode::Normal) {
                    passthrough.extend_from_slice(&input[i..]);
                }
                break;
            };
            let raw = &input[i..i + consumed];
            i += consumed;

            match self.mode {
                Mode::Normal => {
                    if self.bindings.is_prefix(&key) {
                        flush_passthrough!();
                        self.mode = Mode::AwaitingCommand;
                    } else if let Some(action) = self.bindings.lookup_root(&key).cloned() {
                        // Root binding (tmux bind -n): fires without the prefix.
                        flush_passthrough!();
                        reactions.push(Reaction::Do(action));
                    } else {
                        passthrough.extend_from_slice(raw);
                    }
                }
                Mode::AwaitingCommand => {
                    flush_passthrough!();
                    self.mode = Mode::Normal;
                    match self.bindings.lookup(&key).cloned() {
                        Some(Action::SendPrefix) => {
                            reactions
                                .push(Reaction::PassThrough(encode_key(&self.bindings.prefix)));
                        }
                        Some(Action::EnterCopyMode) => {
                            self.mode = Mode::Copy;
                            // Emit the action too so the daemon sets up its
                            // per-client copy-mode state.
                            reactions.push(Reaction::Do(Action::EnterCopyMode));
                        }
                        Some(Action::ShowHelp) => {
                            self.mode = Mode::Help;
                            reactions.push(Reaction::Do(Action::ShowHelp));
                        }
                        Some(Action::ChooseSession) => {
                            self.mode = Mode::ChooseSession;
                            reactions.push(Reaction::Do(Action::ChooseSession));
                        }
                        Some(action @ (Action::RenameWindow | Action::RenameSession)) => {
                            // Open a text prompt; the daemon seeds the buffer and
                            // renders the input line. Subsequent keys edit it.
                            self.mode = Mode::Prompt;
                            reactions.push(Reaction::Do(action));
                        }
                        Some(action) => reactions.push(Reaction::Do(action)),
                        None => { /* unknown command: no-op, back to Normal */ }
                    }
                }
                Mode::Prompt => {
                    flush_passthrough!();
                    if let Some(pk) = prompt_key(&key) {
                        if matches!(pk, PromptKey::Confirm | PromptKey::Cancel) {
                            self.mode = Mode::Normal;
                        }
                        reactions.push(Reaction::Prompt(pk));
                    }
                    // Non-text keys (arrows, etc.) are ignored in the prompt.
                }
                Mode::Copy => {
                    flush_passthrough!();
                    if let Some(ck) = copy_key(&key) {
                        if ck == CopyKey::Quit {
                            self.mode = Mode::Normal;
                        }
                        reactions.push(Reaction::Copy(ck));
                    }
                    // Non-navigation keys in copy-mode are ignored.
                }
                Mode::ChooseSession => {
                    flush_passthrough!();
                    if let Some(sk) = session_key(&key) {
                        if matches!(sk, SessionKey::Confirm | SessionKey::Cancel) {
                            self.mode = Mode::Normal;
                        }
                        reactions.push(Reaction::Session(sk));
                    }
                    // Other keys are ignored while the switcher is open.
                }
                Mode::Help => {
                    // tmux shows key bindings in a scrollable view: arrows / vi
                    // keys / paging scroll the list; q / Escape / ? close it.
                    // Unrecognized keys are ignored (the overlay stays open).
                    flush_passthrough!();
                    match help_key(&key) {
                        Some(HelpKey::Close) => {
                            self.mode = Mode::Normal;
                            reactions.push(Reaction::Do(Action::ShowHelp));
                        }
                        Some(hk) => reactions.push(Reaction::Help(hk)),
                        None => {}
                    }
                }
            }
        }
        if !passthrough.is_empty() {
            reactions.push(Reaction::PassThrough(passthrough));
        }
        reactions
    }

    /// Force the keymap into copy-mode. Used when copy-mode is entered by a
    /// non-keyboard path (e.g. a mouse wheel scroll), where the mode transition
    /// doesn't flow through `feed`. Without this the keymap stays in `Normal`,
    /// so copy-mode keys like `q`/arrows would leak through to the shell.
    pub fn enter_copy_mode(&mut self) {
        self.mode = Mode::Copy;
    }

    /// Force-exit copy mode (e.g. on detach).
    pub fn reset(&mut self) {
        self.mode = Mode::Normal;
    }
}

/// Map a key to a session-switcher action while the switcher is open.
fn session_key(key: &Key) -> Option<SessionKey> {
    let sk = match key.code {
        KeyCode::Up | KeyCode::Char('k') => SessionKey::Up,
        KeyCode::Down | KeyCode::Char('j') => SessionKey::Down,
        KeyCode::Enter => SessionKey::Confirm,
        KeyCode::Escape | KeyCode::Char('q') => SessionKey::Cancel,
        KeyCode::Char(c) if c.is_ascii_digit() => SessionKey::Index(c.to_digit(10).unwrap()),
        _ => return None,
    };
    Some(sk)
}

/// Map a key to a help-overlay action while the help overlay is open. Movement
/// keys scroll; q / Escape / ? close. Other keys return None (ignored).
fn help_key(key: &Key) -> Option<HelpKey> {
    let hk = match key.code {
        KeyCode::Up | KeyCode::Char('k') => HelpKey::Up,
        KeyCode::Down | KeyCode::Char('j') => HelpKey::Down,
        KeyCode::PageUp => HelpKey::PageUp,
        KeyCode::PageDown | KeyCode::Space => HelpKey::PageDown,
        KeyCode::Home | KeyCode::Char('g') => HelpKey::Top,
        KeyCode::End | KeyCode::Char('G') => HelpKey::Bottom,
        KeyCode::Char('q') | KeyCode::Escape | KeyCode::Char('?') | KeyCode::Enter => HelpKey::Close,
        _ => return None,
    };
    Some(hk)
}

/// Map a key to a prompt edit action while a text prompt (rename) is open.
fn prompt_key(key: &Key) -> Option<PromptKey> {
    // Backspace arrives as Ctrl-H (0x08) or DEL (0x7f) depending on the terminal.
    if (key.ctrl && key.code == KeyCode::Char('h'))
        || key.code == KeyCode::Char('\u{7f}')
        || key.code == KeyCode::Char('\u{8}')
    {
        return Some(PromptKey::Backspace);
    }
    let pk = match key.code {
        KeyCode::Enter => PromptKey::Confirm,
        KeyCode::Escape => PromptKey::Cancel,
        KeyCode::Space => PromptKey::Char(' '),
        // Plain printable characters extend the buffer; ignore ctrl/alt combos.
        KeyCode::Char(c) if !key.ctrl && !key.alt && !c.is_control() => PromptKey::Char(c),
        _ => return None,
    };
    Some(pk)
}

fn copy_key(key: &Key) -> Option<CopyKey> {
    let ck = match key.code {
        KeyCode::Up => CopyKey::Up,
        KeyCode::Down => CopyKey::Down,
        KeyCode::Left => CopyKey::Left,
        KeyCode::Right => CopyKey::Right,
        KeyCode::PageUp => CopyKey::PageUp,
        KeyCode::PageDown => CopyKey::PageDown,
        KeyCode::Home => CopyKey::Home,
        KeyCode::End => CopyKey::End,
        KeyCode::Escape => CopyKey::Quit,
        KeyCode::Char('q') => CopyKey::Quit,
        KeyCode::Space => CopyKey::StartSelection,
        KeyCode::Char('v') => CopyKey::StartSelection,
        KeyCode::Enter => CopyKey::Yank,
        KeyCode::Char('y') => CopyKey::Yank,
        // vi-style.
        KeyCode::Char('k') => CopyKey::Up,
        KeyCode::Char('j') => CopyKey::Down,
        KeyCode::Char('h') => CopyKey::Left,
        KeyCode::Char('l') => CopyKey::Right,
        // vi half-page scroll (tmux copy-mode-vi u/d).
        KeyCode::Char('u') => CopyKey::HalfPageUp,
        KeyCode::Char('d') => CopyKey::HalfPageDown,
        _ => return None,
    };
    Some(ck)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
