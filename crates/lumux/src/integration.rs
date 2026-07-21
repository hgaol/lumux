//! `lumux report-state` payload building and `lumux integration <agent>` hook
//! installation.
//!
//! Agents self-report their state (idle/working/blocked/done) so the sidebar
//! and session chooser can show it. `report-state` turns a state name plus the
//! `$LUMUX_PANE` the shell was spawned with into a [`ReportAgentState`] command;
//! `integration claude` writes the Claude Code hooks that call `report-state`
//! at the right lifecycle points.

use std::ffi::OsString;
use std::path::PathBuf;

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
) -> anyhow::Result<Cmd> {
    let pane = getenv("LUMUX_PANE")
        .and_then(|v| v.into_string().ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("report-state must run inside a lumux pane ($LUMUX_PANE is unset)")
        })?;
    // `clear` removes the pane from the agents list (the agent exited but the
    // shell/pane lives on, so nothing else would drop it).
    if state.eq_ignore_ascii_case("clear") {
        return Ok(Cmd::ClearAgentState { pane });
    }
    let state: AgentState = state
        .parse()
        .map_err(|e: lumux_core::agent::AgentStateParseError| anyhow::anyhow!(e.to_string()))?;
    let agent = agent
        .map(str::to_string)
        .or_else(|| getenv("LUMUX_AGENT").and_then(|v| v.into_string().ok()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agent".to_string());
    Ok(Cmd::ReportAgentState { pane, agent, state })
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

/// Install the state-reporting hooks for `agent`. Only `claude` is supported for
/// now; other names return a clear "not yet supported" error so the command
/// surface can grow agent-by-agent.
pub fn install(agent: &str) -> anyhow::Result<()> {
    match agent {
        "claude" => install_claude(claude_settings_path()?),
        other => anyhow::bail!(
            "integration for {other:?} is not yet supported (only `claude` for now)"
        ),
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

    for (event, state) in CLAUDE_HOOK_EVENTS {
        let command = format!("lumux report-state {state} --agent claude");
        let entry = lumux_hook_entry(&command);
        let list = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = list
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("hooks.{event} in {} is not an array", path.display()))?;
        // Drop any prior lumux entry for this event, then append the fresh one —
        // this is what makes re-running idempotent (no duplicate hooks).
        arr.retain(|v| !is_lumux_hook(v));
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

/// A single Claude hook entry (matcher `*`) that runs `command`, tagged so it can
/// be recognized and replaced on re-install.
fn lumux_hook_entry(command: &str) -> serde_json::Value {
    serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": command,
            "lumux_managed": true,
        }],
    })
}

/// Whether a hook-list entry is one lumux installed (so re-install replaces it
/// instead of duplicating). Recognized by the `lumux_managed` marker or, as a
/// fallback, a command that invokes `lumux report-state`.
fn is_lumux_hook(v: &serde_json::Value) -> bool {
    v.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("lumux_managed").and_then(|m| m.as_bool()) == Some(true)
                    || h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c.contains("lumux report-state"))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cmd = build_report_command("idle", None, env_of(&[("LUMUX_PANE", "%3")])).unwrap();
        match cmd {
            Cmd::ReportAgentState { agent, .. } => assert_eq!(agent, "agent"),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn missing_pane_is_an_error() {
        let err = build_report_command("idle", None, env_of(&[])).unwrap_err();
        assert!(err.to_string().contains("LUMUX_PANE"));
    }

    #[test]
    fn bad_state_is_an_error() {
        let err =
            build_report_command("frobbing", None, env_of(&[("LUMUX_PANE", "%1")])).unwrap_err();
        assert!(err.to_string().contains("idle, working, blocked"));
    }

    #[test]
    fn clear_builds_a_clear_command() {
        let cmd = build_report_command("clear", None, env_of(&[("LUMUX_PANE", "%9")])).unwrap();
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

    #[test]
    fn install_claude_writes_valid_idempotent_hooks() {
        let dir = std::env::temp_dir().join(format!("lumux-int-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        // Seed a settings file with an unrelated key + a foreign hook to prove
        // we preserve them.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"theme":"dark","hooks":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"other-tool"}]}]}}"#,
        )
        .unwrap();

        install_claude(path.clone()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        // Unrelated key preserved.
        assert_eq!(v["theme"], "dark");
        // The foreign Stop hook survives alongside ours.
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|h| h["hooks"][0]["command"] == "other-tool"));
        assert!(stop.iter().any(is_lumux_hook));
        // Every mapped event got a lumux hook.
        for (event, state) in CLAUDE_HOOK_EVENTS {
            let arr = v["hooks"][event].as_array().unwrap();
            let ours = arr.iter().find(|h| is_lumux_hook(h)).unwrap();
            assert!(ours["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains(&format!("report-state {state}")));
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
        assert!(stop2.iter().any(|h| h["hooks"][0]["command"] == "other-tool"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_agent_errors() {
        let err = install("codex").unwrap_err();
        assert!(err.to_string().contains("not yet supported"));
    }
}
