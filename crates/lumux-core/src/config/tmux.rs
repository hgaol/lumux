//! Parse a subset of tmux config syntax into a lumux [`Config`].
//!
//! This lets users drop their `~/.tmux.conf` in as `~/.lumux.conf` (or
//! `%APPDATA%\lumux\lumux.conf`) without translating to TOML. We recognize the
//! tmux options and bindings that map onto features lumux actually has; every
//! other directive is skipped with a warning (collected in [`TmuxParse::warnings`])
//! so a full real-world tmux.conf loads and the supported parts take effect.
//!
//! Supported directives:
//! - `set[-option] [-g|-s|-w] <option> <value>` and `setw [-g] <option> <value>`
//!   for: prefix, mouse, history-limit, base-index, pane-base-index,
//!   default-shell, default-command, status-justify, status-left, status-right,
//!   status-style / status-bg / status-fg.
//! - `bind[-key] [-n|-T root] <key> <command> [args…]` and `unbind[-key]`.
//!
//! Line continuations (`\` at end of line), `#` comments, and quoted arguments
//! (single or double) are handled. Commands joined with `\;` (tmux's command
//! separator) keep only the first command on the line — enough for the common
//! `bind r source-file … \; display "…"` pattern.

use super::{Config, ConfigError, ShellProfile};

/// Result of parsing a tmux-syntax config: the [`Config`] plus any directives
/// that were recognized as tmux commands but not applied (for logging).
pub struct TmuxParse {
    pub config: Config,
    pub warnings: Vec<String>,
}

impl Config {
    /// Parse tmux config syntax into a [`Config`]. Unsupported directives are
    /// collected as warnings rather than failing, so a real `~/.tmux.conf`
    /// loads. Returns an error only on a malformed directive we *do* support
    /// (e.g. a bad key spec in a `bind`).
    pub fn from_tmux(s: &str) -> Result<Self, ConfigError> {
        Self::from_tmux_verbose(s).map(|p| p.config)
    }

    /// Like [`from_tmux`] but also returns the list of skipped/unsupported
    /// directives so the loader can log them.
    pub fn from_tmux_verbose(s: &str) -> Result<TmuxParse, ConfigError> {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();

        for raw_line in logical_lines(s) {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Only act on the first sub-command of a `\;`-joined line.
            let line = line.split(" \\; ").next().unwrap_or(line).trim();
            let tokens = match tokenize(line) {
                Some(t) if !t.is_empty() => t,
                _ => continue,
            };
            apply_directive(&mut cfg, &tokens, &mut warnings)?;
        }
        Ok(TmuxParse {
            config: cfg,
            warnings,
        })
    }
}

/// Join physical lines on a trailing backslash into logical lines.
fn logical_lines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for line in s.lines() {
        let trimmed_end = line.trim_end();
        if let Some(prefix) = trimmed_end.strip_suffix('\\') {
            acc.push_str(prefix);
            acc.push(' ');
        } else {
            acc.push_str(line);
            out.push(std::mem::take(&mut acc));
        }
    }
    if !acc.is_empty() {
        out.push(acc);
    }
    out
}

/// Split a line into tokens, honoring single/double quotes. A `#` outside quotes
/// starts a trailing comment. Returns None if quotes are unbalanced.
fn tokenize(line: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false; // have we begun the current token?
    for c in line.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '#' if !started && cur.is_empty() => break, // whole-token comment
                c if c.is_whitespace() => {
                    if started || !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None; // unbalanced
    }
    if started || !cur.is_empty() {
        tokens.push(cur);
    }
    Some(tokens)
}

/// Apply one tokenized directive to the config.
fn apply_directive(
    cfg: &mut Config,
    tokens: &[String],
    warnings: &mut Vec<String>,
) -> Result<(), ConfigError> {
    let cmd = tokens[0].as_str();
    match cmd {
        "set" | "set-option" | "setw" | "set-window-option" | "setenv" | "set-environment" => {
            apply_set(cfg, &tokens[1..], warnings);
            Ok(())
        }
        "bind" | "bind-key" => apply_bind(cfg, &tokens[1..], warnings),
        "unbind" | "unbind-key" => {
            // lumux starts from tmux defaults and only adds bindings; we can't
            // easily remove a default here, so note it and move on.
            warnings.push(format!("unbind not applied: {}", tokens.join(" ")));
            Ok(())
        }
        other => {
            warnings.push(format!("unsupported command: {other}"));
            Ok(())
        }
    }
}

/// Handle `set`/`setw` after the command word: skip flags, then option + value.
fn apply_set(cfg: &mut Config, rest: &[String], warnings: &mut Vec<String>) {
    // Drop leading flags like -g, -s, -w, -ga, -gq, -u (we treat all scopes the
    // same — lumux has a single global config).
    let mut i = 0;
    while i < rest.len() && rest[i].starts_with('-') {
        i += 1;
    }
    let Some(option) = rest.get(i) else {
        return;
    };
    let value = rest.get(i + 1).map(|s| s.as_str()).unwrap_or("");
    match option.as_str() {
        "prefix" => cfg.prefix = value.to_string(),
        "mouse" => cfg.mouse = on_off(value),
        "history-limit" => {
            if let Ok(n) = value.parse() {
                cfg.scrollback = n;
            }
        }
        "base-index" | "pane-base-index" => {
            if let Ok(n) = value.parse() {
                cfg.base_index = n;
            }
        }
        // tmux expresses the startup shell two ways; both become lumux's default
        // shell. default-command is a shell command line; default-shell is a path.
        "default-shell" | "default-command" => {
            set_default_shell(cfg, value);
        }
        "status-justify" => cfg.status_justify = value.to_string(),
        "status-left" => cfg.status_left = value.to_string(),
        "status-right" => cfg.status_right = value.to_string(),
        "status-bg" => cfg.status_bg = value.to_string(),
        "status-fg" => cfg.status_fg = value.to_string(),
        "status-style" => apply_style_pairs(cfg, value),
        // Known-but-irrelevant to lumux: silently accept the common ones so a
        // typical tmux.conf doesn't spew warnings for things that are simply the
        // default behavior in lumux.
        "mode-keys" | "status-keys" | "escape-time" | "status" | "status-interval"
        | "renumber-windows" | "set-titles" | "status-left-length" | "status-right-length"
        | "aggressive-resize" | "default-terminal" | "focus-events" => {}
        other => warnings.push(format!("unsupported option: {other}")),
    }
}

/// Set the default shell from a tmux `default-shell`/`default-command` value by
/// synthesizing a shell profile (lumux's `default_shell` names a profile in
/// `shells`). The value is split into argv on spaces (outside the quoting the
/// tokenizer already removed), so `default-command "powershell -NoLogo"` works.
fn set_default_shell(cfg: &mut Config, value: &str) {
    let argv: Vec<String> = value.split_whitespace().map(|s| s.to_string()).collect();
    if argv.is_empty() {
        return;
    }
    const NAME: &str = "__tmux_default";
    cfg.shells.retain(|p| p.name != NAME);
    cfg.shells.push(ShellProfile {
        name: NAME.to_string(),
        argv,
    });
    cfg.default_shell = Some(NAME.to_string());
}

/// Apply `key=value` / flag style pairs from a tmux `*-style` option (we only
/// pull out fg/bg).
fn apply_style_pairs(cfg: &mut Config, value: &str) {
    for part in value.split(',') {
        let part = part.trim();
        if let Some(c) = part.strip_prefix("bg=") {
            cfg.status_bg = c.to_string();
        } else if let Some(c) = part.strip_prefix("fg=") {
            cfg.status_fg = c.to_string();
        }
    }
}

/// Handle `bind` after the command word: parse flags (-n / -T root for root
/// bindings), the key, and the tmux command, mapping it to a lumux action.
fn apply_bind(
    cfg: &mut Config,
    rest: &[String],
    warnings: &mut Vec<String>,
) -> Result<(), ConfigError> {
    let mut i = 0;
    let mut root = false;
    // Flags: -n (root table), -r (repeatable, ignored), -T <table>.
    while i < rest.len() && rest[i].starts_with('-') {
        match rest[i].as_str() {
            "-n" => root = true,
            "-T" => {
                // -T root means the root (no-prefix) table; other tables ignored.
                if rest.get(i + 1).map(|s| s.as_str()) == Some("root") {
                    root = true;
                }
                i += 1;
            }
            _ => {} // -r, -N, etc.: ignore
        }
        i += 1;
    }
    let Some(key) = rest.get(i) else {
        return Ok(());
    };
    let tmux_cmd = &rest[i + 1..];
    let Some(action) = map_bind_command(tmux_cmd) else {
        warnings.push(format!("unsupported binding command: {}", tmux_cmd.join(" ")));
        return Ok(());
    };
    // Validate the key now so a bad spec surfaces as an error (consistent with
    // the TOML path's to_bindings validation).
    if super::parse_key(key).is_none() {
        return Err(ConfigError::BadKey(key.clone()));
    }
    let table = if root {
        &mut cfg.root_bindings
    } else {
        &mut cfg.bindings
    };
    table.insert(key.clone(), action);
    Ok(())
}

/// Map a tmux binding command (e.g. `split-window -h`) to a lumux action name
/// (the same names the TOML `[bindings]` table uses). None if unsupported.
fn map_bind_command(cmd: &[String]) -> Option<String> {
    let head = cmd.first()?.as_str();
    // Collect the flags present (e.g. -h, -v) for split-window.
    let has = |flag: &str| cmd.iter().any(|a| a == flag);
    let action = match head {
        "split-window" | "split-pane" => {
            if has("-h") {
                "split-horizontal"
            } else {
                "split-vertical"
            }
        }
        "new-window" => "new-window",
        "next-window" => "next-window",
        "previous-window" => "prev-window",
        "kill-pane" => "kill-pane",
        "detach-client" => "detach",
        "copy-mode" => "copy-mode",
        "source-file" => "reload-config",
        "choose-tree" | "choose-session" => "choose-session",
        "command-prompt" => {
            // `command-prompt -I "#W" "rename-window '%%'"` etc. — map the common
            // rename prompts; otherwise unsupported.
            let joined = cmd.join(" ");
            if joined.contains("rename-window") {
                "rename-window"
            } else if joined.contains("rename-session") {
                "rename-session"
            } else {
                return None;
            }
        }
        "rename-window" => "rename-window",
        "rename-session" => "rename-session",
        "resize-pane" => {
            if has("-L") {
                "resize-pane-left"
            } else if has("-R") {
                "resize-pane-right"
            } else if has("-U") {
                "resize-pane-up"
            } else if has("-D") {
                "resize-pane-down"
            } else {
                return None;
            }
        }
        "select-pane" => {
            if has("-L") {
                "select-pane-left"
            } else if has("-R") {
                "select-pane-right"
            } else if has("-U") {
                "select-pane-up"
            } else if has("-D") {
                "select-pane-down"
            } else {
                return None;
            }
        }
        "resize-pane-zoom" => "zoom-pane",
        // `bind z resize-pane -Z` (zoom toggle).
        _ if head == "resize-pane" && has("-Z") => "zoom-pane",
        _ => return None,
    };
    Some(action.to_string())
}

/// Parse a tmux on/off/toggle value; anything other than off-ish is "on".
fn on_off(v: &str) -> bool {
    !matches!(v, "off" | "0" | "false" | "no")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_options() {
        let conf = r#"
            # my tmux.conf
            set -g prefix C-a
            set -g mouse on
            set -g history-limit 5000
            set -g base-index 1
            setw -g pane-base-index 1
        "#;
        let c = Config::from_tmux(conf).unwrap();
        assert_eq!(c.prefix, "C-a");
        assert!(c.mouse);
        assert_eq!(c.scrollback, 5000);
        assert_eq!(c.base_index, 1);
    }

    #[test]
    fn default_shell_becomes_a_profile() {
        let c = Config::from_tmux("set -g default-shell powershell.exe").unwrap();
        assert_eq!(c.shell_argv(None), Some(vec!["powershell.exe".to_string()]));
    }

    #[test]
    fn default_command_with_args_splits_argv() {
        let c = Config::from_tmux(r#"set -g default-command "powershell.exe -NoLogo""#).unwrap();
        assert_eq!(
            c.shell_argv(None),
            Some(vec!["powershell.exe".to_string(), "-NoLogo".to_string()])
        );
    }

    #[test]
    fn binds_map_to_actions() {
        let conf = r#"
            bind | split-window -h
            bind - split-window -v
            bind -n M-Left select-pane -L
            bind r source-file ~/.tmux.conf \; display "reloaded"
        "#;
        let c = Config::from_tmux(conf).unwrap();
        let b = c.to_bindings().unwrap();
        use crate::keymap::{Action, Key, KeyCode};
        assert_eq!(b.lookup(&Key::char('|')), Some(&Action::SplitHorizontal));
        assert_eq!(b.lookup(&Key::char('-')), Some(&Action::SplitVertical));
        assert_eq!(b.lookup(&Key::char('r')), Some(&Action::ReloadConfig));
        assert_eq!(
            b.lookup_root(&Key {
                code: KeyCode::Left,
                ctrl: false,
                alt: true
            }),
            Some(&Action::SelectPaneLeft)
        );
    }

    #[test]
    fn status_style_and_segments() {
        let conf = r##"
            set -g status-justify centre
            set -g status-style bg=colour24,fg=white
            set -g status-left "#[fg=green] #S "
            set -g status-right "%H:%M"
        "##;
        let c = Config::from_tmux(conf).unwrap();
        assert_eq!(c.status_justify, "centre");
        assert_eq!(c.status_bg, "colour24");
        assert_eq!(c.status_fg, "white");
        assert_eq!(c.status_left, "#[fg=green] #S ");
        assert_eq!(c.status_right, "%H:%M");
    }

    #[test]
    fn unsupported_directives_warn_not_fail() {
        let conf = r#"
            set -g prefix C-a
            set -g renumber-windows on
            setw -g monitor-activity on
            bind C-l send-keys C-l
            set -g some-future-option 42
        "#;
        let p = Config::from_tmux_verbose(conf).unwrap();
        assert_eq!(p.config.prefix, "C-a");
        // The unknown option and the unsupported binding are warned about.
        assert!(p.warnings.iter().any(|w| w.contains("monitor-activity")));
        assert!(!p.warnings.is_empty());
    }

    #[test]
    fn comments_and_quotes() {
        let conf = r#"
            set -g status-left "a # not a comment"   # trailing comment
            set -g prefix C-b
        "#;
        let c = Config::from_tmux(conf).unwrap();
        assert_eq!(c.status_left, "a # not a comment");
        assert_eq!(c.prefix, "C-b");
    }

    #[test]
    fn bad_key_in_bind_is_error() {
        // "C-" is an invalid key spec; the supported-command path should error.
        let err = Config::from_tmux("bind C- split-window -h");
        assert!(matches!(err, Err(ConfigError::BadKey(_))));
    }

    #[test]
    fn shipped_example_conf_parses_and_binds() {
        // The example tmux-syntax config must always load and produce the
        // expected bindings (guards the example against drift).
        let conf = include_str!("../../../../examples/lumux.conf");
        let p = Config::from_tmux_verbose(conf).expect("example lumux.conf parses");
        let c = &p.config;
        assert_eq!(c.prefix, "C-b");
        assert!(c.mouse);
        assert_eq!(c.scrollback, 10000);
        assert_eq!(c.base_index, 1);
        assert_eq!(c.status_justify, "centre");
        assert_eq!(c.status_bg, "colour24");
        // default-shell becomes the resolved default argv.
        assert_eq!(c.shell_argv(None), Some(vec!["powershell.exe".to_string()]));
        // Bindings compile, including the splits, zoom, and root nav.
        let b = c.to_bindings().expect("bindings build");
        use crate::keymap::{Action, Key, KeyCode};
        assert_eq!(b.lookup(&Key::char('|')), Some(&Action::SplitHorizontal));
        assert_eq!(b.lookup(&Key::char('-')), Some(&Action::SplitVertical));
        assert_eq!(b.lookup(&Key::char('z')), Some(&Action::ZoomPane));
        assert_eq!(b.lookup(&Key::char('r')), Some(&Action::ReloadConfig));
        assert_eq!(
            b.lookup_root(&Key {
                code: KeyCode::Right,
                ctrl: false,
                alt: true
            }),
            Some(&Action::SelectPaneRight)
        );
    }
}
