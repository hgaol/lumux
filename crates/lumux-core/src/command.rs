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
    /// `rotate-window [-D|-U]`: rotate the panes in the active window (down by
    /// default). tmux prefix `C-o`.
    RotateWindow { down: bool },
    /// `swap-window -s A -t B` / `swap-window -t B`: swap two windows by index
    /// (source defaults to the active window). Indexes are base-index-adjusted.
    SwapWindow { src: Option<u32>, dst: u32 },
    /// `move-window -t N`: move the active window to index N.
    MoveWindow { dst: u32 },
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
    /// `send-keys [-l] KEYS…`: send keys to the active pane. Without `-l`, each
    /// whitespace-separated token is translated as a tmux key name (`Enter`,
    /// `C-c`, `Space`, arrows, …) where recognized, else sent literally; `-l`
    /// forces every token literal. The resolved bytes are computed at parse time.
    SendKeys(Vec<u8>),
    /// `select-layout [NAME]`: apply a named preset layout (even-horizontal,
    /// even-vertical, main-vertical, main-horizontal, tiled), or cycle to the
    /// next preset when no name is given (like `next-layout`).
    SelectLayout(Option<String>),
    /// `previous-layout`: cycle to the previous preset layout (reverse of
    /// `select-layout`'s bare-cycle direction).
    PreviousLayout,
    /// `save-state`: write the session snapshot to disk now (tmux-resurrect save).
    SaveState,
    /// `set-buffer [-b name] TEXT`: store text in a paste buffer (named or auto).
    SetBuffer { name: Option<String>, text: String },
    /// `paste-buffer [-b name]`: paste the named buffer (or the most recent).
    PasteNamedBuffer { name: Option<String> },
    /// `save-buffer [-b name] PATH`: write a buffer's text to a file.
    SaveBuffer { name: Option<String>, path: String },
    /// `load-buffer PATH`: read a file into a new paste buffer.
    LoadBuffer { path: String },
    /// `delete-buffer -b name`: delete the named buffer.
    DeleteBuffer { name: String },
    /// `new-session [-s NAME] [-d]`: create a new session. Unless `-d`
    /// (detached), the issuing client switches to it.
    NewSession { name: Option<String>, detached: bool },
    /// `kill-session [-t NAME]`: kill a session (the current one if omitted).
    KillSession { target: Option<String> },
    /// `kill-server`: kill every session and detach every client.
    KillServer,
    /// `switch-client -t NAME`: switch the current client to another session.
    SwitchClient { target: String },
    /// `resize-pane -L|-R|-U|-D [N]`: resize the active pane by N cells in that
    /// direction; bare (no N) uses the same nudge amount as the interactive
    /// Ctrl/Alt-arrow bindings. `-Z` (zoom) isn't modeled here — see the
    /// keymap-only `ZoomPane` fallback (`resize-pane` without a `-L/-R/-U/-D`
    /// direction falls back to that mapper).
    ResizePane {
        dir: crate::layout::Direction,
        cells: Option<u16>,
    },
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
    // send-keys sends key names (Enter, C-c, Space, arrows, …) translated to the
    // bytes a terminal would send, or literal text with -l. It takes the rest of
    // the line (may contain flag-looking tokens like `-la` as literal text).
    for prefix in ["send-keys ", "send ", "send-keys\t", "send\t"] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let rest = rest.trim();
            // -l anywhere in the leading flags forces literal (no translation).
            let (literal, body) = match rest.strip_prefix("-l") {
                Some(after) => (true, after.trim_start()),
                None => (false, rest),
            };
            if body.is_empty() {
                return Some(ParsedCommand::BadArgs("usage: send-keys [-l] KEYS"));
            }
            return Some(ParsedCommand::SendKeys(encode_send_keys(body, literal)));
        }
    }
    if line == "send-keys" || line == "send" {
        return Some(ParsedCommand::BadArgs("usage: send-keys [-l] KEYS"));
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
        "rotate-window" | "rotatew" => {
            // -U rotates up; default (or -D) rotates down.
            let down = !args.contains(&"-U");
            ParsedCommand::RotateWindow { down }
        }
        "swap-window" | "swapw" => {
            let src = flag_value(args, "-s").and_then(|v| v.trim_start_matches([':', '.']).parse().ok());
            match flag_value(args, "-t").and_then(|v| v.trim_start_matches([':', '.']).parse().ok()) {
                Some(dst) => ParsedCommand::SwapWindow { src, dst },
                None => ParsedCommand::BadArgs("usage: swap-window [-s A] -t B"),
            }
        }
        "move-window" | "movew" => {
            match flag_value(args, "-t").and_then(|v| v.trim_start_matches([':', '.']).parse().ok()) {
                Some(dst) => ParsedCommand::MoveWindow { dst },
                None => ParsedCommand::BadArgs("usage: move-window -t N"),
            }
        }
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
        "previous-layout" | "prevl" => ParsedCommand::PreviousLayout,
        "capture-pane" | "capturep" => ParsedCommand::CapturePane,
        "respawn-pane" | "respawnp" => ParsedCommand::RespawnPane,
        "save-state" | "saves" => ParsedCommand::SaveState,
        "set-buffer" | "setb" => {
            let name = flag_value(args, "-b").map(str::to_string);
            match non_flag_tail(args, "-b") {
                Some(text) => ParsedCommand::SetBuffer { name, text: unquote(&text) },
                None => ParsedCommand::BadArgs("usage: set-buffer [-b name] TEXT"),
            }
        }
        "paste-buffer" | "pasteb" => ParsedCommand::PasteNamedBuffer {
            name: flag_value(args, "-b").map(str::to_string),
        },
        "save-buffer" | "saveb" => {
            let name = flag_value(args, "-b").map(str::to_string);
            match non_flag_tail(args, "-b") {
                Some(path) => ParsedCommand::SaveBuffer { name, path: unquote(&path) },
                None => ParsedCommand::BadArgs("usage: save-buffer [-b name] PATH"),
            }
        }
        "load-buffer" | "loadb" => match non_flag_tail(args, "-b") {
            Some(path) => ParsedCommand::LoadBuffer { path: unquote(&path) },
            None => ParsedCommand::BadArgs("usage: load-buffer PATH"),
        },
        "delete-buffer" | "deleteb" => match flag_value(args, "-b").map(str::to_string) {
            Some(name) => ParsedCommand::DeleteBuffer { name },
            None => ParsedCommand::BadArgs("usage: delete-buffer -b name"),
        },
        "new-session" | "new" => ParsedCommand::NewSession {
            name: flag_value(args, "-s").map(str::to_string),
            detached: args.contains(&"-d"),
        },
        "kill-session" | "killsession" => ParsedCommand::KillSession {
            target: flag_value(args, "-t").map(str::to_string),
        },
        "kill-server" => ParsedCommand::KillServer,
        "switch-client" | "switchc" => match flag_value(args, "-t") {
            Some(t) => ParsedCommand::SwitchClient { target: t.to_string() },
            None => ParsedCommand::BadArgs("usage: switch-client -t NAME"),
        },
        "resize-pane" | "resizep" => {
            use crate::layout::Direction;
            let dir = if args.contains(&"-L") {
                Some(Direction::Left)
            } else if args.contains(&"-R") {
                Some(Direction::Right)
            } else if args.contains(&"-U") {
                Some(Direction::Up)
            } else if args.contains(&"-D") {
                Some(Direction::Down)
            } else {
                None
            };
            match dir {
                // The amount is the first bare (non-flag) numeric argument.
                Some(dir) => ParsedCommand::ResizePane {
                    dir,
                    cells: args.iter().find(|a| !a.starts_with('-')).and_then(|a| a.parse().ok()),
                },
                None => ParsedCommand::BadArgs("usage: resize-pane -L|-R|-U|-D [N]"),
            }
        }
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

/// Join the args that are neither `flag` nor its value into a space-separated
/// string — the free-text/path tail after removing a `-b name`-style option.
/// None when nothing remains.
fn non_flag_tail(args: &[&str], flag: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            i += 2; // skip the flag and its value
            continue;
        }
        out.push(args[i]);
        i += 1;
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
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

/// Resolve a `send-keys` body to the bytes to inject. With `literal`, the body
/// (unquoted) is sent verbatim. Otherwise each whitespace-separated token is a
/// tmux key name — `Enter`, `Space`, `Tab`, `C-c`, `M-x`, arrows, etc. —
/// translated via [`parse_key`](crate::config::parse_key) +
/// [`encode_key`](crate::keymap::encode_key); a token that isn't a known key
/// name is sent as its literal characters (so `send-keys hello Enter` types
/// "hello" then a carriage return).
fn encode_send_keys(body: &str, literal: bool) -> Vec<u8> {
    if literal {
        return unquote(body).into_bytes();
    }
    let mut out = Vec::new();
    for tok in body.split_whitespace() {
        match crate::config::parse_key(tok) {
            Some(key) => out.extend(crate::keymap::encode_key(&key)),
            // Not a key name: send its characters literally.
            None => out.extend(unquote(tok).as_bytes()),
        }
    }
    out
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
    fn send_keys_translates_key_names() {
        // Named keys translate to their byte sequences; Enter -> CR.
        assert_eq!(
            parse_command("send-keys Enter"),
            Some(ParsedCommand::SendKeys(b"\r".to_vec()))
        );
        // C-c -> 0x03; a plain word is sent as its literal chars.
        assert_eq!(
            parse_command("send-keys C-c"),
            Some(ParsedCommand::SendKeys(vec![0x03]))
        );
        assert_eq!(
            parse_command("send-keys hello Enter"),
            Some(ParsedCommand::SendKeys(b"hello\r".to_vec()))
        );
        // -l forces literal: the body is sent verbatim, "Enter" as text.
        assert_eq!(
            parse_command("send-keys -l Enter"),
            Some(ParsedCommand::SendKeys(b"Enter".to_vec()))
        );
        assert!(matches!(parse_command("send-keys"), Some(ParsedCommand::BadArgs(_))));
    }

    #[test]
    fn select_layout_parse() {
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

    #[test]
    fn buffer_commands_parse() {
        assert_eq!(
            parse_command("set-buffer hello world"),
            Some(ParsedCommand::SetBuffer { name: None, text: "hello world".to_string() })
        );
        assert_eq!(
            parse_command("set-buffer -b greet hi"),
            Some(ParsedCommand::SetBuffer { name: Some("greet".to_string()), text: "hi".to_string() })
        );
        assert_eq!(
            parse_command("paste-buffer -b greet"),
            Some(ParsedCommand::PasteNamedBuffer { name: Some("greet".to_string()) })
        );
        assert_eq!(
            parse_command("paste-buffer"),
            Some(ParsedCommand::PasteNamedBuffer { name: None })
        );
        assert_eq!(
            parse_command("save-buffer -b greet /tmp/x"),
            Some(ParsedCommand::SaveBuffer { name: Some("greet".to_string()), path: "/tmp/x".to_string() })
        );
        assert_eq!(
            parse_command("load-buffer /tmp/y"),
            Some(ParsedCommand::LoadBuffer { path: "/tmp/y".to_string() })
        );
        assert_eq!(
            parse_command("delete-buffer -b greet"),
            Some(ParsedCommand::DeleteBuffer { name: "greet".to_string() })
        );
        assert!(matches!(parse_command("set-buffer"), Some(ParsedCommand::BadArgs(_))));
        assert!(matches!(parse_command("delete-buffer"), Some(ParsedCommand::BadArgs(_))));
    }

    #[test]
    fn session_lifecycle_commands_parse() {
        assert_eq!(
            parse_command("new-session"),
            Some(ParsedCommand::NewSession { name: None, detached: false })
        );
        assert_eq!(
            parse_command("new-session -s work -d"),
            Some(ParsedCommand::NewSession { name: Some("work".to_string()), detached: true })
        );
        assert_eq!(
            parse_command("new -s work"),
            Some(ParsedCommand::NewSession { name: Some("work".to_string()), detached: false })
        );
        assert_eq!(
            parse_command("kill-session"),
            Some(ParsedCommand::KillSession { target: None })
        );
        assert_eq!(
            parse_command("kill-session -t work"),
            Some(ParsedCommand::KillSession { target: Some("work".to_string()) })
        );
        assert_eq!(parse_command("kill-server"), Some(ParsedCommand::KillServer));
        assert_eq!(
            parse_command("switch-client -t work"),
            Some(ParsedCommand::SwitchClient { target: "work".to_string() })
        );
        assert!(matches!(
            parse_command("switch-client"),
            Some(ParsedCommand::BadArgs(_))
        ));
    }

    #[test]
    fn resize_pane_parses_direction_and_amount() {
        use crate::layout::Direction;
        assert_eq!(
            parse_command("resize-pane -L 10"),
            Some(ParsedCommand::ResizePane { dir: Direction::Left, cells: Some(10) })
        );
        assert_eq!(
            parse_command("resize-pane -R"),
            Some(ParsedCommand::ResizePane { dir: Direction::Right, cells: None })
        );
        assert_eq!(
            parse_command("resize-pane -U 3"),
            Some(ParsedCommand::ResizePane { dir: Direction::Up, cells: Some(3) })
        );
        assert_eq!(
            parse_command("resize-pane -D 7"),
            Some(ParsedCommand::ResizePane { dir: Direction::Down, cells: Some(7) })
        );
        // No direction (and not -Z, which is handled by the keymap-only fallback
        // for `bind`, not this parser): BadArgs.
        assert!(matches!(
            parse_command("resize-pane -Z"),
            Some(ParsedCommand::BadArgs(_))
        ));
    }

    #[test]
    fn previous_layout_parses() {
        assert_eq!(parse_command("previous-layout"), Some(ParsedCommand::PreviousLayout));
        assert_eq!(parse_command("prevl"), Some(ParsedCommand::PreviousLayout));
    }
}
