//! Kimi Code lifecycle-hook adapter.
//!
//! Kimi configures hooks in **TOML** (`config.toml`) rather than JSON, as an
//! array of `[[hooks]]` tables carrying `event`, an optional regex `matcher`,
//! `command` and `timeout`. Because the file is user-authored TOML, this
//! adapter edits it as text between delimiter comments instead of reformatting
//! the whole document — reinstalling replaces only lumux's block.
//!
//! Kimi exposes no session-end hook, so nothing here clears the pane on exit;
//! the daemon's process detection drops the row when the agent exits.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::common;

const INTEGRATION_MARKER: &str = "LUMUX_INTEGRATION_ID=kimi";
const BLOCK_BEGIN: &str = "# >>> lumux kimi integration";
const BLOCK_END: &str = "# <<< lumux kimi integration";

const HOOK_FILE: &str = "lumux-agent-state.sh";
const HOOK_WRAPPER: &str = include_str!("assets/kimi/lumux-agent-state.sh");

/// Kimi asks the user a question by calling an `AskUserQuestion` tool, so the
/// pane is blocked for the span of that one tool call and working for every
/// other. The matchers are regexes, which is how that distinction is expressed.
const ASK_USER_QUESTION: &str = "^AskUserQuestion$";
const OTHER_TOOL: &str = "^(?!AskUserQuestion$).*$";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HookEvent {
    event: &'static str,
    matcher: Option<&'static str>,
    state: &'static str,
}

const HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        event: "SessionStart",
        matcher: None,
        state: "idle",
    },
    HookEvent {
        event: "UserPromptSubmit",
        matcher: None,
        state: "working",
    },
    HookEvent {
        event: "PreToolUse",
        matcher: Some(OTHER_TOOL),
        state: "working",
    },
    // Asking the user a question is the one tool call that means "waiting".
    HookEvent {
        event: "PreToolUse",
        matcher: Some(ASK_USER_QUESTION),
        state: "blocked",
    },
    HookEvent {
        event: "PostToolUse",
        matcher: Some(ASK_USER_QUESTION),
        state: "working",
    },
    HookEvent {
        event: "PostToolUseFailure",
        matcher: Some(ASK_USER_QUESTION),
        state: "working",
    },
    HookEvent {
        event: "PermissionRequest",
        matcher: None,
        state: "blocked",
    },
    HookEvent {
        event: "PermissionResult",
        matcher: None,
        state: "working",
    },
    HookEvent {
        event: "Stop",
        matcher: None,
        state: "idle",
    },
    HookEvent {
        event: "Interrupt",
        matcher: None,
        state: "idle",
    },
];

pub(super) fn install() -> anyhow::Result<()> {
    install_at(kimi_dir()?)
}

fn kimi_dir() -> anyhow::Result<PathBuf> {
    kimi_dir_with(|key| std::env::var_os(key))
}

fn kimi_dir_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    common::config_dir_with("KIMI_CODE_HOME", ".kimi-code", getenv)
}

fn install_at(dir: PathBuf) -> anyhow::Result<()> {
    if !dir.is_dir() {
        anyhow::bail!(
            "kimi code config directory not found at {}; install Kimi Code first",
            dir.display()
        );
    }
    let config_path = dir.join("config.toml");
    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(HOOK_FILE);

    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let updated = config_with_block(&existing, &hook_path);

    std::fs::create_dir_all(&hooks_dir)?;
    common::write_managed_hook(&hook_path, HOOK_WRAPPER)?;
    if updated != existing {
        common::write_config_text(&config_path, &updated)?;
    }
    println!(
        "installed Kimi state hooks into {} ({} events)",
        config_path.display(),
        HOOK_EVENTS.len()
    );
    Ok(())
}

/// Replace lumux's delimited block, leaving every other line untouched. Editing
/// as text (rather than parse-and-reserialize) preserves the user's comments,
/// ordering and formatting in a file they own.
fn config_with_block(content: &str, hook_path: &Path) -> String {
    let mut result = strip_block(content).trim_end_matches('\n').to_string();
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    result.push_str(BLOCK_BEGIN);
    result.push('\n');
    for descriptor in HOOK_EVENTS {
        result.push_str(&hook_table(hook_path, *descriptor));
    }
    result.push_str(BLOCK_END);
    result.push('\n');
    result
}

fn strip_block(content: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in content.lines() {
        if line.trim() == BLOCK_BEGIN {
            inside = true;
            continue;
        }
        if inside {
            if line.trim() == BLOCK_END {
                inside = false;
            }
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

fn hook_table(hook_path: &Path, descriptor: HookEvent) -> String {
    let matcher = descriptor
        .matcher
        .map(|matcher| format!("matcher = {}\n", toml_string(matcher)))
        .unwrap_or_default();
    format!(
        "[[hooks]]\nevent = {}\n{matcher}command = {}\ntimeout = 10\n\n",
        toml_string(descriptor.event),
        toml_string(&hook_command(hook_path, descriptor.state))
    )
}

fn hook_command(hook_path: &Path, state: &str) -> String {
    format!(
        "{INTEGRATION_MARKER} sh {} {state}",
        single_quote(&hook_path.display().to_string())
    )
}

/// POSIX single-quoting, kept local because the shared helper is unix-gated and
/// this module must still compile on Windows to report the unsupported case.
fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// TOML basic string. The command embeds single quotes and backslashes, so it
/// must be escaped rather than emitted raw.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_prefers_the_env_override() {
        let dir =
            kimi_dir_with(|key| (key == "KIMI_CODE_HOME").then(|| OsString::from("/tmp/kimi-cfg")))
                .unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/kimi-cfg"));
    }

    #[test]
    fn block_replaces_itself_and_preserves_user_config() {
        let hook = PathBuf::from("/home/u/.kimi-code/hooks/lumux-agent-state.sh");
        let user = "model = \"k2\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"mine\"\n";
        let once = config_with_block(user, &hook);
        assert!(once.starts_with("model = \"k2\""), "user config kept first");
        assert!(once.contains("command = \"mine\""), "foreign hook kept");
        assert_eq!(once.matches(BLOCK_BEGIN).count(), 1);

        // Reinstalling replaces the block instead of stacking a second copy.
        let twice = config_with_block(&once, &hook);
        assert_eq!(twice.matches(BLOCK_BEGIN).count(), 1);
        assert_eq!(twice.matches(BLOCK_END).count(), 1);
        assert_eq!(once, twice, "the block must be stable across reinstalls");
        assert!(
            twice.contains("command = \"mine\""),
            "foreign hook survives"
        );
    }

    #[test]
    fn ask_user_question_is_the_blocked_matcher() {
        let blocked: Vec<_> = HOOK_EVENTS
            .iter()
            .filter(|d| d.state == "blocked")
            .collect();
        assert!(blocked
            .iter()
            .any(|d| d.event == "PreToolUse" && d.matcher == Some(ASK_USER_QUESTION)));
        // ...and the complementary matcher keeps every other tool "working".
        assert!(HOOK_EVENTS.iter().any(|d| d.event == "PreToolUse"
            && d.matcher == Some(OTHER_TOOL)
            && d.state == "working"));
    }

    #[test]
    fn emitted_toml_escapes_the_command() {
        let hook = PathBuf::from("/tmp/it's odd/lumux-agent-state.sh");
        let table = hook_table(
            &hook,
            HookEvent {
                event: "Stop",
                matcher: None,
                state: "idle",
            },
        );
        assert!(table.contains("event = \"Stop\""));
        assert!(table.contains("timeout = 10"));
        // Both quoting layers compose: the path's single quote is shell-escaped
        // to '"'"' and that sequence is then TOML-escaped, so the file carries
        // '\"'\"' — anything else would break one of the two parsers.
        assert!(
            table.contains(r#"'\"'\"'"#),
            "shell quoting must survive TOML escaping: {table}"
        );
    }

    #[test]
    fn install_errors_when_the_config_dir_is_missing() {
        let dir = std::env::temp_dir().join(format!("lumux-kimi-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = install_at(dir).unwrap_err();
        assert!(
            err.to_string().contains("install Kimi Code first"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn install_writes_the_wrapper_and_config() {
        let dir = std::env::temp_dir().join(format!("lumux-kimi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "model = \"k2\"\n").unwrap();

        install_at(dir.clone()).unwrap();
        let config = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(config.contains("model = \"k2\""));
        assert_eq!(config.matches("[[hooks]]").count(), HOOK_EVENTS.len());
        let hook = dir.join("hooks").join(HOOK_FILE);
        assert!(hook.is_file(), "wrapper should land under hooks/");
        assert!(std::fs::read_to_string(&hook)
            .unwrap()
            .contains(INTEGRATION_MARKER));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
