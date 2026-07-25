//! Cursor Agent CLI lifecycle-hook adapter.
//!
//! Cursor uses a flat `hooks.json` — `{"version": 1, "hooks": {event: [{command}]}}`
//! — rather than Claude's nested matcher groups, and its event names are
//! camelCase. Unlike Codex it exposes a real `sessionEnd`, so the pane is
//! cleared directly and no process-exit watcher is needed. The adapter stays
//! behind the generic `lumux report-state` seam; no Cursor concepts reach the
//! daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::common;

const INTEGRATION_MARKER: &str = "LUMUX_INTEGRATION_ID=cursor";

/// Cursor ships a POSIX-shell hook only (its CLI is unix-only today), so unlike
/// Codex there is no PowerShell variant to install.
const HOOK_FILE: &str = "lumux-agent-state.sh";
const HOOK_WRAPPER: &str = include_str!("assets/cursor/lumux-agent-state.sh");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HookEvent {
    event: &'static str,
    state: &'static str,
}

/// Only events that represent stable semantic transitions are installed.
/// `sessionEnd` clears the pane because Cursor's process usually outlives the
/// agent session inside a still-live shell, so nothing else would drop the row.
const HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        event: "sessionStart",
        state: "idle",
    },
    HookEvent {
        event: "beforeSubmitPrompt",
        state: "working",
    },
    HookEvent {
        event: "beforeShellExecution",
        state: "working",
    },
    HookEvent {
        event: "beforeMCPExecution",
        state: "working",
    },
    HookEvent {
        event: "stop",
        state: "idle",
    },
    HookEvent {
        event: "sessionEnd",
        state: "clear",
    },
];

pub(super) fn install() -> anyhow::Result<()> {
    install_at(cursor_dir()?)
}

fn cursor_dir() -> anyhow::Result<PathBuf> {
    cursor_dir_with(|key| std::env::var_os(key))
}

fn cursor_dir_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    common::config_dir_with("CURSOR_CONFIG_DIR", ".cursor", getenv)
}

fn install_at(dir: PathBuf) -> anyhow::Result<()> {
    if !dir.is_dir() {
        anyhow::bail!(
            "cursor config directory not found at {}; install the Cursor agent CLI first",
            dir.display()
        );
    }
    let hooks_path = dir.join("hooks.json");
    let hook_path = dir.join(HOOK_FILE);

    // Parse the user-owned config before creating the managed wrapper so a bad
    // config leaves nothing half-installed.
    let (write_path, mut root) = common::read_json_config(&hooks_path)?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", hooks_path.display()))?;
    // Cursor requires a schema version; preserve any existing value.
    obj.entry("version").or_insert_with(|| Value::from(1));
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` in {} is not an object", hooks_path.display()))?;

    prune_managed(hooks);
    for descriptor in HOOK_EVENTS {
        append_hook(hooks, &hook_path, *descriptor)?;
    }

    common::write_managed_hook(&hook_path, HOOK_WRAPPER)?;
    common::write_json_config(&hooks_path, &write_path, &root)?;
    println!(
        "installed Cursor state hooks into {} ({} events)",
        hooks_path.display(),
        HOOK_EVENTS.len()
    );
    Ok(())
}

/// Drop every previously-installed lumux entry so reinstalling is idempotent and
/// a renamed event never leaves an orphan behind. Foreign hooks are untouched.
fn prune_managed(hooks: &mut Map<String, Value>) {
    let mut empty = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(list) = entries.as_array_mut() else {
            continue;
        };
        list.retain(|entry| !is_managed(entry));
        if list.is_empty() {
            empty.push(event.clone());
        }
    }
    for event in empty {
        hooks.remove(&event);
    }
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
    let entries = hooks
        .entry(descriptor.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("hook entries for {} must be an array", descriptor.event)
        })?;
    entries.push(Value::Object(
        [("command".to_string(), Value::from(hook_command(hook_path, descriptor.state)))]
            .into_iter()
            .collect::<Map<String, Value>>(),
    ));
    Ok(())
}

/// The shell command Cursor runs. The marker travels in the command string so a
/// reinstall can recognize its own entries without a side-car registry.
fn hook_command(hook_path: &Path, state: &str) -> String {
    format!(
        "{INTEGRATION_MARKER} sh {} {state}",
        single_quote(&hook_path.display().to_string())
    )
}

/// POSIX single-quoting. Defined locally rather than reusing the parent helper,
/// which is unix-gated: the installer itself must still compile on Windows so
/// `lumux integration cursor` can report the unsupported case rather than fail
/// to build.
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
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| is_managed(entry))
                    .map(|entry| entry["command"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn config_dir_prefers_the_env_override() {
        let dir = cursor_dir_with(|key| {
            (key == "CURSOR_CONFIG_DIR").then(|| OsString::from("/tmp/cursor-cfg"))
        })
        .unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/cursor-cfg"));
    }

    #[test]
    fn install_writes_every_event_and_preserves_foreign_config() {
        let dir = std::env::temp_dir().join(format!("lumux-cursor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hooks_path = dir.join("hooks.json");
        std::fs::write(
            &hooks_path,
            r#"{"version":1,"theme":"dark","hooks":{"stop":[{"command":"other-tool"}]}}"#,
        )
        .unwrap();

        install_at(dir.clone()).unwrap();
        let root = read(&hooks_path);

        assert_eq!(root["version"], 1);
        assert_eq!(root["theme"], "dark", "foreign keys must survive");
        // The foreign stop hook survives alongside ours.
        let stop = root["hooks"]["stop"].as_array().unwrap();
        assert!(stop.iter().any(|e| e["command"] == "other-tool"));
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
        // The wrapper is on disk and executable.
        let hook = dir.join(HOOK_FILE);
        assert!(hook.is_file());
        assert!(std::fs::read_to_string(&hook).unwrap().contains(INTEGRATION_MARKER));

        // Re-running replaces our entries rather than duplicating them.
        install_at(dir.clone()).unwrap();
        let root = read(&hooks_path);
        for descriptor in HOOK_EVENTS {
            assert_eq!(
                managed_commands(&root, descriptor.event).len(),
                1,
                "{} must stay single after reinstall",
                descriptor.event
            );
        }
        assert!(root["hooks"]["stop"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["command"] == "other-tool"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_errors_when_the_config_dir_is_missing() {
        let dir = std::env::temp_dir().join(format!("lumux-cursor-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = install_at(dir).unwrap_err();
        assert!(
            err.to_string().contains("install the Cursor agent CLI first"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn session_end_clears_and_start_claims() {
        let map: std::collections::HashMap<_, _> = HOOK_EVENTS
            .iter()
            .map(|descriptor| (descriptor.event, descriptor.state))
            .collect();
        assert_eq!(map.get("sessionStart"), Some(&"idle"));
        assert_eq!(map.get("sessionEnd"), Some(&"clear"));
    }
}
