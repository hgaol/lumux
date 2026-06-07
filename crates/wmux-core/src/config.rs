//! Configuration: prefix key, key bindings, shell profiles, status bar, and
//! scrollback, loaded from TOML.
//!
//! The config is plain data that the daemon applies to its keymap and pane
//! spawning. It is hot-reloadable: `wmux source-file` re-parses and the daemon
//! rebinds every attached client's keymap live.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::keymap::{Action, Bindings, Key, KeyCode};

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
    /// Status bar format string (supports #S session, #W window, #H host).
    pub status_format: String,
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
            status_format: "[#S] #W".to_string(),
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
        let prefix = parse_key(&self.prefix).ok_or_else(|| ConfigError::BadKey(self.prefix.clone()))?;
        b.set_prefix(prefix);
        for (key_str, action_str) in &self.bindings {
            let key = parse_key(key_str).ok_or_else(|| ConfigError::BadKey(key_str.clone()))?;
            let action =
                parse_action(action_str).ok_or_else(|| ConfigError::BadAction(action_str.clone()))?;
            b.bind(key, action);
        }
        Ok(b)
    }
}

/// Parse a key spec like "C-b", "M-x", "|", "c", "Up", "Enter".
pub fn parse_key(s: &str) -> Option<Key> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("C-") {
        let c = single_char(rest)?;
        return Some(Key::ctrl(c));
    }
    if let Some(rest) = s.strip_prefix("M-") {
        let c = single_char(rest)?;
        return Some(Key {
            code: KeyCode::Char(c),
            ctrl: false,
            alt: true,
        });
    }
    match s {
        "Up" => Some(Key::plain(KeyCode::Up)),
        "Down" => Some(Key::plain(KeyCode::Down)),
        "Left" => Some(Key::plain(KeyCode::Left)),
        "Right" => Some(Key::plain(KeyCode::Right)),
        "Enter" => Some(Key::plain(KeyCode::Enter)),
        "Space" => Some(Key::plain(KeyCode::Space)),
        "Escape" => Some(Key::plain(KeyCode::Escape)),
        _ => single_char(s).map(Key::char),
    }
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
        "detach" => Action::Detach,
        "copy-mode" => Action::EnterCopyMode,
        "kill-pane" => Action::KillPane,
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
}
