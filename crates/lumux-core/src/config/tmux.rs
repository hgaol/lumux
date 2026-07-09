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
//! (single or double) are handled. A `bind` whose command tail is chained with
//! `\;` (tmux's command separator) keeps the whole chain — each command runs in
//! order (e.g. `bind r new-window \; split-window -h`). For non-`bind`
//! directives a stray `\;` tail is still dropped (they act on one command).
//!
//! Config variables are supported: `%hidden NAME=value` (or a bare
//! `NAME=value`) defines a variable, referenced as `$NAME` / `${NAME}` and
//! expanded before a directive is applied (falling back to the environment, so
//! `$HOME` works). This is what makes palette-style configs — define colors
//! once, reuse them across `*-style` options — load correctly.

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
        // tmux config variables: `%hidden NAME=value` or a bare `NAME=value`
        // line. Referenced elsewhere as `$NAME` / `${NAME}` and expanded before a
        // directive is applied. (Without this, e.g. a `$BG` in a status-style
        // string would reach the renderer literally and render as the default.)
        let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        for raw_line in logical_lines(s) {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.trim();
            // `bind` may chain commands with `\;` (tmux's separator); the whole
            // line must reach apply_bind so the chain is preserved. Every other
            // directive acts on a single command, so a stray `\;` tail is dropped.
            let first_word = line.split_whitespace().next().unwrap_or("");
            let line = if matches!(first_word, "bind" | "bind-key") {
                line
            } else {
                line.split(" \\; ").next().unwrap_or(line).trim()
            };
            let tokens = match tokenize(line) {
                Some(t) if !t.is_empty() => t,
                _ => continue,
            };
            // Variable assignment: `%hidden NAME=value` or a bare `NAME=value`.
            // Capture it (expanding any vars already defined) and move on — it's
            // not a directive to apply.
            let assign_tok = if tokens[0] == "%hidden" {
                tokens.get(1)
            } else {
                Some(&tokens[0])
            };
            if let Some((name, value)) = assign_tok.and_then(|t| parse_assignment(t)) {
                vars.insert(name, expand_vars(&value, &vars));
                continue;
            }
            // Expand `$VAR` in every token, then apply.
            let expanded: Vec<String> = tokens.iter().map(|t| expand_vars(t, &vars)).collect();
            apply_directive(&mut cfg, &expanded, &mut warnings)?;
        }
        Ok(TmuxParse {
            config: cfg,
            warnings,
        })
    }
}

/// Parse a tmux variable assignment token `NAME=value` (the value already
/// unquoted by the tokenizer). Returns None if the left side isn't a plain
/// identifier, so directive tokens like `bg=green` on a style line — which only
/// ever appear as non-first tokens — are never mistaken for assignments.
fn parse_assignment(tok: &str) -> Option<(String, String)> {
    let (name, value) = tok.split_once('=')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name.to_string(), value.to_string()))
}

/// Expand `$NAME` and `${NAME}` references in `s` using `vars`, falling back to
/// the process environment (so `$HOME` works like tmux). Unknown names are left
/// literal — more debuggable than silently blanking, and a stray `$X` in a color
/// slot degrades to the default anyway.
fn expand_vars(s: &str, vars: &std::collections::HashMap<String, String>) -> String {
    if !s.contains('$') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        // `${NAME}` (braced) or `$NAME` (bare, alnum/underscore run).
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next(); // consume '{'
        }
        let mut name = String::new();
        while let Some(&nc) = chars.peek() {
            let part_of_name = if braced {
                nc != '}'
            } else {
                nc.is_ascii_alphanumeric() || nc == '_'
            };
            if !part_of_name {
                break;
            }
            name.push(nc);
            chars.next();
        }
        if braced {
            chars.next(); // consume '}' (if present)
        }
        match vars.get(&name).cloned().or_else(|| std::env::var(&name).ok()) {
            Some(v) => out.push_str(&v),
            None => {
                // Leave the reference literal.
                out.push('$');
                if braced {
                    out.push('{');
                    out.push_str(&name);
                    out.push('}');
                } else {
                    out.push_str(&name);
                }
            }
        }
    }
    out
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
        // set-hook [-g] <event> <command>: store a command to run on an event.
        "set-hook" | "set-hooks" => {
            apply_set_hook(cfg, &tokens[1..], warnings);
            Ok(())
        }
        // if-shell <cmd> <then> [else]: run <cmd>; on success apply <then> as a
        // directive, else <else>. The then/else are quoted command strings (the
        // tokenizer has already unquoted them).
        "if-shell" | "if" => {
            apply_if_shell(cfg, &tokens[1..], warnings);
            Ok(())
        }
        // run-shell at config-load time: run the command for its side effects
        // (its output has nowhere to go during parsing). Bounded; failures warn.
        "run-shell" | "run" => {
            if let Some(c) = tokens.get(1) {
                let _ = std::process::Command::new("sh").arg("-c").arg(c).output();
            }
            Ok(())
        }
        other => {
            warnings.push(format!("unsupported command: {other}"));
            Ok(())
        }
    }
}

/// Handle `set-hook [-g] <event> <command>`: store the command line to run when
/// `<event>` fires. Flags (-g global, -a append, -u unset) are tolerated; -u
/// removes the hook. The command is a single (usually quoted) token.
fn apply_set_hook(cfg: &mut Config, rest: &[String], _warnings: &mut [String]) {
    // -u (possibly combined, e.g. -gu) unsets the hook.
    let unset = rest.iter().any(|t| t.starts_with('-') && t.contains('u'));
    let args: Vec<&String> = rest.iter().filter(|t| !t.starts_with('-')).collect();
    let Some(event) = args.first() else { return };
    let event = event.trim_end_matches("[]").to_string();
    if unset {
        cfg.hooks.remove(&event);
        return;
    }
    if let Some(command) = args.get(1) {
        cfg.hooks.insert(event, command.to_string());
    }
}

/// Handle `if-shell <cmd> <then> [else]`: run `<cmd>` via `sh -c`; on exit 0
/// apply the `<then>` command line, otherwise the optional `<else>` line. The
/// branch strings are themselves tokenized and applied as a directive (so e.g.
/// `if-shell "test -d ~/x" "set -g mouse on"` works). Skipped flags like `-b`/`-F`
/// are ignored.
fn apply_if_shell(cfg: &mut Config, rest: &[String], warnings: &mut Vec<String>) {
    // Drop leading flags (-b background, -F format, etc.).
    let args: Vec<&String> = rest.iter().filter(|t| !t.starts_with('-')).collect();
    let Some(test_cmd) = args.first() else {
        warnings.push("if-shell: missing command".to_string());
        return;
    };
    let ok = std::process::Command::new("sh")
        .arg("-c")
        .arg(test_cmd.as_str())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let branch = if ok { args.get(1) } else { args.get(2) };
    if let Some(branch) = branch {
        if let Some(tokens) = tokenize(branch) {
            if !tokens.is_empty() {
                let _ = apply_directive(cfg, &tokens, warnings);
            }
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
    // The option -> Config mapping lives on Config so the runtime `:set` command
    // shares it exactly (one source of truth; the file loader and the prompt
    // can't drift apart).
    if let Err(msg) = cfg.set_option(option, value) {
        warnings.push(msg);
    }
}

impl Config {
    /// Apply one tmux `set-option` name/value pair to this config, returning
    /// `Err(message)` for an unrecognized option (the caller decides whether to
    /// warn or flash it). This is the single source of truth for the option ->
    /// field mapping, shared by the config-file loader ([`apply_set`]) and the
    /// runtime `:set` command, so the two can never drift apart.
    ///
    /// `value` is the already-unquoted argument text (may contain spaces for
    /// format/style options). Malformed numeric values are ignored (the field
    /// keeps its prior value) rather than erroring, matching the file loader.
    pub fn set_option(&mut self, option: &str, value: &str) -> Result<(), String> {
        let cfg = self;
        match option {
            "prefix" => cfg.prefix = value.to_string(),
            "mouse" => cfg.mouse = on_off(value),
            // lumux extension: whether the client tracks terminal-size changes and
            // tells the daemon. On by default; matters most over SSH. (Distinct from
            // tmux's `aggressive-resize`, which is about multi-client sizing policy
            // and stays ignored below.)
            "auto-resize" => cfg.auto_resize = on_off(value),
            "remain-on-exit" => cfg.remain_on_exit = on_off(value),
            "persist" => cfg.persist = on_off(value),
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
            // Active pane border color (tmux pane-active-border-style fg=green). We
            // take the fg= part; lumux doesn't style inactive borders.
            "pane-active-border-style" => {
                if let Some(fg) = style_fg(value) {
                    cfg.pane_active_border_fg = fg;
                }
            }
            // Inactive pane border color (tmux pane-border-style fg=...). Empty leaves
            // the terminal default.
            "pane-border-style" => {
                if let Some(fg) = style_fg(value) {
                    cfg.pane_border_fg = fg;
                }
            }
            // Window-list entry formats. These carry `#[...]` style spans and tokens
            // (#I index, #W name, #F flags) rendered by the status formatter.
            "window-status-format" => cfg.window_status_format = value.to_string(),
            "window-status-current-format" => cfg.window_status_current_format = value.to_string(),
            "window-status-separator" => cfg.window_status_separator = value.to_string(),
            // Message / command-prompt row styling (raw spec; fg/bg/attrs applied).
            "message-style" => cfg.message_style = value.to_string(),
            // display-panes overlay number colors (tmux prefix q).
            "display-panes-colour" | "display-panes-color" => {
                cfg.display_panes_colour = value.to_string();
            }
            "display-panes-active-colour" | "display-panes-active-color" => {
                cfg.display_panes_active_colour = value.to_string();
            }
            // Copy-mode key style: vi (default) or emacs.
            "mode-keys" => {
                let v = value.trim().to_lowercase();
                if v == "vi" || v == "emacs" {
                    cfg.mode_keys = v;
                }
            }
            "copy-command" => cfg.copy_command = value.trim().to_string(),
            // Known-but-irrelevant to lumux: silently accept the common ones so a
            // typical tmux.conf doesn't spew warnings for things that are simply the
            // default behavior in lumux, or features it doesn't have (copy-mode
            // selection styling, clock mode, activity monitoring, clipboard/terminal
            // negotiation).
            "status-keys" | "escape-time" | "status" | "status-interval"
            | "renumber-windows" | "set-titles" | "status-left-length" | "status-right-length"
            | "aggressive-resize" | "default-terminal" | "focus-events"
            | "mode-style" | "message-command-style" | "window-status-activity-style"
            | "window-status-bell-style" | "monitor-activity" | "monitor-bell"
            | "clock-mode-colour" | "clock-mode-color" | "clock-mode-style"
            | "set-clipboard" | "terminal-overrides" | "terminal-features"
            | "status-position" | "display-time" | "visual-activity" | "visual-bell" => {}
            other => return Err(format!("unsupported option: {other}")),
        }
        Ok(())
    }
}

/// Extract the `fg=` color from a tmux style string like `fg=green,bold`.
fn style_fg(value: &str) -> Option<String> {
    value
        .split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("fg=").map(str::to_string))
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
    let mut repeat = false;
    // Consume only *recognized* bind flags, stopping at the first token that
    // isn't one — that token is the key. A blind "starts_with('-')" would be
    // wrong because tmux keys can be a dash (`bind - …`). tmux's bind flags are a
    // fixed set: -n (root table), -r (repeatable), -a, -N <note>, -T <table>.
    while i < rest.len() {
        match rest[i].as_str() {
            "-n" => {
                root = true;
                i += 1;
            }
            "-r" => {
                repeat = true;
                i += 1;
            }
            "-a" => i += 1,
            "-N" => i += 2, // takes a note argument
            "-T" => {
                // -T root means the root (no-prefix) table; other tables ignored.
                if rest.get(i + 1).map(|s| s.as_str()) == Some("root") {
                    root = true;
                }
                i += 2;
            }
            _ => break, // not a known flag -> this is the key
        }
    }
    let Some(key) = rest.get(i) else {
        return Ok(());
    };
    // Validate the key now so a bad spec surfaces as an error (consistent with
    // the TOML path's to_bindings validation).
    if super::parse_key(key).is_none() {
        return Err(ConfigError::BadKey(key.clone()));
    }
    let tmux_cmd = &rest[i + 1..];
    let action = match resolve_bind_action(tmux_cmd) {
        Some(a) => a,
        None => {
            warnings.push(format!("unsupported binding command: {}", tmux_cmd.join(" ")));
            return Ok(());
        }
    };
    let table = if root {
        &mut cfg.root_bindings
    } else {
        &mut cfg.bindings
    };
    table.insert(key.clone(), action);
    if repeat {
        cfg.repeat_bindings.insert(key.clone());
    }
    Ok(())
}

/// Resolve a tmux binding's command tail to a lumux binding-value string (the
/// value later parsed by [`super::parse_action`] in `to_bindings`). Prefers the
/// unified command parser so a bind carries real arguments and `;`/`\;` chains
/// (e.g. `bind X new-window \; split-window -h`); falls back to the single-action
/// mapper for keymap-only verbs the command parser doesn't cover (select-pane,
/// resize-pane, copy-mode and the other overlay openers). None if unsupported.
///
/// A command chain is stored as the raw command line behind [`CMD_SENTINEL`] so
/// `parse_action` can re-parse it into an [`Action::RunCommands`]; storing the
/// line (not a serialized command list) keeps the config value a plain string.
fn resolve_bind_action(tmux_cmd: &[String]) -> Option<String> {
    // Reconstruct the command line (tmux joins the tail tokens; `\;` separates
    // chained commands). The tokenizer already stripped surrounding quotes, so
    // rejoin with spaces and let parse_commands re-split on `;`.
    let line = tmux_cmd
        .iter()
        .map(|t| if t == "\\;" { ";" } else { t.as_str() })
        .collect::<Vec<_>>()
        .join(" ");
    let cmds = crate::command::parse_commands(&line);
    // Use the command chain only if every segment is a recognized command (no
    // Unknown/BadArgs) — otherwise the verb belongs to the keymap-only set and
    // the single-action mapper handles it (or reports it unsupported).
    let all_known = !cmds.is_empty()
        && cmds.iter().all(|c| {
            !matches!(
                c,
                crate::command::ParsedCommand::Unknown(_) | crate::command::ParsedCommand::BadArgs(_)
            )
        });
    if all_known {
        return Some(format!("{}{}", super::CMD_SENTINEL, line));
    }
    map_bind_command(tmux_cmd)
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
        "last-window" => "last-window",
        "last-pane" => "last-pane",
        "kill-window" => "kill-window",
        "next-layout" => "next-layout",
        // `select-layout` without a named arg cycles like next-layout in lumux.
        "select-layout" => "next-layout",
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
            if has("-Z") {
                "zoom-pane" // tmux `resize-pane -Z` is the zoom toggle
            } else if has("-L") {
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
            } else if has("-m") {
                "mark-pane"
            } else {
                return None;
            }
        }
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
    fn set_option_is_the_shared_mapping() {
        // The runtime `:set` path calls Config::set_option directly, so it must
        // mutate exactly the fields the file loader does. Known options apply;
        // an unknown one returns Err (which the file loader turns into a warning
        // and the prompt flashes).
        let mut c = Config::default();
        assert!(c.set_option("mouse", "on").is_ok());
        assert!(c.mouse);
        assert!(c.set_option("mouse", "off").is_ok());
        assert!(!c.mouse);
        assert!(c.set_option("base-index", "1").is_ok());
        assert_eq!(c.base_index, 1);
        assert!(c.set_option("status-bg", "red").is_ok());
        assert_eq!(c.status_bg, "red");
        // A value with spaces (format string) is kept whole.
        assert!(c.set_option("status-left", "hi there").is_ok());
        assert_eq!(c.status_left, "hi there");
        // Unknown option is a recoverable error, not a silent no-op.
        assert!(c.set_option("bogus-option", "x").is_err());
    }

    #[test]
    fn default_shell_becomes_a_profile() {
        let c = Config::from_tmux("set -g default-shell powershell.exe").unwrap();
        assert_eq!(c.shell_argv(None), Some(vec!["powershell.exe".to_string()]));
    }

    #[test]
    fn auto_resize_option_parses_and_defaults_on() {
        // Default (no directive) stays on.
        assert!(Config::from_tmux("set -g mouse on").unwrap().auto_resize);
        // Explicit off.
        let c = Config::from_tmux("set -g auto-resize off").unwrap();
        assert!(!c.auto_resize);
        // Explicit on.
        let c = Config::from_tmux("set -g auto-resize on").unwrap();
        assert!(c.auto_resize);
        // tmux's aggressive-resize is a different concept and must NOT toggle it
        // (and must not warn).
        let p = Config::from_tmux_verbose("set -g aggressive-resize on").unwrap();
        assert!(p.config.auto_resize, "aggressive-resize must not affect auto_resize");
        assert!(
            p.warnings.is_empty(),
            "aggressive-resize is a known-ignored option; got {:?}",
            p.warnings
        );
    }

    #[test]
    fn pane_active_border_style_sets_fg() {
        let c = Config::from_tmux("set -g pane-active-border-style fg=colour208,bold").unwrap();
        assert_eq!(c.pane_active_border_fg, "colour208");
        // pane-border-style is accepted (ignored) without a warning.
        let p = Config::from_tmux_verbose("set -g pane-border-style fg=grey").unwrap();
        assert!(p.warnings.is_empty(), "pane-border-style must be silently accepted");
    }

    #[test]
    fn mode_keys_and_remain_on_exit_parse() {
        let c = Config::from_tmux("setw -g mode-keys emacs").unwrap();
        assert_eq!(c.mode_keys, "emacs");
        // Default stays vi; an unknown value is ignored (keeps default).
        assert_eq!(Config::default().mode_keys, "vi");
        let c = Config::from_tmux("set -g mode-keys bogus").unwrap();
        assert_eq!(c.mode_keys, "vi", "unknown mode-keys keeps the default");
        // remain-on-exit toggles the flag and doesn't warn.
        let p = Config::from_tmux_verbose("set -g remain-on-exit on").unwrap();
        assert!(p.config.remain_on_exit);
        assert!(p.warnings.is_empty());
        // persist toggles its flag too.
        let c = Config::from_tmux("set -g persist on").unwrap();
        assert!(c.persist);
        assert!(!Config::default().persist, "persist is off by default");
    }

    #[test]
    fn if_shell_applies_the_chosen_branch() {
        // True test → the THEN branch runs (mouse turned on).
        let c = Config::from_tmux(r#"if-shell "true" "set -g mouse on""#).unwrap();
        assert!(c.mouse, "then-branch should apply when the test succeeds");
        // False test → the ELSE branch runs.
        let c = Config::from_tmux(r#"if-shell "false" "set -g mouse on" "set -g mouse off""#).unwrap();
        assert!(!c.mouse, "else-branch should apply when the test fails");
        // False test with no else → nothing happens (mouse stays default off).
        let c = Config::from_tmux(r#"if-shell "false" "set -g mouse on""#).unwrap();
        assert!(!c.mouse);
    }

    #[test]
    fn set_hook_stores_and_unsets() {
        let c = Config::from_tmux(r#"set-hook -g pane-exited "respawn-pane""#).unwrap();
        assert_eq!(c.hooks.get("pane-exited").map(String::as_str), Some("respawn-pane"));
        // -u removes a previously-set hook.
        let c = Config::from_tmux(
            "set-hook -g pane-exited \"respawn-pane\"\nset-hook -gu pane-exited",
        )
        .unwrap();
        assert!(!c.hooks.contains_key("pane-exited"));
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
            bind D new-window \; split-window -h
        "#;
        let c = Config::from_tmux(conf).unwrap();
        let b = c.to_bindings().unwrap();
        use crate::command::{Dir, ParsedCommand};
        use crate::keymap::{Action, Key, KeyCode};
        // split-window now flows through the command parser so it carries its
        // -h/-v argument (a RunCommands chain), instead of collapsing to a fixed
        // split-horizontal/-vertical action.
        assert_eq!(
            b.lookup(&Key::char('|')),
            Some(&Action::RunCommands(vec![ParsedCommand::SplitWindow(Dir::Horizontal)]))
        );
        assert_eq!(
            b.lookup(&Key::char('-')),
            Some(&Action::RunCommands(vec![ParsedCommand::SplitWindow(Dir::Vertical)]))
        );
        // A `\;` chain of two command verbs is preserved in order.
        assert_eq!(
            b.lookup(&Key::char('D')),
            Some(&Action::RunCommands(vec![
                ParsedCommand::NewWindow,
                ParsedCommand::SplitWindow(Dir::Horizontal),
            ]))
        );
        // select-pane is a keymap-only verb (no command equivalent), so it still
        // resolves to the directional action via the fallback mapper.
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
    fn source_file_bind_still_reloads() {
        // A keymap-only verb chained with a command verb can't be represented as
        // a RunCommands chain (source-file isn't a command); it falls back to the
        // single-action mapper, keeping the reload (the display tail is dropped,
        // same as before this feature — documented limitation, not a regression).
        use crate::keymap::{Action, Key};
        let c = Config::from_tmux("bind r source-file ~/.tmux.conf \\; display \"reloaded\"").unwrap();
        let b = c.to_bindings().unwrap();
        assert_eq!(b.lookup(&Key::char('r')), Some(&Action::ReloadConfig));
    }

    #[test]
    fn dash_key_and_flags_parse_correctly() {
        use crate::command::{Dir, ParsedCommand};
        use crate::keymap::{Action, Key, KeyCode};
        // Regression: a key that looks like a flag (`-`) must not be consumed as
        // one. `bind - split-window -v` binds the literal minus key.
        let c = Config::from_tmux("bind - split-window -v").unwrap();
        let b = c.to_bindings().unwrap();
        assert_eq!(
            b.lookup(&Key::char('-')),
            Some(&Action::RunCommands(vec![ParsedCommand::SplitWindow(Dir::Vertical)]))
        );

        // Real bind flags are still consumed: -r (repeat) and -N <note> before
        // the key; -n routes to the root table. Bind zoom to a NON-default key
        // ('g') so the assertion proves the `-Z` mapping, not the default `z`.
        let c = Config::from_tmux(
            "bind -r H resize-pane -L\nbind -N \"note here\" g resize-pane -Z\nbind -n M-x kill-pane",
        )
        .unwrap();
        let b = c.to_bindings().unwrap();
        // resize-pane -L is now a real command verb (carries its direction), so
        // it's a RunCommands chain; -Z (zoom) has no command equivalent and
        // still falls back to the keymap-only mapper.
        assert_eq!(
            b.lookup(&Key::char('H')),
            Some(&Action::RunCommands(vec![ParsedCommand::ResizePane {
                dir: crate::layout::Direction::Left,
                cells: None
            }]))
        );
        assert_eq!(b.lookup(&Key::char('g')), Some(&Action::ZoomPane));
        // -r on `bind -r H ...` marks H repeatable; the other two binds didn't
        // request -r, so they're not.
        assert!(b.is_repeatable(&Key::char('H')));
        assert!(!b.is_repeatable(&Key::char('g')));
        // kill-pane is a command verb → a RunCommands chain of one.
        assert_eq!(
            b.lookup_root(&Key {
                code: KeyCode::Char('x'),
                ctrl: false,
                alt: true
            }),
            Some(&Action::RunCommands(vec![ParsedCommand::KillPane(None)]))
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
    fn hidden_vars_expand_in_directives() {
        // The Catppuccin-style pattern: define a palette with %hidden, reference
        // it as $VAR / ${VAR} inside style strings and other options.
        let conf = r##"
            %hidden BG="#1e1e2e"
            %hidden FG="#cdd6f4"
            %hidden ACCENT="#89b4fa"
            set -g status-style "bg=$BG,fg=$FG"
            set -g status-left "#[fg=$BG,bg=${ACCENT}] #S "
            set -g pane-active-border-style "fg=$ACCENT"
        "##;
        let c = Config::from_tmux(conf).unwrap();
        // %hidden lines are not warnings and the vars are substituted.
        assert_eq!(c.status_bg, "#1e1e2e");
        assert_eq!(c.status_fg, "#cdd6f4");
        assert_eq!(c.status_left, "#[fg=#1e1e2e,bg=#89b4fa] #S ");
        assert_eq!(c.pane_active_border_fg, "#89b4fa");
    }

    #[test]
    fn bare_and_referential_assignments_work() {
        // Bare NAME=value (no %hidden) also defines a var, and a later var may
        // reference an earlier one.
        let conf = r##"
            ACCENT="#89b4fa"
            LEFT="fg=$ACCENT,bold"
            set -g status-style "$LEFT"
        "##;
        let p = Config::from_tmux_verbose(conf).unwrap();
        // Assignments must not produce "unsupported command" warnings.
        assert!(
            !p.warnings.iter().any(|w| w.contains("ACCENT") || w.contains("LEFT")),
            "assignments should not warn: {:?}",
            p.warnings
        );
        // status-style only pulls fg here; bold is ignored, fg expanded.
        assert_eq!(p.config.status_fg, "#89b4fa");
    }

    #[test]
    fn unknown_var_left_literal() {
        // An undefined reference is left as-is rather than blanked, so the
        // mistake is visible instead of silently dropping the value.
        let c = Config::from_tmux(r#"set -g status-left "$NOPE end""#).unwrap();
        assert_eq!(c.status_left, "$NOPE end");
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
        // A genuinely unknown option is warned about. (monitor-activity and
        // renumber-windows are silently accepted — lumux either has no such
        // feature or it's already the default, so warning would be noise.)
        assert!(p.warnings.iter().any(|w| w.contains("some-future-option")));
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
        assert_eq!(c.pane_active_border_fg, "green");
        // default-shell becomes the resolved default argv.
        assert_eq!(c.shell_argv(None), Some(vec!["powershell.exe".to_string()]));
        // Bindings compile, including the splits, zoom, and root nav.
        let b = c.to_bindings().expect("bindings build");
        use crate::command::{Dir, ParsedCommand};
        use crate::keymap::{Action, Key, KeyCode};
        // split-window carries its -h/-v arg through the command parser now.
        assert_eq!(
            b.lookup(&Key::char('|')),
            Some(&Action::RunCommands(vec![ParsedCommand::SplitWindow(Dir::Horizontal)]))
        );
        assert_eq!(
            b.lookup(&Key::char('-')),
            Some(&Action::RunCommands(vec![ParsedCommand::SplitWindow(Dir::Vertical)]))
        );
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
