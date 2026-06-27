//! Parse a typed command line (tmux's command-prompt, prefix `:`) into a
//! [`ParsedCommand`] the daemon can dispatch. This is pure, unit-testable
//! parsing — no I/O — covering the subset of tmux commands lumux implements.
//!
//! Only the first command on the line is parsed (no `;`-separated chains).
//! Unknown verbs yield [`ParsedCommand::Unknown`] so the daemon can flash a
//! helpful message instead of silently ignoring the input.

/// A direction argument for split/join (`-h` horizontal, `-v` vertical).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Horizontal,
    Vertical,
}

/// A parsed command-prompt line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    SplitWindow(Dir),
    NewWindow,
    KillPane,
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
    /// `swap-pane -U` (previous) / `-D` (next). Defaults to next.
    SwapPane { next: bool },
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
    let mut parts = line.split_whitespace();
    let verb = parts.next()?;
    let args: Vec<&str> = parts.collect();
    Some(dispatch(verb, &args))
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
        "kill-pane" | "killp" => ParsedCommand::KillPane,
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
            let next = !args.contains(&"-U");
            ParsedCommand::SwapPane { next }
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
        "capture-pane" | "capturep" => ParsedCommand::CapturePane,
        "respawn-pane" | "respawnp" => ParsedCommand::RespawnPane,
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
        assert_eq!(parse_command("killp"), Some(ParsedCommand::KillPane));
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
        assert_eq!(parse_command("swap-pane -U"), Some(ParsedCommand::SwapPane { next: false }));
        assert_eq!(parse_command("swap-pane -D"), Some(ParsedCommand::SwapPane { next: true }));
        assert_eq!(parse_command("swapp"), Some(ParsedCommand::SwapPane { next: true }));
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
}
