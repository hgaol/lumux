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
    /// Jump to the previously-active window (tmux `last-window`, prefix `l`).
    LastWindow,
    /// Kill the active window and all its panes (tmux `kill-window`, prefix `&`).
    KillWindow,
    /// Directional pane focus (tmux select-pane -L/-R/-U/-D).
    SelectPaneLeft,
    SelectPaneRight,
    SelectPaneUp,
    SelectPaneDown,
    /// Directional pane resize (tmux resize-pane -L/-R/-U/-D).
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    /// Toggle zoom on the active pane (tmux prefix z).
    ZoomPane,
    /// Break the active pane out into its own new window (tmux break-pane, `!`).
    BreakPane,
    /// Swap the active pane with the previous one in the window (tmux `{`).
    SwapPanePrev,
    /// Swap the active pane with the next one in the window (tmux `}`).
    SwapPaneNext,
    /// Cycle to the next preset layout (tmux `next-layout`, prefix Space).
    NextLayout,
    /// Jump to the previously-active pane (tmux `last-pane`, prefix `;`).
    LastPane,
    /// Open a prompt to rename the current window (tmux prefix ,).
    RenameWindow,
    /// Open a prompt to rename the current session (tmux prefix $).
    RenameSession,
    Detach,
    EnterCopyMode,
    KillPane,
    /// Re-source the config file and flash a confirmation (tmux prefix r).
    ReloadConfig,
    /// Show the key-binding help overlay (tmux prefix ?).
    ShowHelp,
    /// Open the session switcher (tmux prefix s).
    ChooseSession,
    /// Show pane numbers; the next digit focuses that pane (tmux prefix q).
    DisplayPanes,
    /// Move the active window one slot earlier in the window list (prefix `<`).
    SwapWindowLeft,
    /// Move the active window one slot later in the window list (prefix `>`).
    SwapWindowRight,
    /// Toggle synchronize-panes for the active window (lumux: prefix `S`).
    ToggleSync,
    /// Open a prompt to find/switch to a window by name (tmux prefix `f`).
    FindWindow,
    /// Paste the most-recent paste buffer into the active pane (tmux prefix ]).
    PasteBuffer,
    /// Open the paste-buffer chooser (tmux prefix =).
    ChooseBuffer,
    /// The prefix was pressed twice: send a literal prefix byte to the pane.
    SendPrefix,
}

impl Action {
    /// A short human-readable description for the help overlay.
    pub fn describe(&self) -> &'static str {
        match self {
            Action::SplitHorizontal => "split pane left/right",
            Action::SplitVertical => "split pane top/bottom",
            Action::NewWindow => "new window",
            Action::NextWindow => "next window",
            Action::PrevWindow => "previous window",
            Action::SelectWindow(_) => "select window by number",
            Action::LastWindow => "last (previous) window",
            Action::KillWindow => "kill the active window",
            Action::SelectPaneLeft => "select pane to the left",
            Action::SelectPaneRight => "select pane to the right",
            Action::SelectPaneUp => "select pane above",
            Action::SelectPaneDown => "select pane below",
            Action::ResizePaneLeft => "resize pane left",
            Action::ResizePaneRight => "resize pane right",
            Action::ResizePaneUp => "resize pane up",
            Action::ResizePaneDown => "resize pane down",
            Action::ZoomPane => "zoom/unzoom the active pane",
            Action::BreakPane => "break the pane into a new window",
            Action::SwapPanePrev => "swap with the previous pane",
            Action::SwapPaneNext => "swap with the next pane",
            Action::NextLayout => "cycle preset layouts",
            Action::LastPane => "last (previous) pane",
            Action::RenameWindow => "rename the current window",
            Action::RenameSession => "rename the current session",
            Action::Detach => "detach (session keeps running)",
            Action::EnterCopyMode => "enter copy-mode",
            Action::KillPane => "kill the active pane",
            Action::ReloadConfig => "reload configuration",
            Action::ShowHelp => "show this help",
            Action::ChooseSession => "choose session",
            Action::DisplayPanes => "show pane numbers (then press one)",
            Action::SwapWindowLeft => "move this window left in the list",
            Action::SwapWindowRight => "move this window right in the list",
            Action::ToggleSync => "toggle synchronize-panes",
            Action::FindWindow => "find a window by name",
            Action::PasteBuffer => "paste the most recent buffer",
            Action::ChooseBuffer => "choose a paste buffer",
            Action::SendPrefix => "send the prefix key to the shell",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bindings {
    /// The prefix key (default Ctrl-b).
    pub prefix: Key,
    /// Bindings that fire after the prefix.
    table: HashMap<Key, Action>,
    /// Root bindings that fire WITHOUT the prefix (tmux `bind -n`), checked on
    /// every key in Normal mode before pass-through.
    root: HashMap<Key, Action>,
}

impl Default for Bindings {
    fn default() -> Self {
        Self::tmux_defaults()
    }
}

impl Bindings {
    pub fn tmux_defaults() -> Self {
        let mut table = HashMap::new();
        // Splits: tmux's true defaults are " (top/bottom) and % (left/right).
        table.insert(Key::char('"'), Action::SplitVertical);
        table.insert(Key::char('%'), Action::SplitHorizontal);
        table.insert(Key::char('c'), Action::NewWindow);
        table.insert(Key::char('n'), Action::NextWindow);
        table.insert(Key::char('p'), Action::PrevWindow);
        table.insert(Key::char('l'), Action::LastWindow);
        table.insert(Key::char(';'), Action::LastPane);
        table.insert(Key::char('&'), Action::KillWindow);
        table.insert(Key::char('d'), Action::Detach);
        table.insert(Key::char('['), Action::EnterCopyMode);
        table.insert(Key::char('x'), Action::KillPane);
        table.insert(Key::char('?'), Action::ShowHelp);
        table.insert(Key::char('s'), Action::ChooseSession);
        // Show pane numbers; the next digit focuses that pane (tmux prefix q).
        table.insert(Key::char('q'), Action::DisplayPanes);
        // Reorder the active window in the list (lumux: prefix < / >).
        table.insert(Key::char('<'), Action::SwapWindowLeft);
        table.insert(Key::char('>'), Action::SwapWindowRight);
        // Toggle synchronize-panes (lumux: prefix S).
        table.insert(Key::char('S'), Action::ToggleSync);
        // Find/switch to a window by name (tmux prefix f).
        table.insert(Key::char('f'), Action::FindWindow);
        // Paste buffers (tmux prefix ] pastes the top buffer; = opens the chooser).
        table.insert(Key::char(']'), Action::PasteBuffer);
        table.insert(Key::char('='), Action::ChooseBuffer);
        // Prefixed directional pane selection (tmux default arrow bindings).
        table.insert(Key::plain(KeyCode::Left), Action::SelectPaneLeft);
        table.insert(Key::plain(KeyCode::Right), Action::SelectPaneRight);
        table.insert(Key::plain(KeyCode::Up), Action::SelectPaneUp);
        table.insert(Key::plain(KeyCode::Down), Action::SelectPaneDown);
        // Zoom the active pane (tmux prefix z).
        table.insert(Key::char('z'), Action::ZoomPane);
        // Break the active pane to a new window; swap panes (tmux !, {, }).
        table.insert(Key::char('!'), Action::BreakPane);
        table.insert(Key::char('{'), Action::SwapPanePrev);
        table.insert(Key::char('}'), Action::SwapPaneNext);
        // Cycle preset layouts (tmux prefix Space / next-layout).
        table.insert(Key::plain(KeyCode::Space), Action::NextLayout);
        // Directional resize on the real tmux keys: Ctrl+arrows and Alt+arrows
        // (tmux's repeatable resize-pane bindings; plain arrows are select-pane
        // above). lumux resizes by a fixed ratio per press, so Ctrl and Alt map
        // to the same step rather than tmux's 1-vs-5 cell amounts.
        for (ctrl, alt) in [(true, false), (false, true)] {
            table.insert(Key::modified(KeyCode::Left, ctrl, alt), Action::ResizePaneLeft);
            table.insert(Key::modified(KeyCode::Right, ctrl, alt), Action::ResizePaneRight);
            table.insert(Key::modified(KeyCode::Up, ctrl, alt), Action::ResizePaneUp);
            table.insert(Key::modified(KeyCode::Down, ctrl, alt), Action::ResizePaneDown);
        }
        // Rename prompts (tmux prefix , and $).
        table.insert(Key::char(','), Action::RenameWindow);
        table.insert(Key::char('$'), Action::RenameSession);
        for n in 0..=9u32 {
            table.insert(
                Key::char(char::from_digit(n, 10).unwrap()),
                Action::SelectWindow(n),
            );
        }
        // Pressing the prefix again sends a literal prefix.
        let prefix = Key::ctrl('b');
        table.insert(prefix, Action::SendPrefix);
        Self {
            prefix,
            table,
            root: HashMap::new(),
        }
    }

    /// Bind a key in the root table (fires without the prefix; tmux `bind -n`).
    pub fn bind_root(&mut self, key: Key, action: Action) {
        self.root.insert(key, action);
    }

    /// Look up a root (no-prefix) binding.
    pub fn lookup_root(&self, key: &Key) -> Option<&Action> {
        self.root.get(key)
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

    /// Enumerate bindings for the help overlay as (binding-label, description),
    /// sorted by label. Prefix bindings are shown as "<prefix> <key>"; root
    /// bindings as the bare key (they fire without the prefix). The literal
    /// send-prefix entry is omitted as noise.
    pub fn help_entries(&self) -> Vec<(String, &'static str)> {
        let prefix = self.prefix.to_string();
        let mut entries: Vec<(String, &'static str)> = Vec::new();
        for (key, action) in &self.table {
            if matches!(action, Action::SendPrefix) {
                continue;
            }
            entries.push((format!("{prefix} {key}"), action.describe()));
        }
        for (key, action) in &self.root {
            entries.push((format!("{key}"), action.describe()));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
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
        // tmux's true split keys.
        assert_eq!(b.lookup(&Key::char('"')), Some(&Action::SplitVertical));
        assert_eq!(b.lookup(&Key::char('%')), Some(&Action::SplitHorizontal));
        assert_eq!(b.lookup(&Key::char('c')), Some(&Action::NewWindow));
        assert_eq!(b.lookup(&Key::char('d')), Some(&Action::Detach));
        assert_eq!(b.lookup(&Key::char('3')), Some(&Action::SelectWindow(3)));
    }

    #[test]
    fn resize_bound_on_tmux_arrow_keys_not_capitals() {
        let b = Bindings::default();
        // tmux resize-pane keys: Ctrl+arrows and Alt+arrows.
        assert_eq!(
            b.lookup(&Key::modified(KeyCode::Left, true, false)),
            Some(&Action::ResizePaneLeft)
        );
        assert_eq!(
            b.lookup(&Key::modified(KeyCode::Up, false, true)),
            Some(&Action::ResizePaneUp)
        );
        // The old invented H/J/K/L bindings are gone.
        assert_eq!(b.lookup(&Key::char('H')), None);
        assert_eq!(b.lookup(&Key::char('J')), None);
        // Plain arrows stay select-pane, not resize.
        assert_eq!(
            b.lookup(&Key::plain(KeyCode::Left)),
            Some(&Action::SelectPaneLeft)
        );
    }

    #[test]
    fn non_tmux_defaults_are_unbound() {
        let b = Bindings::default();
        // tmux has no |, -, or r(reload) defaults.
        assert_eq!(b.lookup(&Key::char('|')), None);
        assert_eq!(b.lookup(&Key::char('-')), None);
        assert_eq!(b.lookup(&Key::char('r')), None);
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
