//! Parse a typed command line (tmux's command-prompt, prefix `:`) into
//! [`ParsedCommand`]s the daemon can dispatch. This is pure, unit-testable
//! parsing — no I/O — covering the subset of tmux commands lumux implements.
//!
//! A line may chain several commands with `;` ([`parse_commands`]); each segment
//! is parsed by [`parse_command`]. Unknown verbs yield [`ParsedCommand::Unknown`]
//! so the daemon can flash a helpful message instead of silently ignoring input.

/// A direction argument for split/join (`-h` horizontal, `-v` vertical).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Horizontal,
    Vertical,
}

/// A parsed `-t` target. tmux target syntax is rich; v1 handles the numeric
/// forms lumux can resolve: a window by index (`-t N` / `-t :N`) and a pane by
/// index within the active window (`-t .N`). `None`-valued commands act on the
/// active window/pane as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A window by its (base-index-adjusted) position: `-t 2`, `-t :2`.
    Window(u32),
    /// A pane by its index within the active window: `-t .1`.
    Pane(u32),
}

impl Target {
    /// Parse a `-t` value into a [`Target`]. `.N` is a pane index; `N` or `:N`
    /// (a leading `:` names a window in tmux) is a window index. Returns None if
    /// the value isn't one of these numeric forms.
    pub fn parse(v: &str) -> Option<Target> {
        if let Some(rest) = v.strip_prefix('.') {
            return rest.parse().ok().map(Target::Pane);
        }
        let rest = v.strip_prefix(':').unwrap_or(v);
        rest.parse().ok().map(Target::Window)
    }
}

/// A parsed command-prompt line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    SplitWindow(Dir),
    NewWindow,
    /// `kill-pane [-t TARGET]`: kill the target pane (None = the active pane).
    KillPane(Option<Target>),
    KillWindow,
    NextWindow,
    PrevWindow,
    LastWindow,
    LastPane,
    /// `select-window -t N` / `select-window N` / `selectw N`.
    SelectWindow(u32),
    RenameWindow(String),
    RenameSession(String),
    /// `find-window <query>` / `findw <query>`.
    FindWindow(String),
    BreakPane,
    /// `swap-pane -U` (previous) / `-D` (next). Defaults to next. `-t .N` swaps
    /// the active pane with pane N in the active window instead of a sibling.
    SwapPane { next: bool, target: Option<Target> },
    /// `join-pane [-h|-v] [-s SRC]`: move the active pane of source window `src`
    /// (an index; None = the last-active window) into the current window,
    /// splitting in `dir`.
    JoinPane { dir: Dir, src: Option<u32> },
    /// `synchronize-panes [on|off]`; None toggles.
    SynchronizePanes(Option<bool>),
    DisplayPanes,
    /// `capture-pane`: copy the active pane's visible text into a paste buffer.
    CapturePane,
    /// `respawn-pane`: restart the shell in a dead pane (remain-on-exit).
    RespawnPane,
    /// `run-shell <cmd>`: run a shell command; its output goes to a paste buffer.
    RunShell(String),
    /// `display-message <text>` / `display <text>`: flash a status-line message.
    DisplayMessage(String),
    /// `send-keys <text>`: inject text into the active pane (verbatim; no tmux
    /// key-name translation). Usable from a binding or the `:` prompt.
    SendKeys(String),
    /// `select-layout [NAME]`: apply a named preset layout (even-horizontal,
    /// even-vertical, main-vertical, main-horizontal, tiled), or cycle to the
    /// next preset when no name is given (like `next-layout`).
    SelectLayout(Option<String>),
    /// `save-state`: write the session snapshot to disk now (tmux-resurrect save).
    SaveState,
    Detach,
    /// Recognized verb but the arguments didn't parse (flash a usage hint).
    BadArgs(&'static str),
    /// Unrecognized verb (the string is the verb, for the error message).
    Unknown(String),
}

/// Parse one command line. Leading/trailing whitespace is ignored; an empty
/// line yields None (nothing to do).
pub fn parse_command(line: &str) -> Option<ParsedCommand> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // run-shell takes the entire remainder verbatim (it's a shell command line,
    // so we must NOT split/strip its flags and quotes).
    for prefix in ["run-shell ", "run ", "run-shell\t", "run\t"] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let cmd = rest.trim();
            if cmd.is_empty() {
                return Some(ParsedCommand::BadArgs("usage: run-shell COMMAND"));
            }
            return Some(ParsedCommand::RunShell(cmd.to_string()));
        }
    }
    if line == "run-shell" || line == "run" {
        return Some(ParsedCommand::BadArgs("usage: run-shell COMMAND"));
    }
    // send-keys injects its argument verbatim (it may contain flag-looking text
    // like `ls -la`), so take the rest of the line rather than splitting flags.
    for prefix in ["send-keys ", "send ", "send-keys\t", "send\t"] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let text = rest.trim();
            if text.is_empty() {
                return Some(ParsedCommand::BadArgs("usage: send-keys TEXT"));
            }
            return Some(ParsedCommand::SendKeys(unquote(text)));
        }
    }
    if line == "send-keys" || line == "send" {
        return Some(ParsedCommand::BadArgs("usage: send-keys TEXT"));
    }
    let mut parts = line.split_whitespace();
    let verb = parts.next()?;
    let args: Vec<&str> = parts.collect();
    Some(dispatch(verb, &args))
}

/// Parse a command line that may contain several commands joined by `;` (tmux's
/// command separator). Each segment is parsed with [`parse_command`]; empty
/// segments are skipped. A `;` inside single/double quotes is literal, and the
/// verbatim tail of a `run-shell`/`run` segment keeps its `;` (so
/// `run-shell "a; b"` and `run echo hi ; next-window` both behave correctly).
///
/// Returns the commands in order (possibly empty for a blank line).
pub fn parse_commands(line: &str) -> Vec<ParsedCommand> {
    split_commands(line)
        .into_iter()
        .filter_map(|seg| parse_command(&seg))
        .collect()
}

/// Split a line into command segments on top-level (unquoted) `;`. Quotes are
/// honored so a `;` inside `'...'`/`"..."` doesn't split. A segment whose verb
/// is `run-shell`/`run` swallows the rest of the line verbatim (its shell
/// command may contain `;`), matching how [`parse_command`] treats that tail.
fn split_commands(line: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        // Once the current segment is a run-shell/run command, the remainder is
        // its verbatim shell command line — take all of it (no more splitting).
        if quote.is_none() && is_run_shell_head(&cur) {
            cur.push(c);
            cur.extend(chars);
            break;
        }
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                ';' => {
                    segments.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            },
        }
    }
    segments.push(cur);
    segments
}

/// Whether the accumulated segment text so far begins with the `run-shell`/`run`
/// verb followed by a space (so its tail must be taken verbatim). Only the first
/// word matters; leading whitespace is ignored.
fn is_run_shell_head(seg: &str) -> bool {
    let s = seg.trim_start();
    matches!(s.split_whitespace().next(), Some("run-shell" | "run")) && s.contains(char::is_whitespace)
}


fn dispatch(verb: &str, args: &[&str]) -> ParsedCommand {
    match verb {
        "split-window" | "splitw" => {
            // tmux defaults to a vertical split (-v, top/bottom) without flags.
            let dir = if args.contains(&"-h") {
                Dir::Horizontal
            } else {
                Dir::Vertical
            };
            ParsedCommand::SplitWindow(dir)
        }
        "new-window" | "neww" => ParsedCommand::NewWindow,
        "kill-pane" | "killp" => ParsedCommand::KillPane(parse_target(args)),
        "kill-window" | "killw" => ParsedCommand::KillWindow,
        "next-window" | "next" => ParsedCommand::NextWindow,
        "previous-window" | "prev" => ParsedCommand::PrevWindow,
        "last-window" | "last" => ParsedCommand::LastWindow,
        "last-pane" | "lastp" => ParsedCommand::LastPane,
        "select-window" | "selectw" => match parse_target_index(args) {
            Some(n) => ParsedCommand::SelectWindow(n),
            None => ParsedCommand::BadArgs("usage: select-window -t INDEX"),
        },
        "rename-window" | "renamew" => match join_rest(args) {
            Some(name) => ParsedCommand::RenameWindow(name),
            None => ParsedCommand::BadArgs("usage: rename-window NAME"),
        },
        "rename-session" | "rename" => match join_rest(args) {
            Some(name) => ParsedCommand::RenameSession(name),
            None => ParsedCommand::BadArgs("usage: rename-session NAME"),
        },
        "find-window" | "findw" => match join_rest(args) {
            Some(q) => ParsedCommand::FindWindow(q),
            None => ParsedCommand::BadArgs("usage: find-window QUERY"),
        },
        "break-pane" | "breakp" => ParsedCommand::BreakPane,
        "swap-pane" | "swapp" => {
            // -U = swap with previous (up), -D = with next (down). Default next.
            // -t .N targets a specific pane in the active window.
            let next = !args.contains(&"-U");
            ParsedCommand::SwapPane {
                next,
                target: parse_target(args),
            }
        }
        "join-pane" | "joinp" => {
            let dir = if args.contains(&"-h") {
                Dir::Horizontal
            } else {
                Dir::Vertical
            };
            // -s SRC selects the source window by index; absent = last window.
            let src = match flag_value(args, "-s") {
                Some(v) => match v.trim_start_matches('.').parse() {
                    Ok(n) => Some(Some(n)),
                    Err(_) => None, // present but unparsable
                },
                None => Some(None),
            };
            match src {
                Some(src) => ParsedCommand::JoinPane { dir, src },
                None => ParsedCommand::BadArgs("usage: join-pane [-h|-v] [-s INDEX]"),
            }
        }
        "synchronize-panes" | "synchronize-pane" => {
            let state = match args.first().copied() {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            };
            ParsedCommand::SynchronizePanes(state)
        }
        "display-panes" | "displayp" => ParsedCommand::DisplayPanes,
        "display-message" | "display" => match join_rest(args) {
            Some(text) => ParsedCommand::DisplayMessage(unquote(&text)),
            None => ParsedCommand::BadArgs("usage: display-message TEXT"),
        },
        // send-keys is handled by the verbatim-tail path in parse_command; it
        // never reaches dispatch(), so no arm is needed here.
        "select-layout" | "selectl" => {
            // Bare = cycle (like next-layout); a name applies that preset.
            ParsedCommand::SelectLayout(join_rest(args))
        }
        "capture-pane" | "capturep" => ParsedCommand::CapturePane,
        "respawn-pane" | "respawnp" => ParsedCommand::RespawnPane,
        "save-state" | "saves" => ParsedCommand::SaveState,
        "detach-client" | "detach" => ParsedCommand::Detach,
        other => ParsedCommand::Unknown(other.to_string()),
    }
}

/// Extract an index from `select-window`-style args: either a bare number, or
/// the value after `-t` (tmux's target flag; a leading `.`/`:` is tolerated).
fn parse_target_index(args: &[&str]) -> Option<u32> {
    if let Some(v) = flag_value(args, "-t") {
        return v.trim_start_matches([':', '.']).parse().ok();
    }
    // Otherwise the first bare (non-flag) argument.
    args.iter()
        .find(|a| !a.starts_with('-'))
        .and_then(|a| a.parse().ok())
}

/// The value following `flag` in `args`, if present (`-t 3` → "3").
fn flag_value<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1).copied())
}

/// Parse an optional `-t TARGET` from args into a [`Target`]. Absent flag → None
/// (act on the active window/pane); present-but-unparsable → None too (a bad
/// target degrades to the active default rather than failing the command).
fn parse_target(args: &[&str]) -> Option<Target> {
    flag_value(args, "-t").and_then(Target::parse)
}

/// Join all non-flag args into a single space-separated string (for names /
/// queries). None if there are no such args.
fn join_rest(args: &[&str]) -> Option<String> {
    let words: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with('-')).collect();
    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}

/// Strip a single pair of matching surrounding quotes (`'…'` or `"…"`). The `:`
/// prompt tokenizer is whitespace-based, so a quoted argument arrives with its
/// quotes intact; commands that take free text (display-message) unquote here.
fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line_is_none() {
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn split_window_directions() {
        assert_eq!(parse_command("split-window"), Some(ParsedCommand::SplitWindow(Dir::Vertical)));
        assert_eq!(parse_command("splitw -h"), Some(ParsedCommand::SplitWindow(Dir::Horizontal)));
        assert_eq!(parse_command("split-window -v"), Some(ParsedCommand::SplitWindow(Dir::Vertical)));
    }

    #[test]
    fn simple_verbs_and_aliases() {
        assert_eq!(parse_command("new-window"), Some(ParsedCommand::NewWindow));
        assert_eq!(parse_command("neww"), Some(ParsedCommand::NewWindow));
        assert_eq!(parse_command("killp"), Some(ParsedCommand::KillPane(None)));
        assert_eq!(parse_command("next"), Some(ParsedCommand::NextWindow));
        assert_eq!(parse_command("prev"), Some(ParsedCommand::PrevWindow));
        assert_eq!(parse_command("last"), Some(ParsedCommand::LastWindow));
        assert_eq!(parse_command("display-panes"), Some(ParsedCommand::DisplayPanes));
    }

    #[test]
    fn select_window_target_forms() {
        assert_eq!(parse_command("select-window -t 2"), Some(ParsedCommand::SelectWindow(2)));
        assert_eq!(parse_command("selectw 3"), Some(ParsedCommand::SelectWindow(3)));
        // tmux tolerates a leading : on the target.
        assert_eq!(parse_command("select-window -t :1"), Some(ParsedCommand::SelectWindow(1)));
        assert!(matches!(parse_command("select-window"), Some(ParsedCommand::BadArgs(_))));
    }

    #[test]
    fn rename_and_find_take_the_rest_as_text() {
        assert_eq!(
            parse_command("rename-window my build"),
            Some(ParsedCommand::RenameWindow("my build".to_string()))
        );
        assert_eq!(
            parse_command("find-window vim"),
            Some(ParsedCommand::FindWindow("vim".to_string()))
        );
        assert!(matches!(parse_command("rename-window"), Some(ParsedCommand::BadArgs(_))));
    }

    #[test]
    fn swap_pane_direction() {
        assert_eq!(parse_command("swap-pane -U"), Some(ParsedCommand::SwapPane { next: false, target: None }));
        assert_eq!(parse_command("swap-pane -D"), Some(ParsedCommand::SwapPane { next: true, target: None }));
        assert_eq!(parse_command("swapp"), Some(ParsedCommand::SwapPane { next: true, target: None }));
    }

    #[test]
    fn join_pane_source_and_direction() {
        assert_eq!(
            parse_command("join-pane"),
            Some(ParsedCommand::JoinPane { dir: Dir::Vertical, src: None })
        );
        assert_eq!(
            parse_command("join-pane -h -s 2"),
            Some(ParsedCommand::JoinPane { dir: Dir::Horizontal, src: Some(2) })
        );
        // tmux's .N relative target form: the leading . is stripped.
        assert_eq!(
            parse_command("joinp -s .1"),
            Some(ParsedCommand::JoinPane { dir: Dir::Vertical, src: Some(1) })
        );
        assert!(matches!(parse_command("join-pane -s notanum"), Some(ParsedCommand::BadArgs(_))));
    }

    #[test]
    fn synchronize_panes_states() {
        assert_eq!(parse_command("synchronize-panes"), Some(ParsedCommand::SynchronizePanes(None)));
        assert_eq!(parse_command("synchronize-panes on"), Some(ParsedCommand::SynchronizePanes(Some(true))));
        assert_eq!(parse_command("synchronize-panes off"), Some(ParsedCommand::SynchronizePanes(Some(false))));
    }

    #[test]
    fn unknown_verb_is_reported() {
        assert_eq!(
            parse_command("frobnicate everything"),
            Some(ParsedCommand::Unknown("frobnicate".to_string()))
        );
    }

    #[test]
    fn capture_pane_and_aliases() {
        assert_eq!(parse_command("capture-pane"), Some(ParsedCommand::CapturePane));
        assert_eq!(parse_command("capturep"), Some(ParsedCommand::CapturePane));
    }

    #[test]
    fn respawn_pane_and_aliases() {
        assert_eq!(parse_command("respawn-pane"), Some(ParsedCommand::RespawnPane));
        assert_eq!(parse_command("respawnp"), Some(ParsedCommand::RespawnPane));
    }

    #[test]
    fn run_shell_takes_the_rest_verbatim() {
        assert_eq!(
            parse_command("run-shell echo hi -n"),
            Some(ParsedCommand::RunShell("echo hi -n".to_string()))
        );
        assert_eq!(
            parse_command("run date +%s"),
            Some(ParsedCommand::RunShell("date +%s".to_string()))
        );
        assert!(matches!(parse_command("run-shell"), Some(ParsedCommand::BadArgs(_))));
    }

    #[test]
    fn save_state_and_alias() {
        assert_eq!(parse_command("save-state"), Some(ParsedCommand::SaveState));
        assert_eq!(parse_command("saves"), Some(ParsedCommand::SaveState));
    }

    #[test]
    fn parse_commands_splits_on_semicolons() {
        assert_eq!(
            parse_commands("split-window ; new-window"),
            vec![
                ParsedCommand::SplitWindow(Dir::Vertical),
                ParsedCommand::NewWindow
            ]
        );
        // No spaces around the separator.
        assert_eq!(
            parse_commands("new-window;next-window"),
            vec![ParsedCommand::NewWindow, ParsedCommand::NextWindow]
        );
    }

    #[test]
    fn parse_commands_skips_empty_segments() {
        assert_eq!(
            parse_commands("  ; new-window ;; next-window ; "),
            vec![ParsedCommand::NewWindow, ParsedCommand::NextWindow]
        );
        assert!(parse_commands("").is_empty());
        assert!(parse_commands("   ;  ; ").is_empty());
    }

    #[test]
    fn parse_commands_keeps_semicolon_inside_quotes() {
        // A `;` inside a quoted rename argument is literal, not a separator.
        assert_eq!(
            parse_commands("rename-window \"a; b\" ; new-window"),
            vec![
                // The tokenizer here is whitespace-based, so the quotes are kept
                // in the name; what matters is the `;` did NOT split the segment.
                ParsedCommand::RenameWindow("\"a; b\"".to_string()),
                ParsedCommand::NewWindow
            ]
        );
    }

    #[test]
    fn parse_commands_run_shell_tail_keeps_semicolons() {
        // run-shell swallows the rest of the line verbatim, `;` included.
        assert_eq!(
            parse_commands("run-shell echo a; echo b ; next-window"),
            vec![ParsedCommand::RunShell("echo a; echo b ; next-window".to_string())]
        );
        // But a command BEFORE run-shell still splits normally.
        assert_eq!(
            parse_commands("new-window ; run echo hi ; there"),
            vec![
                ParsedCommand::NewWindow,
                ParsedCommand::RunShell("echo hi ; there".to_string())
            ]
        );
    }

    #[test]
    fn parse_commands_single_command_unchanged() {
        assert_eq!(
            parse_commands("split-window -h"),
            vec![ParsedCommand::SplitWindow(Dir::Horizontal)]
        );
    }

    #[test]
    fn display_message_takes_text_and_unquotes() {
        assert_eq!(
            parse_command("display-message hello there"),
            Some(ParsedCommand::DisplayMessage("hello there".to_string()))
        );
        // A single quoted word is unquoted (the `:` tokenizer keeps the quotes).
        assert_eq!(
            parse_command("display \"reloaded\""),
            Some(ParsedCommand::DisplayMessage("reloaded".to_string()))
        );
        assert!(matches!(
            parse_command("display-message"),
            Some(ParsedCommand::BadArgs(_))
        ));
    }

    #[test]
    fn target_parses_window_and_pane_forms() {
        assert_eq!(Target::parse("2"), Some(Target::Window(2)));
        assert_eq!(Target::parse(":3"), Some(Target::Window(3)));
        assert_eq!(Target::parse(".1"), Some(Target::Pane(1)));
        assert_eq!(Target::parse("notanum"), None);
        assert_eq!(Target::parse(".x"), None);
    }

    #[test]
    fn kill_pane_and_swap_pane_carry_targets() {
        assert_eq!(parse_command("kill-pane"), Some(ParsedCommand::KillPane(None)));
        assert_eq!(
            parse_command("kill-pane -t .2"),
            Some(ParsedCommand::KillPane(Some(Target::Pane(2))))
        );
        assert_eq!(
            parse_command("swap-pane -t .1"),
            Some(ParsedCommand::SwapPane {
                next: true,
                target: Some(Target::Pane(1))
            })
        );
        assert_eq!(
            parse_command("swap-pane -U"),
            Some(ParsedCommand::SwapPane {
                next: false,
                target: None
            })
        );
    }

    #[test]
    fn send_keys_and_select_layout_parse() {
        assert_eq!(
            parse_command("send-keys ls -la"),
            Some(ParsedCommand::SendKeys("ls -la".to_string()))
        );
        assert_eq!(
            parse_command("send \"echo hi\""),
            Some(ParsedCommand::SendKeys("echo hi".to_string()))
        );
        assert!(matches!(parse_command("send-keys"), Some(ParsedCommand::BadArgs(_))));
        // select-layout: bare cycles (None), a name applies that preset.
        assert_eq!(
            parse_command("select-layout"),
            Some(ParsedCommand::SelectLayout(None))
        );
        assert_eq!(
            parse_command("select-layout main-vertical"),
            Some(ParsedCommand::SelectLayout(Some("main-vertical".to_string())))
        );
    }
}
