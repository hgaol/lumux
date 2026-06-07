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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKey {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
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
                break;
            };
            let raw = &input[i..i + consumed];
            i += consumed;

            match self.mode {
                Mode::Normal => {
                    if self.bindings.is_prefix(&key) {
                        flush_passthrough!();
                        self.mode = Mode::AwaitingCommand;
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
                        }
                        Some(action) => reactions.push(Reaction::Do(action)),
                        None => { /* unknown command: no-op, back to Normal */ }
                    }
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
            }
        }
        if !passthrough.is_empty() {
            reactions.push(Reaction::PassThrough(passthrough));
        }
        reactions
    }

    /// Force-exit copy mode (e.g. on detach).
    pub fn reset(&mut self) {
        self.mode = Mode::Normal;
    }
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
        // vi-style.
        KeyCode::Char('k') => CopyKey::Up,
        KeyCode::Char('j') => CopyKey::Down,
        KeyCode::Char('h') => CopyKey::Left,
        KeyCode::Char('l') => CopyKey::Right,
        _ => return None,
    };
    Some(ck)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
