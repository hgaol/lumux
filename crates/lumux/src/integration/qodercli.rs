//! Qoder CLI lifecycle-hook adapter.
//!
//! Qoder's hook schema mirrors Claude's `settings.json` (see
//! <https://docs.qoder.com/cli/hooks>): a top-level `hooks` object keyed by
//! event name, each entry holding a matcher plus a list of
//! `{type: "command", command, timeout}` invocations, with the event payload
//! arriving on stdin. The wrapper lives under `<config>/hooks/`. The adapter
//! stays behind the generic `lumux report-state` seam; no Qoder concepts reach
//! the daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::common;

const INTEGRATION_MARKER: &str = "LUMUX_INTEGRATION_ID=qodercli";

const HOOK_FILE: &str = "lumux-agent-state.sh";
const HOOK_WRAPPER: &str = include_str!("assets/qodercli/lumux-agent-state.sh");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HookEvent {
    event: &'static str,
    state: &'static str,
}

/// Only events representing a stable semantic transition are installed.
///
/// Qoder also emits `SubagentStart`/`SubagentStop`, `PreCompact` and
/// `PostToolUseFailure`. Those are deliberately skipped: a background subagent
/// or a mid-turn compaction would otherwise repaint the foreground pane, and a
/// tool failure is still "working" rather than a new state.
const HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        event: "SessionStart",
        state: "idle",
    },
    HookEvent {
        event: "UserPromptSubmit",
        state: "working",
    },
    HookEvent {
        event: "PreToolUse",
        state: "working",
    },
    HookEvent {
        event: "PostToolUse",
        state: "working",
    },
    HookEvent {
        event: "PermissionRequest",
        state: "blocked",
    },
    HookEvent {
        event: "Notification",
        state: "blocked",
    },
    HookEvent {
        event: "Stop",
        state: "idle",
    },
    // Qoder's process usually outlives the agent session inside a live shell,
    // so nothing else would drop the row.
    HookEvent {
        event: "SessionEnd",
        state: "clear",
    },
];

pub(super) fn install() -> anyhow::Result<()> {
    install_at(qodercli_dir()?)
}

fn qodercli_dir() -> anyhow::Result<PathBuf> {
    qodercli_dir_with(|key| std::env::var_os(key))
}

fn qodercli_dir_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    common::config_dir_with("QODER_HOME", ".qoder", getenv)
}

fn install_at(dir: PathBuf) -> anyhow::Result<()> {
    if !dir.is_dir() {
        anyhow::bail!(
            "qoder config directory not found at {}; install the Qoder CLI first",
            dir.display()
        );
    }
    let settings_path = dir.join("settings.json");
    let hooks_dir = dir.join("hooks");
    let hook_path = hooks_dir.join(HOOK_FILE);

    // Parse the user-owned config before creating the managed wrapper so a bad
    // config leaves nothing half-installed.
    let (write_path, mut root) = common::read_json_config(&settings_path)?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", settings_path.display()))?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` in {} is not an object", settings_path.display()))?;

    common::prune_managed_nested_hooks(hooks, is_managed);
    for descriptor in HOOK_EVENTS {
        append_hook(hooks, &hook_path, *descriptor)?;
    }

    std::fs::create_dir_all(&hooks_dir)?;
    common::write_managed_hook(&hook_path, HOOK_WRAPPER)?;
    common::write_json_config(&settings_path, &write_path, &root)?;
    println!(
        "installed Qoder state hooks into {} ({} events)",
        settings_path.display(),
        HOOK_EVENTS.len()
    );
    Ok(())
}

fn is_managed(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(INTEGRATION_MARKER))
}

fn append_hook(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
    descriptor: HookEvent,
) -> anyhow::Result<()> {
    let groups = hooks
        .entry(descriptor.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks.{} must be an array", descriptor.event))?;
    groups.push(serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": hook_command(hook_path, descriptor.state),
            "timeout": 10,
        }],
    }));
    Ok(())
}

/// The shell command Qoder runs. The marker travels in the command string so a
/// reinstall recognizes its own entries without a side-car registry.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn managed_commands(root: &Value, event: &str) -> Vec<String> {
        root["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|group| group["hooks"].as_array())
                    .flatten()
                    .filter(|entry| is_managed(entry))
                    .map(|entry| entry["command"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn config_dir_prefers_the_env_override() {
        let dir =
            qodercli_dir_with(|key| (key == "QODER_HOME").then(|| OsString::from("/tmp/qoder-cfg")))
                .unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/qoder-cfg"));
    }

    #[test]
    fn install_writes_every_event_and_preserves_foreign_config() {
        let dir = std::env::temp_dir().join(format!("lumux-qoder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings_path = dir.join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"theme":"dark","hooks":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"other-tool"}]}]}}"#,
        )
        .unwrap();

        install_at(dir.clone()).unwrap();
        let root = read(&settings_path);

        assert_eq!(root["theme"], "dark", "foreign keys must survive");
        // The foreign Stop hook survives alongside ours.
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert!(stop
            .iter()
            .filter_map(|group| group["hooks"].as_array())
            .flatten()
            .any(|entry| entry["command"] == "other-tool"));
        for descriptor in HOOK_EVENTS {
            let commands = managed_commands(&root, descriptor.event);
            assert_eq!(commands.len(), 1, "one hook for {}", descriptor.event);
            assert!(
                commands[0].ends_with(descriptor.state),
                "{} should report {}; got {}",
                descriptor.event,
                descriptor.state,
                commands[0]
            );
        }
        // The wrapper lands under hooks/ and carries the marker.
        let hook = dir.join("hooks").join(HOOK_FILE);
        assert!(hook.is_file(), "wrapper should be written under hooks/");
        assert!(std::fs::read_to_string(&hook)
            .unwrap()
            .contains(INTEGRATION_MARKER));

        // Re-running replaces our entries rather than duplicating them.
        install_at(dir.clone()).unwrap();
        let root = read(&settings_path);
        for descriptor in HOOK_EVENTS {
            assert_eq!(
                managed_commands(&root, descriptor.event).len(),
                1,
                "{} must stay single after reinstall",
                descriptor.event
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_errors_when_the_config_dir_is_missing() {
        let dir = std::env::temp_dir().join(format!("lumux-qoder-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = install_at(dir).unwrap_err();
        assert!(
            err.to_string().contains("install the Qoder CLI first"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn subagent_and_compaction_events_are_not_installed() {
        // These would let a background subagent or a mid-turn compaction
        // repaint the foreground pane.
        for skipped in [
            "SubagentStart",
            "SubagentStop",
            "PreCompact",
            "PostToolUseFailure",
        ] {
            assert!(
                !HOOK_EVENTS.iter().any(|d| d.event == skipped),
                "{skipped} must not be installed"
            );
        }
    }
}
