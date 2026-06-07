//! Binding table: which key (after the prefix) triggers which action.
//!
//! Defaults mirror tmux. The table is plain data so Phase 9 config can replace
//! or extend it without touching the state machine.

use super::key::{Key, KeyCode};
use std::collections::HashMap;

/// A bound action. These are the prefixed commands; copy-mode navigation is a
/// separate concern handled while in copy state (Phase 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SplitHorizontal,
    SplitVertical,
    NewWindow,
    NextWindow,
    PrevWindow,
    SelectWindow(u32),
    Detach,
    EnterCopyMode,
    KillPane,
    /// The prefix was pressed twice: send a literal prefix byte to the pane.
    SendPrefix,
}

#[derive(Debug, Clone)]
pub struct Bindings {
    /// The prefix key (default Ctrl-b).
    pub prefix: Key,
    table: HashMap<Key, Action>,
}

impl Default for Bindings {
    fn default() -> Self {
        Self::tmux_defaults()
    }
}

impl Bindings {
    pub fn tmux_defaults() -> Self {
        let mut table = HashMap::new();
        table.insert(Key::char('|'), Action::SplitHorizontal);
        table.insert(Key::char('"'), Action::SplitVertical);
        table.insert(Key::char('-'), Action::SplitVertical);
        table.insert(Key::char('%'), Action::SplitHorizontal);
        table.insert(Key::char('c'), Action::NewWindow);
        table.insert(Key::char('n'), Action::NextWindow);
        table.insert(Key::char('p'), Action::PrevWindow);
        table.insert(Key::char('d'), Action::Detach);
        table.insert(Key::char('['), Action::EnterCopyMode);
        table.insert(Key::char('x'), Action::KillPane);
        for n in 0..=9u32 {
            table.insert(
                Key::char(char::from_digit(n, 10).unwrap()),
                Action::SelectWindow(n),
            );
        }
        // Pressing the prefix again sends a literal prefix.
        let prefix = Key::ctrl('b');
        table.insert(prefix, Action::SendPrefix);
        Self { prefix, table }
    }

    pub fn set_prefix(&mut self, prefix: Key) {
        // Re-point the SendPrefix binding at the new prefix.
        self.table.remove(&self.prefix);
        self.table.insert(prefix, Action::SendPrefix);
        self.prefix = prefix;
    }

    pub fn bind(&mut self, key: Key, action: Action) {
        self.table.insert(key, action);
    }

    pub fn lookup(&self, key: &Key) -> Option<&Action> {
        self.table.get(key)
    }

    pub fn is_prefix(&self, key: &Key) -> bool {
        *key == self.prefix
    }
}

/// Encode a key back into the raw bytes a terminal would send, for the
/// `SendPrefix` case (delivering a literal prefix to the pane).
pub fn encode_key(key: &Key) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) if key.ctrl => {
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_uppercase() {
                vec![(upper as u8) - b'A' + 1]
            } else {
                vec![c as u8]
            }
        }
        KeyCode::Char(c) => {
            let mut v = Vec::new();
            if key.alt {
                v.push(0x1b);
            }
            v.push(c as u8);
            v
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Space => vec![b' '],
        KeyCode::Escape => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prefix_is_ctrl_b() {
        let b = Bindings::default();
        assert!(b.is_prefix(&Key::ctrl('b')));
        assert!(!b.is_prefix(&Key::char('b')));
    }

    #[test]
    fn default_bindings_present() {
        let b = Bindings::default();
        assert_eq!(b.lookup(&Key::char('|')), Some(&Action::SplitHorizontal));
        assert_eq!(b.lookup(&Key::char('c')), Some(&Action::NewWindow));
        assert_eq!(b.lookup(&Key::char('d')), Some(&Action::Detach));
        assert_eq!(b.lookup(&Key::char('3')), Some(&Action::SelectWindow(3)));
    }

    #[test]
    fn prefix_self_binding_sends_literal() {
        let b = Bindings::default();
        assert_eq!(b.lookup(&Key::ctrl('b')), Some(&Action::SendPrefix));
    }

    #[test]
    fn rebinding_prefix_moves_send_literal() {
        let mut b = Bindings::default();
        b.set_prefix(Key::ctrl('a'));
        assert!(b.is_prefix(&Key::ctrl('a')));
        assert_eq!(b.lookup(&Key::ctrl('a')), Some(&Action::SendPrefix));
    }

    #[test]
    fn encode_ctrl_b_roundtrips() {
        assert_eq!(encode_key(&Key::ctrl('b')), vec![0x02]);
        assert_eq!(encode_key(&Key::char('x')), vec![b'x']);
        assert_eq!(encode_key(&Key::plain(KeyCode::Up)), b"\x1b[A".to_vec());
    }
}
