//! `lumux report-state` payload building and `lumux integration <agent>` hook
//! installation.
//!
//! Agents self-report their state (idle/working/blocked/done) so the sidebar
//! and session chooser can show it. `report-state` turns a state name plus the
//! `$LUMUX_PANE` the shell was spawned with into a [`ReportAgentState`] command;
//! `integration claude` writes the Claude Code hooks that call `report-state`
//! at the right lifecycle points.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use lumux_core::agent::AgentState;
use lumux_core::proto::Command as Cmd;

/// Build the `ReportAgentState` command for `lumux report-state`. Reads the
/// target pane from `$LUMUX_PANE` (set on every pane the daemon spawns) and the
/// agent label from `--agent`, else `$LUMUX_AGENT`, else `"agent"`. `getenv` is
/// injected so this is unit-testable without touching the process environment.
pub fn build_report_command(
    state: &str,
    agent: Option<&str>,
    getenv: impl Fn(&str) -> Option<OsString>,
) -> anyhow::Result<Option<Cmd>> {
    let pane = getenv("LUMUX_PANE")
        .and_then(|v| v.into_string().ok())
        .filter(|s| !s.is_empty());
    // Claude's user-level hooks also run in terminals that are not owned by
    // lumux. Telemetry is inapplicable there, not an error: match herdr's
    // best-effort hook contract and exit silently.
    let Some(pane) = pane else {
        return Ok(None);
    };
    // `clear` removes the pane from the agents list (the agent exited but the
    // shell/pane lives on, so nothing else would drop it).
    if state.eq_ignore_ascii_case("clear") {
        return Ok(Some(Cmd::ClearAgentState { pane }));
    }
    let state: AgentState = state
        .parse()
        .map_err(|e: lumux_core::agent::AgentStateParseError| anyhow::anyhow!(e.to_string()))?;
    let agent = agent
        .map(str::to_string)
        .or_else(|| getenv("LUMUX_AGENT").and_then(|v| v.into_string().ok()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agent".to_string());
    Ok(Some(Cmd::ReportAgentState { pane, agent, state }))
}

/// The lifecycle-event → reported-state mapping for an agent's hooks. Each entry
/// is `(claude_hook_event, state)`. Kept as data so it can be asserted in tests
/// and rendered into the settings JSON.
pub const CLAUDE_HOOK_EVENTS: &[(&str, &str)] = &[
    // The agent just launched → show it immediately (idle until the first
    // prompt), so it appears in the sidebar the moment Claude Code starts.
    ("SessionStart", "idle"),
    // The user submitted a prompt / a tool is about to run → the agent is busy.
    ("UserPromptSubmit", "working"),
    ("PreToolUse", "working"),
    // The agent is asking for input / permission → it's waiting on you.
    ("Notification", "blocked"),
    // The turn finished, prompt is back → idle.
    ("Stop", "idle"),
    // The agent exited but its pane/shell stays alive, so remove it from the
    // agents list (nothing else would clear it — close_pane only fires on a
    // pane's own death).
    ("SessionEnd", "clear"),
];

#[cfg(not(windows))]
const CLAUDE_HOOK_FILE: &str = "lumux-agent-state.sh";
#[cfg(windows)]
const CLAUDE_HOOK_FILE: &str = "lumux-agent-state.cmd";

#[cfg(not(windows))]
const CLAUDE_HOOK_WRAPPER: &str = r#"#!/bin/sh
# installed by lumux
# managed by lumux; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# LUMUX_INTEGRATION_ID=claude
# LUMUX_INTEGRATION_VERSION=1

# Claude sends hook metadata on stdin. Drain it even when this is not a lumux
# pane so the global hook remains invisible to its caller.
cat >/dev/null 2>&1 || true

state="${1:-}"
case "$state" in
  idle|working|blocked|done|clear) ;;
  *) exit 0 ;;
esac

[ -n "${LUMUX:-}" ] || exit 0
[ -n "${LUMUX_PANE:-}" ] || exit 0
command -v lumux >/dev/null 2>&1 || exit 0

lumux report-state "$state" --agent claude >/dev/null 2>&1 || true
exit 0
"#;

#[cfg(windows)]
const CLAUDE_HOOK_WRAPPER: &str = "@echo off\r\n\
rem installed by lumux\r\n\
rem managed by lumux; reinstalling or updating the integration overwrites this file.\r\n\
rem add custom hooks beside this file instead of editing it.\r\n\
rem LUMUX_INTEGRATION_ID=claude\r\n\
rem LUMUX_INTEGRATION_VERSION=1\r\n\
more >nul 2>nul\r\n\
set \"state=%~1\"\r\n\
if /I \"%state%\"==\"idle\" goto valid_state\r\n\
if /I \"%state%\"==\"working\" goto valid_state\r\n\
if /I \"%state%\"==\"blocked\" goto valid_state\r\n\
if /I \"%state%\"==\"done\" goto valid_state\r\n\
if /I \"%state%\"==\"clear\" goto valid_state\r\n\
exit /b 0\r\n\
:valid_state\r\n\
if not defined LUMUX exit /b 0\r\n\
if not defined LUMUX_PANE exit /b 0\r\n\
where lumux >nul 2>nul || exit /b 0\r\n\
lumux report-state \"%state%\" --agent claude >nul 2>nul\r\n\
exit /b 0\r\n";

/// Install the state-reporting hooks for `agent`. Only `claude` is supported for
/// now; other names return a clear "not yet supported" error so the command
/// surface can grow agent-by-agent.
pub fn install(agent: &str) -> anyhow::Result<()> {
    match agent {
        "claude" => install_claude(claude_settings_path()?),
        other => {
            anyhow::bail!("integration for {other:?} is not yet supported (only `claude` for now)")
        }
    }
}

/// Path to Claude Code's user settings file (`~/.claude/settings.json`).
fn claude_settings_path() -> anyhow::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("cannot locate home directory"))?;
    Ok(home.join(".claude").join("settings.json"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Merge lumux's report-state hooks into the Claude settings JSON at `path`,
/// preserving every other key and any non-lumux hooks the user already has.
/// Idempotent: re-running replaces only lumux's own hook entries.
fn install_claude(path: PathBuf) -> anyhow::Result<()> {
    use serde_json::{Map, Value};

    // Load existing settings (or start empty). A parse error is surfaced rather
    // than silently overwriting the user's config.
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?,
        _ => Value::Object(Map::new()),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;

    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` in {} is not an object", path.display()))?;

    // Clean every event, not only the events in the current mapping. This is
    // also the migration path for lifecycle hooks installed by older versions.
    // Ownership lives on nested command entries: matcher groups may contain
    // user hooks beside ours and must not be discarded wholesale.
    remove_managed_claude_hooks(hooks);

    let wrapper_path = install_claude_wrapper(&path)?;

    for (event, state) in CLAUDE_HOOK_EVENTS {
        let command = claude_hook_command(&wrapper_path, state);
        let entry = lumux_hook_entry(&command);
        let list = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = list.as_array_mut().ok_or_else(|| {
            anyhow::anyhow!("hooks.{event} in {} is not an array", path.display())
        })?;
        arr.push(entry);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(&root)?;
    std::fs::write(&path, serialized + "\n")?;
    println!(
        "installed Claude Code state hooks into {} ({} events)",
        path.display(),
        CLAUDE_HOOK_EVENTS.len()
    );
    Ok(())
}

/// Install the managed boundary between Claude's global hooks and the lumux
/// CLI. The wrapper drains stdin, validates pane context, suppresses output,
/// and always succeeds so reporting can never disturb Claude's terminal.
fn install_claude_wrapper(settings_path: &Path) -> anyhow::Result<PathBuf> {
    let settings_dir = settings_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let wrapper_path = settings_dir.join("hooks").join(CLAUDE_HOOK_FILE);
    if let Some(parent) = wrapper_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&wrapper_path, CLAUDE_HOOK_WRAPPER)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&wrapper_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper_path, permissions)?;
    }

    Ok(wrapper_path)
}

#[cfg(not(windows))]
fn claude_hook_command(wrapper_path: &Path, state: &str) -> String {
    format!(
        "sh {} {state}",
        shell_single_quote(&wrapper_path.display().to_string())
    )
}

#[cfg(not(windows))]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn claude_hook_command(wrapper_path: &Path, state: &str) -> String {
    // Double quotes cannot occur in a Windows path, and `call` is required for
    // a .cmd wrapper to return control cleanly to Claude's command shell.
    format!("call \"{}\" {state}", wrapper_path.display())
}

/// A single Claude hook entry (matcher `*`) that runs `command`, tagged so it can
/// be recognized and replaced on re-install.
fn lumux_hook_entry(command: &str) -> serde_json::Value {
    serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 1,
            "lumux_managed": true,
        }],
    })
}

/// Remove only lumux-owned nested commands from every Claude hook event.
/// Matcher groups that still contain foreign hooks retain all their metadata;
/// groups and event keys made empty by the migration are removed.
fn remove_managed_claude_hooks(hooks: &mut serde_json::Map<String, serde_json::Value>) {
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(groups) = hooks
            .get_mut(&event)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };

        let mut removed_any = false;
        groups.retain_mut(|group| {
            let Some(commands) = group
                .get_mut("hooks")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return true;
            };
            let before = commands.len();
            commands.retain(|command| !is_lumux_managed_command(command));
            let removed = commands.len() != before;
            removed_any |= removed;

            // Preserve pre-existing empty/unknown groups. Remove a group only
            // when deleting our commands is what made it empty.
            !(removed && commands.is_empty())
        });

        if removed_any && groups.is_empty() {
            hooks.remove(&event);
        }
    }
}

fn is_lumux_managed_command(hook: &serde_json::Value) -> bool {
    if hook.get("lumux_managed").and_then(|m| m.as_bool()) == Some(true) {
        return true;
    }

    hook.get("command")
        .and_then(|command| command.as_str())
        .is_some_and(|command| {
            let command = command.to_ascii_lowercase();
            command.contains("lumux report-state")
                || command.contains("lumux.exe report-state")
                || command.contains("lumux-agent-state")
        })
}

/// Whether a hook-list entry is one lumux installed (so re-install replaces it
/// instead of duplicating). Recognized by the `lumux_managed` marker or, as a
/// fallback, a command that invokes `lumux report-state`.
#[cfg(test)]
fn is_lumux_hook(v: &serde_json::Value) -> bool {
    v.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| arr.iter().any(is_lumux_managed_command))
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEMP_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique_temp_dir() -> PathBuf {
        let id = NEXT_TEMP_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("lumux-int-test-{}-{id}", std::process::id()))
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| OsString::from(*v))
        }
    }

    #[test]
    fn report_command_reads_pane_and_agent_from_env() {
        let cmd = build_report_command(
            "working",
            None,
            env_of(&[("LUMUX_PANE", "%42"), ("LUMUX_AGENT", "claude")]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            cmd,
            Cmd::ReportAgentState {
                pane: "%42".into(),
                agent: "claude".into(),
                state: AgentState::Working,
            }
        );
    }

    #[test]
    fn explicit_agent_flag_overrides_env() {
        let cmd = build_report_command(
            "blocked",
            Some("codex"),
            env_of(&[("LUMUX_PANE", "%1"), ("LUMUX_AGENT", "claude")]),
        )
        .unwrap()
        .unwrap();
        match cmd {
            Cmd::ReportAgentState { agent, state, .. } => {
                assert_eq!(agent, "codex");
                assert_eq!(state, AgentState::Blocked);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn agent_defaults_when_unset() {
        let cmd = build_report_command("idle", None, env_of(&[("LUMUX_PANE", "%3")]))
            .unwrap()
            .unwrap();
        match cmd {
            Cmd::ReportAgentState { agent, .. } => assert_eq!(agent, "agent"),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn missing_pane_is_a_silent_noop() {
        assert_eq!(
            build_report_command("idle", None, env_of(&[])).unwrap(),
            None
        );
    }

    #[test]
    fn bad_state_is_an_error() {
        let err =
            build_report_command("frobbing", None, env_of(&[("LUMUX_PANE", "%1")])).unwrap_err();
        assert!(err.to_string().contains("idle, working, blocked"));
    }

    #[test]
    fn clear_builds_a_clear_command() {
        let cmd = build_report_command("clear", None, env_of(&[("LUMUX_PANE", "%9")]))
            .unwrap()
            .unwrap();
        assert_eq!(cmd, Cmd::ClearAgentState { pane: "%9".into() });
    }

    #[test]
    fn hooks_cover_launch_and_exit() {
        // SessionStart makes the agent appear immediately; SessionEnd clears it
        // so it vanishes when the agent exits (the pane lives on).
        let map: std::collections::HashMap<_, _> = CLAUDE_HOOK_EVENTS.iter().copied().collect();
        assert_eq!(map.get("SessionStart"), Some(&"idle"));
        assert_eq!(map.get("SessionEnd"), Some(&"clear"));
    }

    #[cfg(not(windows))]
    #[test]
    fn hook_command_shell_quotes_wrapper_path() {
        let path = Path::new("/tmp/claude's hooks/lumux-agent-state.sh");
        assert_eq!(
            claude_hook_command(path, "idle"),
            "sh '/tmp/claude'\"'\"'s hooks/lumux-agent-state.sh' idle"
        );
    }

    #[test]
    fn install_claude_writes_valid_idempotent_hooks() {
        let dir = unique_temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        // Seed mixed matcher groups so migration must remove only the owned
        // nested commands, including one on an event lumux no longer installs.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&serde_json::json!({
                "theme": "dark",
                "hooks": {
                    "Stop": [{
                        "matcher": "*",
                        "hooks": [
                            {"type": "command", "command": "other-tool"},
                            {"type": "command", "command": "lumux report-state idle --agent claude"}
                        ]
                    }],
                    "PostToolUse": [{
                        "matcher": "tool",
                        "hooks": [
                            {"type": "command", "command": "lumux report-state working --agent claude"},
                            {"type": "command", "command": "echo keep-post"}
                        ]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        install_claude(path.clone()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        // Unrelated key preserved.
        assert_eq!(v["theme"], "dark");
        // The foreign Stop hook survives alongside ours.
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|group| {
            group["hooks"]
                .as_array()
                .is_some_and(|hooks| hooks.iter().any(|hook| hook["command"] == "other-tool"))
        }));
        assert!(stop.iter().any(is_lumux_hook));

        // Obsolete managed hooks are purged across every event without
        // deleting their foreign siblings or matcher metadata.
        let post = v["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 1);
        assert_eq!(post[0]["matcher"], "tool");
        let post_hooks = post[0]["hooks"].as_array().unwrap();
        assert_eq!(post_hooks.len(), 1);
        assert_eq!(post_hooks[0]["command"], "echo keep-post");

        #[cfg(windows)]
        let wrapper_path = dir.join("hooks").join("lumux-agent-state.cmd");
        #[cfg(not(windows))]
        let wrapper_path = dir.join("hooks").join("lumux-agent-state.sh");

        assert!(wrapper_path.is_file(), "managed hook wrapper must exist");
        // Every mapped event got a lumux hook.
        for (event, state) in CLAUDE_HOOK_EVENTS {
            let arr = v["hooks"][event].as_array().unwrap();
            let ours = arr.iter().find(|h| is_lumux_hook(h)).unwrap();
            let command = ours["hooks"][0]["command"].as_str().unwrap();
            assert!(command.contains("lumux-agent-state"));
            assert!(command.ends_with(state));
            assert_eq!(ours["hooks"][0]["timeout"], 1);
        }

        let wrapper = std::fs::read_to_string(&wrapper_path).unwrap();
        assert!(wrapper.contains("LUMUX_INTEGRATION_ID=claude"));
        assert!(wrapper.contains("LUMUX_PANE"));
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;

            assert!(wrapper.contains("${LUMUX:-}"));
            assert!(wrapper.contains(">/dev/null 2>&1"));
            assert_ne!(
                std::fs::metadata(&wrapper_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0,
                "managed Unix wrapper must be executable"
            );

            // The global Claude hook also runs outside lumux. That case must
            // consume stdin and return silently instead of printing an error.
            let output = std::process::Command::new("sh")
                .arg(&wrapper_path)
                .arg("idle")
                .env_remove("LUMUX")
                .env_remove("LUMUX_PANE")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stdout.is_empty());
            assert!(output.stderr.is_empty());
        }
        #[cfg(windows)]
        {
            assert!(wrapper.contains("if not defined LUMUX"));
            assert!(wrapper.contains(">nul 2>nul"));
        }

        // Re-run: idempotent — exactly one lumux hook per event, no duplicates.
        install_claude(path.clone()).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (event, _) in CLAUDE_HOOK_EVENTS {
            let arr = v2["hooks"][event].as_array().unwrap();
            assert_eq!(
                arr.iter().filter(|h| is_lumux_hook(h)).count(),
                1,
                "event {event} must have exactly one lumux hook after re-install"
            );
        }
        // Foreign Stop hook still there after re-run.
        let stop2 = v2["hooks"]["Stop"].as_array().unwrap();
        assert!(stop2.iter().any(|group| {
            group["hooks"]
                .as_array()
                .is_some_and(|hooks| hooks.iter().any(|hook| hook["command"] == "other-tool"))
        }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_agent_errors() {
        let err = install("codex").unwrap_err();
        assert!(err.to_string().contains("not yet supported"));
    }
}
