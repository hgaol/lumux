//! Configuration: prefix key, key bindings, shell profiles, status bar, and
//! scrollback. Loaded from TOML (the native format) or from tmux config syntax
//! (see [`tmux`]), so an existing `~/.tmux.conf` can be used directly.
//!
//! The config is plain data that the daemon applies to its keymap and pane
//! spawning. It is hot-reloadable: `lumux source-file` re-parses and the daemon
//! rebinds every attached client's keymap live.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::keymap::{Action, Bindings, Key, KeyCode};

pub mod tmux;

pub use tmux::TmuxParse;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Prefix key, e.g. "C-b" or "C-a".
    pub prefix: String,
    /// Lines of scrollback to retain per pane.
    pub scrollback: usize,
    /// Default shell profile name (must exist in `shells`).
    pub default_shell: Option<String>,
    /// Named shell profiles.
    pub shells: Vec<ShellProfile>,
    /// Extra key bindings: key string -> action name.
    pub bindings: BTreeMap<String, String>,
    /// Root (no-prefix) key bindings (tmux `bind -n`): key -> action name.
    pub root_bindings: BTreeMap<String, String>,
    /// Status bar format string (supports #S session, #W window, #H host).
    pub status_format: String,
    /// Lowest window/pane index shown to the user (tmux base-index). tmux's
    /// `base-index 1` makes numbering start at 1 instead of 0.
    pub base_index: u32,
    /// Whether the mouse is enabled (click/scroll/drag).
    pub mouse: bool,
    /// Whether the client polls its terminal size and tells the daemon when it
    /// changes. On by default. Needed especially over SSH, where the size at
    /// attach is often stale (sshd's ConPTY negotiates it asynchronously) and a
    /// later window resize would otherwise leave the grid at the wrong width,
    /// shifting columns. Disable only if the size polling is unwanted.
    pub auto_resize: bool,
    /// status-left format (left segment of the status bar).
    pub status_left: String,
    /// status-right format (right segment).
    pub status_right: String,
    /// Status justification: "left", "centre"/"center", or "right".
    pub status_justify: String,
    /// Status bar background/foreground as tmux color names or indices.
    pub status_bg: String,
    pub status_fg: String,
    /// Foreground color of the active pane's border (tmux pane-active-border).
    /// A tmux color name or `colourN`. Empty disables the highlight.
    pub pane_active_border_fg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellProfile {
    pub name: String,
    pub argv: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: "C-b".to_string(),
            scrollback: 2000,
            default_shell: None,
            shells: Vec::new(),
            bindings: BTreeMap::new(),
            root_bindings: BTreeMap::new(),
            status_format: "[#S] #W".to_string(),
            base_index: 0,
            mouse: false,
            auto_resize: true,
            // A tmux-like default look out of the box (green bar, session name on
            // the left, window list centred, a clock on the right) so a fresh
            // install without a config file still looks good.
            status_left: "#[bg=green,fg=black,bold] #S #[bg=colour236,fg=green] ".to_string(),
            status_right: "#[bg=colour236,fg=cyan] %H:%M #[bg=green,fg=black,bold] %d-%b ".to_string(),
            status_justify: "centre".to_string(),
            status_bg: "colour236".to_string(),
            status_fg: "white".to_string(),
            // tmux's default: the active pane gets a green border.
            pane_active_border_fg: "green".to_string(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Parse(String),
    BadKey(String),
    BadAction(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(e) => write!(f, "config parse error: {e}"),
            ConfigError::BadKey(k) => write!(f, "invalid key spec: {k}"),
            ConfigError::BadAction(a) => write!(f, "unknown action: {a}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Parse config from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// The centre segment format. When status is centre-justified (tmux shows
    /// the window list there), use the window-name token; otherwise empty.
    pub fn status_format_centre(&self) -> String {
        if self.status_justify == "centre" || self.status_justify == "center" {
            "#W".to_string()
        } else {
            String::new()
        }
    }

    /// Resolve the shell argv for a profile name (or the default).
    pub fn shell_argv(&self, name: Option<&str>) -> Option<Vec<String>> {
        let target = name.or(self.default_shell.as_deref());
        match target {
            Some(n) => self
                .shells
                .iter()
                .find(|p| p.name == n)
                .map(|p| p.argv.clone()),
            None => None,
        }
    }

    /// Build a [`Bindings`] table from this config (prefix + custom bindings),
    /// starting from the tmux defaults.
    pub fn to_bindings(&self) -> Result<Bindings, ConfigError> {
        let mut b = Bindings::tmux_defaults();
        let prefix =
            parse_key(&self.prefix).ok_or_else(|| ConfigError::BadKey(self.prefix.clone()))?;
        b.set_prefix(prefix);
        for (key_str, action_str) in &self.bindings {
            let key = parse_key(key_str).ok_or_else(|| ConfigError::BadKey(key_str.clone()))?;
            let action = parse_action(action_str)
                .ok_or_else(|| ConfigError::BadAction(action_str.clone()))?;
            b.bind(key, action);
        }
        for (key_str, action_str) in &self.root_bindings {
            let key = parse_key(key_str).ok_or_else(|| ConfigError::BadKey(key_str.clone()))?;
            let action = parse_action(action_str)
                .ok_or_else(|| ConfigError::BadAction(action_str.clone()))?;
            b.bind_root(key, action);
        }
        Ok(b)
    }
}

/// Parse a key spec like "C-b", "M-x", "M-Left", "|", "c", "Up", "Enter".
pub fn parse_key(s: &str) -> Option<Key> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("C-") {
        let c = single_char(rest)?;
        return Some(Key::ctrl(c));
    }
    if let Some(rest) = s.strip_prefix("M-") {
        // Alt + a named key (M-Left) or a single char (M-x).
        if let Some(code) = named_key(rest) {
            return Some(Key {
                code,
                ctrl: false,
                alt: true,
            });
        }
        let c = single_char(rest)?;
        return Some(Key {
            code: KeyCode::Char(c),
            ctrl: false,
            alt: true,
        });
    }
    if let Some(code) = named_key(s) {
        return Some(Key::plain(code));
    }
    single_char(s).map(Key::char)
}

/// Map a named key string to a KeyCode, or None for non-named keys.
fn named_key(s: &str) -> Option<KeyCode> {
    Some(match s {
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Enter" => KeyCode::Enter,
        "Space" => KeyCode::Space,
        "Escape" => KeyCode::Escape,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        _ => return None,
    })
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_none() {
        Some(c)
    } else {
        None
    }
}

/// Map an action name to an [`Action`].
pub fn parse_action(s: &str) -> Option<Action> {
    let action = match s {
        "split-horizontal" => Action::SplitHorizontal,
        "split-vertical" => Action::SplitVertical,
        "new-window" => Action::NewWindow,
        "next-window" => Action::NextWindow,
        "prev-window" => Action::PrevWindow,
        "last-window" => Action::LastWindow,
        "kill-window" => Action::KillWindow,
        "last-pane" => Action::LastPane,
        "select-pane-left" => Action::SelectPaneLeft,
        "select-pane-right" => Action::SelectPaneRight,
        "select-pane-up" => Action::SelectPaneUp,
        "select-pane-down" => Action::SelectPaneDown,
        "resize-pane-left" => Action::ResizePaneLeft,
        "resize-pane-right" => Action::ResizePaneRight,
        "resize-pane-up" => Action::ResizePaneUp,
        "resize-pane-down" => Action::ResizePaneDown,
        "zoom-pane" => Action::ZoomPane,
        "next-layout" => Action::NextLayout,
        "rename-window" => Action::RenameWindow,
        "rename-session" => Action::RenameSession,
        "detach" => Action::Detach,
        "copy-mode" => Action::EnterCopyMode,
        "kill-pane" => Action::KillPane,
        "reload-config" => Action::ReloadConfig,
        "show-help" => Action::ShowHelp,
        "choose-session" => Action::ChooseSession,
        _ => return None,
    };
    Some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_empty() {
        let c = Config::from_toml("").unwrap();
        assert_eq!(c.prefix, "C-b");
        assert_eq!(c.scrollback, 2000);
    }

    #[test]
    fn default_status_is_styled_out_of_the_box() {
        use crate::status::{self, StatusContext};
        use termwiz::color::ColorAttribute;
        let c = Config::default();
        // A fresh install should have a non-empty, styled status bar (not blank).
        assert!(!c.status_left.is_empty(), "default status-left should be set");
        assert!(!c.status_right.is_empty(), "default status-right should be set");
        assert_eq!(c.status_justify, "centre");

        // The left segment formats to spans containing the session name on a
        // green background (the tmux-like look).
        let ctx = StatusContext {
            session: "work".into(),
            ..Default::default()
        };
        let spans = status::format(&c.status_left, &ctx);
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("work"), "left segment shows the session name");
        let green = ColorAttribute::PaletteIndex(2);
        assert!(
            spans.iter().any(|s| s.attrs.background() == green),
            "default status should use a green background span"
        );

        // The right segment carries a clock token that formats to digits.
        let clock = status::format(&c.status_right, &ctx);
        let ctext: String = clock.iter().map(|s| s.text.as_str()).collect();
        assert!(ctext.contains(':'), "right segment has a HH:MM clock; got {ctext:?}");
    }

    #[test]
    fn full_config_roundtrips() {
        let toml = r#"
prefix = "C-a"
scrollback = 5000
default_shell = "ps5"

[[shells]]
name = "ps5"
argv = ["powershell.exe", "-NoLogo"]

[[shells]]
name = "cmd"
argv = ["cmd.exe"]

[bindings]
"C-s" = "split-vertical"
"#;
        let c = Config::from_toml(toml).unwrap();
        assert_eq!(c.prefix, "C-a");
        assert_eq!(c.scrollback, 5000);
        assert_eq!(c.default_shell.as_deref(), Some("ps5"));
        assert_eq!(
            c.shell_argv(Some("ps5")),
            Some(vec!["powershell.exe".to_string(), "-NoLogo".to_string()])
        );
        assert_eq!(c.shell_argv(None), c.shell_argv(Some("ps5")));
        assert_eq!(c.shell_argv(Some("cmd")), Some(vec!["cmd.exe".to_string()]));
    }

    #[test]
    fn bindings_apply_prefix_and_custom() {
        let toml = r#"
prefix = "C-a"
[bindings]
"C-s" = "split-vertical"
"#;
        let c = Config::from_toml(toml).unwrap();
        let b = c.to_bindings().unwrap();
        assert!(b.is_prefix(&Key::ctrl('a')));
        assert_eq!(b.lookup(&Key::ctrl('s')), Some(&Action::SplitVertical));
        // tmux defaults still present.
        assert_eq!(b.lookup(&Key::char('c')), Some(&Action::NewWindow));
    }

    #[test]
    fn bad_action_rejected() {
        let toml = r#"
[bindings]
"x" = "frobnicate"
"#;
        let c = Config::from_toml(toml).unwrap();
        assert!(matches!(c.to_bindings(), Err(ConfigError::BadAction(_))));
    }

    #[test]
    fn parse_key_specs() {
        assert_eq!(parse_key("C-b"), Some(Key::ctrl('b')));
        assert_eq!(parse_key("|"), Some(Key::char('|')));
        assert_eq!(parse_key("Up"), Some(Key::plain(KeyCode::Up)));
        assert!(parse_key("C-").is_none());
        let m = parse_key("M-x").unwrap();
        assert!(m.alt);
    }

    #[test]
    fn invalid_shell_profile_returns_none() {
        let c = Config::default();
        assert_eq!(c.shell_argv(Some("nonexistent")), None);
    }

    #[test]
    fn parses_base_index_and_mouse() {
        let c = Config::from_toml("base_index = 1\nmouse = true\nscrollback = 10000\n").unwrap();
        assert_eq!(c.base_index, 1);
        assert!(c.mouse);
        assert_eq!(c.scrollback, 10000);
    }

    #[test]
    fn auto_resize_defaults_on_and_can_be_disabled() {
        // Default (empty config) must enable auto-resize.
        assert!(Config::from_toml("").unwrap().auto_resize);
        assert!(Config::default().auto_resize);
        // Explicit opt-out via TOML.
        let c = Config::from_toml("auto_resize = false\n").unwrap();
        assert!(!c.auto_resize);
    }
}
