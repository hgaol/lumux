//! Codex CLI lifecycle-hook adapter.
//!
//! Codex uses Claude-style nested hook groups in `hooks.json`, but has its own
//! config root and feature flag. The adapter deliberately stays behind the
//! generic `lumux report-state` interface; no Codex concepts enter the daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::common;

const INTEGRATION_MARKER: &str = "LUMUX_INTEGRATION_ID=codex";

#[cfg(not(windows))]
const HOOK_FILE: &str = "lumux-agent-state.sh";
#[cfg(windows)]
const HOOK_FILE: &str = "lumux-agent-state.ps1";

#[cfg(not(windows))]
const HOOK_WRAPPER: &str = include_str!("assets/codex/lumux-agent-state.sh");
#[cfg(windows)]
const HOOK_WRAPPER: &str = include_str!("assets/codex/lumux-agent-state.ps1");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HookEvent {
    event: &'static str,
    state: &'static str,
    matcher: Option<&'static str>,
    /// SessionStart launches the exit watcher. Every event still receives the
    /// native parent pid so its owner includes a process generation; a watcher
    /// from an earlier process cannot clear a resumed process with the same
    /// Codex session id.
    watch_parent: bool,
}

/// Only events that represent stable semantic transitions are installed.
/// `PostToolUse` returns a pane from permission-waiting to active work; `Stop`
/// is the conclusive end-of-turn signal. Codex currently exposes no documented
/// SessionEnd hook, so SessionStart also launches an identity-checked
/// process-exit watcher.
const HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        event: "SessionStart",
        state: "idle",
        // A compact can occur in the middle of active work and must not paint
        // the pane idle. Startup/resume/clear all present an input prompt.
        matcher: Some("startup|resume|clear"),
        watch_parent: true,
    },
    HookEvent {
        event: "UserPromptSubmit",
        state: "working",
        matcher: None,
        watch_parent: false,
    },
    HookEvent {
        event: "PreToolUse",
        state: "working",
        matcher: None,
        watch_parent: false,
    },
    HookEvent {
        event: "PermissionRequest",
        state: "blocked",
        matcher: None,
        watch_parent: false,
    },
    HookEvent {
        event: "PostToolUse",
        state: "working",
        matcher: None,
        watch_parent: false,
    },
    HookEvent {
        event: "Stop",
        state: "idle",
        matcher: None,
        watch_parent: false,
    },
];

pub(super) fn install() -> anyhow::Result<()> {
    install_at(codex_dir()?)
}

fn codex_dir() -> anyhow::Result<PathBuf> {
    codex_dir_with(|key| std::env::var_os(key))
}

fn codex_dir_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    common::config_dir_with("CODEX_HOME", ".codex", getenv)
}

fn install_at(dir: PathBuf) -> anyhow::Result<()> {
    let hooks_path = dir.join("hooks.json");
    let config_path = dir.join("config.toml");
    let hook_path = dir.join(HOOK_FILE);

    // Parse every user-owned config before creating the managed wrapper. A bad
    // file therefore cannot leave a half-installed integration behind.
    let (hooks_write_path, mut root) = common::read_json_config(&hooks_path)?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", hooks_path.display()))?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` in {} is not an object", hooks_path.display()))?;
    common::prune_managed_nested_hooks(hooks, is_managed_codex_command);
    for descriptor in HOOK_EVENTS {
        append_hook(hooks, &hook_path, *descriptor)?;
    }

    let config = read_and_enable_hooks(&config_path)?;

    common::write_managed_hook(&hook_path, HOOK_WRAPPER)?;
    common::write_json_config(&hooks_path, &hooks_write_path, &root)?;
    common::write_config_text(&config_path, &config)?;

    println!(
        "installed Codex state hooks into {} ({} events)",
        hooks_path.display(),
        HOOK_EVENTS.len()
    );
    Ok(())
}

fn append_hook(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
    descriptor: HookEvent,
) -> anyhow::Result<()> {
    let command = hook_command(hook_path, descriptor);
    let mut group = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 10,
        }]
    });
    if let Some(matcher) = descriptor.matcher {
        group
            .as_object_mut()
            .expect("hook group is constructed as an object")
            .insert("matcher".to_string(), matcher.into());
    }
    hooks
        .entry(descriptor.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("hook entries for {} must be an array", descriptor.event))?
        .push(group);
    Ok(())
}

fn is_managed_codex_command(command: &Value) -> bool {
    command
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(INTEGRATION_MARKER))
}

#[cfg(not(windows))]
fn hook_command(hook_path: &Path, descriptor: HookEvent) -> String {
    format!(
        "{INTEGRATION_MARKER} sh {} {} \"$PPID\"",
        super::shell_single_quote(&hook_path.display().to_string()),
        descriptor.state,
    )
}

#[cfg(windows)]
fn hook_command(hook_path: &Path, descriptor: HookEvent) -> String {
    let path = hook_path.display().to_string().replace('\'', "''");
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"$env:LUMUX_INTEGRATION_ID='codex'; $hookShellPid=0; $parentPid=0; try {{ $hookShellPid=[int](Get-CimInstance Win32_Process -Filter ('ProcessId = ' + $PID) -ErrorAction Stop).ParentProcessId; $parentPid=[int](Get-CimInstance Win32_Process -Filter ('ProcessId = ' + $hookShellPid) -ErrorAction Stop).ParentProcessId }} catch {{}}; & '{path}' '{}' $parentPid\"",
        descriptor.state
    )
}

fn read_and_enable_hooks(path: &Path) -> anyhow::Result<String> {
    let write_path = common::resolve_write_path(path)?;
    let existing = match std::fs::read_to_string(&write_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to read {}: {error}",
                path.display()
            ));
        }
    };
    enable_hooks_feature(&existing)
        .map_err(|error| anyhow::anyhow!("failed to update {}: {error}", path.display()))
}

/// Preserve comments, ordering, profiles, and unrelated feature flags while
/// enabling only the top-level Codex hooks feature. The deprecated key is
/// removed from that table, never from nested profile feature tables.
fn enable_hooks_feature(content: &str) -> anyhow::Result<String> {
    use toml_edit::{value, DocumentMut, Item, Table, Value as TomlValue};

    fn set_enabled_preserving_decor(current: &mut TomlValue) {
        let decor = current.decor().clone();
        let mut enabled = TomlValue::from(true);
        *enabled.decor_mut() = decor;
        *current = enabled;
    }

    let mut document = content
        .parse::<DocumentMut>()
        .map_err(|error| anyhow::anyhow!("Codex config is not valid TOML: {error}"))?;
    let features = document
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()));
    if let Some(table) = features.as_table_mut() {
        table.remove("codex_hooks");
        if let Some(hooks) = table.get_mut("hooks") {
            let hooks = hooks
                .as_value_mut()
                .ok_or_else(|| anyhow::anyhow!("top-level `features.hooks` must be a value"))?;
            set_enabled_preserving_decor(hooks);
        } else {
            table.insert("hooks", value(true));
        }
    } else if let Some(inline) = features
        .as_value_mut()
        .and_then(TomlValue::as_inline_table_mut)
    {
        inline.remove("codex_hooks");
        if let Some(hooks) = inline.get_mut("hooks") {
            set_enabled_preserving_decor(hooks);
        } else {
            inline.insert("hooks", TomlValue::from(true));
        }
    } else {
        anyhow::bail!("top-level `features` in Codex config must be a table");
    }
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    #[test]
    fn codex_home_honors_override_default_and_platform_home_fallback() {
        assert_eq!(
            codex_dir_with(env_of(&[("CODEX_HOME", "custom"), ("HOME", "/home/me")])).unwrap(),
            PathBuf::from("custom")
        );
        assert_eq!(
            codex_dir_with(env_of(&[("CODEX_HOME", "~/agent"), ("HOME", "/home/me")])).unwrap(),
            PathBuf::from("/home/me/agent")
        );
        assert_eq!(
            codex_dir_with(env_of(&[("CODEX_HOME", "~"), ("HOME", "/home/me")])).unwrap(),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            codex_dir_with(env_of(&[("CODEX_HOME", ""), ("HOME", "/home/me")])).unwrap(),
            PathBuf::from("/home/me/.codex")
        );
        assert_eq!(
            codex_dir_with(env_of(&[("USERPROFILE", "C:\\Users\\me")])).unwrap(),
            PathBuf::from("C:\\Users\\me").join(".codex")
        );
    }

    #[test]
    fn hook_mapping_covers_state_transitions_without_compact_idle() {
        assert_eq!(
            HOOK_EVENTS,
            &[
                HookEvent {
                    event: "SessionStart",
                    state: "idle",
                    matcher: Some("startup|resume|clear"),
                    watch_parent: true,
                },
                HookEvent {
                    event: "UserPromptSubmit",
                    state: "working",
                    matcher: None,
                    watch_parent: false,
                },
                HookEvent {
                    event: "PreToolUse",
                    state: "working",
                    matcher: None,
                    watch_parent: false,
                },
                HookEvent {
                    event: "PermissionRequest",
                    state: "blocked",
                    matcher: None,
                    watch_parent: false,
                },
                HookEvent {
                    event: "PostToolUse",
                    state: "working",
                    matcher: None,
                    watch_parent: false,
                },
                HookEvent {
                    event: "Stop",
                    state: "idle",
                    matcher: None,
                    watch_parent: false,
                },
            ]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn every_command_passes_native_pid_while_only_session_start_launches_a_watcher() {
        let path = Path::new("/tmp/codex hooks/state.sh");
        for descriptor in HOOK_EVENTS {
            let command = hook_command(path, *descriptor);
            assert!(
                command.contains("\"$PPID\""),
                "{} lacks a process-generation pid: {command}",
                descriptor.event
            );
        }
        assert_eq!(
            HOOK_EVENTS
                .iter()
                .filter(|descriptor| descriptor.watch_parent)
                .map(|descriptor| descriptor.event)
                .collect::<Vec<_>>(),
            vec!["SessionStart"]
        );
    }

    #[test]
    fn windows_wrapper_tracks_process_identity_and_emits_an_owned_clear() {
        let wrapper = include_str!("assets/codex/lumux-agent-state.ps1");
        for expected in [
            "$candidate.StartTime.ToUniversalTime().Ticks -eq $watchIdentity",
            "$nativePidSupplied = $PSBoundParameters.ContainsKey(\"NativePid\")",
            "$nativePidSupplied -and $null -eq $nativeIdentity",
            "LUMUX_CODEX_WATCH_PID",
            "LUMUX_CODEX_WATCH_IDENTITY",
            "LUMUX_AGENT_OWNER",
            "$env:LUMUX_BIN",
            "& $lumuxBin report-state clear --agent codex",
            "RedirectStandardInput = $true",
            "RedirectStandardOutput = $true",
            "RedirectStandardError = $true",
        ] {
            assert!(
                wrapper.contains(expected),
                "PowerShell exit watcher is missing {expected:?}"
            );
        }
    }

    #[test]
    fn install_preserves_foreign_hooks_and_toml_layout_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("hooks.json"),
            r#"{"description":"keep","hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo keep"}]}],"Legacy":[{"hooks":[{"type":"command","command":"LUMUX_INTEGRATION_ID=codex old"},{"type":"command","command":"echo sibling"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("config.toml"),
            "model = \"gpt-5\"\n\n[profiles.work.features]\nhooks = false\ncodex_hooks = false\n\n[features]\ncodex_hooks = false\nother = true\n",
        )
        .unwrap();

        install_at(root.clone()).unwrap();
        let first_hooks = std::fs::read(root.join("hooks.json")).unwrap();
        let first_config = std::fs::read(root.join("config.toml")).unwrap();
        let first_wrapper = std::fs::read(root.join(HOOK_FILE)).unwrap();
        install_at(root.clone()).unwrap();

        assert_eq!(std::fs::read(root.join("hooks.json")).unwrap(), first_hooks);
        assert_eq!(
            std::fs::read(root.join("config.toml")).unwrap(),
            first_config
        );
        assert_eq!(std::fs::read(root.join(HOOK_FILE)).unwrap(), first_wrapper);

        let hooks: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("hooks.json")).unwrap())
                .unwrap();
        assert_eq!(hooks["description"], "keep");
        assert_eq!(hooks["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            hooks["hooks"]["Legacy"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for descriptor in HOOK_EVENTS {
            let groups = hooks["hooks"][descriptor.event].as_array().unwrap();
            let managed: Vec<_> = groups
                .iter()
                .filter(|group| {
                    group["hooks"][0]["command"]
                        .as_str()
                        .is_some_and(|command| command.contains(INTEGRATION_MARKER))
                })
                .collect();
            assert_eq!(
                managed.len(),
                1,
                "{} should have one managed hook",
                descriptor.event
            );
            let group = managed[0];
            match descriptor.matcher {
                Some(matcher) => assert_eq!(group["matcher"].as_str(), Some(matcher)),
                None => assert!(group.get("matcher").is_none()),
            }
            let handler = &group["hooks"][0];
            assert_eq!(handler["type"].as_str(), Some("command"));
            assert_eq!(handler["timeout"].as_u64(), Some(10));
            let expected_command = hook_command(&root.join(HOOK_FILE), *descriptor);
            assert_eq!(handler["command"].as_str(), Some(expected_command.as_str()));
        }
        let config = std::fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(config.contains("[profiles.work.features]\nhooks = false\ncodex_hooks = false"));
        let parsed: toml::Value = toml::from_str(&config).unwrap();
        assert_eq!(parsed["features"]["hooks"].as_bool(), Some(true));
        assert_eq!(parsed["features"]["other"].as_bool(), Some(true));
        assert!(parsed["features"].get("codex_hooks").is_none());
        assert_eq!(config.matches("hooks = true").count(), 1);
    }

    #[test]
    fn inline_features_are_updated_without_reformatting_the_document() {
        let updated = enable_hooks_feature(
            "model = \"gpt-5\" # keep this comment\nfeatures = { codex_hooks = false, other = true }\n",
        )
        .unwrap();
        assert!(updated.contains("model = \"gpt-5\" # keep this comment"));
        let parsed: toml::Value = toml::from_str(&updated).unwrap();
        assert_eq!(parsed["features"]["hooks"].as_bool(), Some(true));
        assert_eq!(parsed["features"]["other"].as_bool(), Some(true));
        assert!(parsed["features"].get("codex_hooks").is_none());
    }

    #[test]
    fn existing_hooks_setting_keeps_its_comments() {
        let updated = enable_hooks_feature(
            "[features]\n# Needed by another integration.\nhooks = false # keep this explanation\nother = true\n",
        )
        .unwrap();
        assert!(updated
            .contains("# Needed by another integration.\nhooks = true # keep this explanation"));
    }

    #[test]
    fn invalid_config_prevents_partial_install() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("hooks.json"), b"not json").unwrap();
        let error = install_at(root.clone()).unwrap_err();
        assert!(error.to_string().contains("valid JSON"));
        assert!(!root.join(HOOK_FILE).exists());
        assert_eq!(std::fs::read(root.join("hooks.json")).unwrap(), b"not json");
    }

    #[test]
    fn invalid_toml_prevents_every_partial_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        let hooks = br#"{"description":"unchanged","hooks":{}}"#;
        let config = b"model = [\"unterminated\"\n";
        let wrapper = b"user-owned placeholder\n";
        std::fs::write(root.join("hooks.json"), hooks).unwrap();
        std::fs::write(root.join("config.toml"), config).unwrap();
        std::fs::write(root.join(HOOK_FILE), wrapper).unwrap();

        let error = install_at(root.clone()).unwrap_err();
        assert!(error.to_string().contains("not valid TOML"));
        assert_eq!(std::fs::read(root.join("hooks.json")).unwrap(), hooks);
        assert_eq!(std::fs::read(root.join("config.toml")).unwrap(), config);
        assert_eq!(std::fs::read(root.join(HOOK_FILE)).unwrap(), wrapper);
    }

    #[cfg(not(windows))]
    fn run_wrapper(
        wrapper: &Path,
        reporter: &Path,
        log: &Path,
        state: &str,
        payload: &str,
        native_pid: Option<u32>,
    ) -> std::process::Output {
        let mut command = std::process::Command::new("sh");
        command.arg(wrapper).arg(state);
        if let Some(native_pid) = native_pid {
            command.arg(native_pid.to_string());
        }
        let mut child = command
            .env("LUMUX", "daemon")
            .env("LUMUX_PANE", "%7")
            .env("LUMUX_AGENT_OWNER", "inherited-owner")
            .env("LUMUX_AGENT_SEQUENCE", "7")
            .env("LUMUX_AGENT_CLAIM", "inherited-claim")
            .env("LUMUX_TEST_LOG", log)
            .env("LUMUX_BIN", reporter)
            // The provider hook must not depend on the user's interactive
            // PATH containing lumux. Keep only system tools needed by the
            // wrapper; the fake reporter is intentionally outside this PATH.
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    #[cfg(not(windows))]
    #[test]
    fn wrapper_is_silent_and_best_effort_inside_or_outside_lumux() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        install_at(root.clone()).unwrap();
        let wrapper = root.join(HOOK_FILE);
        let outside = std::process::Command::new("sh")
            .arg(&wrapper)
            .arg("idle")
            .env_remove("LUMUX")
            .env_remove("LUMUX_PANE")
            .output()
            .unwrap();
        assert!(outside.status.success());
        assert!(outside.stdout.is_empty());
        assert!(outside.stderr.is_empty());

        let bin = dir.path().join("bin");
        let log = dir.path().join("calls");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("lumux");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s|owner=%s|sequence=%s|claim=%s\\n' \"$*\" \"${LUMUX_AGENT_OWNER-}\" \"${LUMUX_AGENT_SEQUENCE-}\" \"${LUMUX_AGENT_CLAIM-}\" >\"$LUMUX_TEST_LOG\"\nprintf noise\nexit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        let output = run_wrapper(
            &wrapper,
            &fake,
            &log,
            "blocked",
            r#"{"hook_event_name":"PermissionRequest","session_id":"codex-session"}"#,
            None,
        );
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        let invocation = std::fs::read_to_string(&log).unwrap();
        let (command, metadata) = invocation.trim().split_once("|owner=").unwrap();
        let (owner, ordering) = metadata.split_once("|sequence=").unwrap();
        let (sequence, claim) = ordering.split_once("|claim=").unwrap();
        assert_eq!(command, "report-state blocked --agent codex");
        assert_eq!(owner, "codex-session");
        assert!(
            sequence
                .parse::<u64>()
                .is_ok_and(|sequence| sequence > 0 && sequence != 7),
            "wrapper must capture a numeric event-start sequence: {sequence:?}"
        );
        assert_eq!(claim, "0", "permission hooks must not claim ownership");

        std::fs::remove_file(&log).unwrap();
        let ownerless = run_wrapper(
            &wrapper,
            &fake,
            &log,
            "working",
            r#"{"hook_event_name":"UserPromptSubmit"}"#,
            None,
        );
        assert!(ownerless.status.success());
        assert!(
            !log.exists(),
            "a provider report without a session id must not replace owned state"
        );

        let foreground = run_wrapper(
            &wrapper,
            &fake,
            &log,
            "working",
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"codex-session"}"#,
            None,
        );
        assert!(foreground.status.success());
        let invocation = std::fs::read_to_string(&log).unwrap();
        assert!(
            invocation.trim().ends_with("|claim=1"),
            "UserPromptSubmit must claim lifecycle ownership: {invocation:?}"
        );

        std::fs::remove_file(&log).unwrap();
        for unknown_pid in [0, u32::MAX] {
            let unknown_generation = run_wrapper(
                &wrapper,
                &fake,
                &log,
                "working",
                r#"{"hook_event_name":"UserPromptSubmit","session_id":"codex-session"}"#,
                Some(unknown_pid),
            );
            assert!(unknown_generation.status.success());
            assert!(unknown_generation.stdout.is_empty());
            assert!(unknown_generation.stderr.is_empty());
            assert!(
                !log.exists(),
                "an explicitly supplied pid without a process identity must not fall back to a bare owner"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn session_start_returns_promptly_then_clears_after_native_process_exit() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        install_at(root.clone()).unwrap();
        let wrapper = root.join(HOOK_FILE);

        let bin = dir.path().join("bin");
        let log = dir.path().join("calls");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("lumux");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s|owner=%s|sequence=%s|claim=%s\\n' \"$*\" \"${LUMUX_AGENT_OWNER-}\" \"${LUMUX_AGENT_SEQUENCE-}\" \"${LUMUX_AGENT_CLAIM-}\" >>\"$LUMUX_TEST_LOG\"\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        // This long-lived process stands in for the native Codex binary. The
        // hook must not wait for it; only the detached watcher does.
        let mut native = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let started = Instant::now();
        let output = run_wrapper(
            &wrapper,
            &fake,
            &log,
            "idle",
            r#"{"hook_event_name":"SessionStart","session_id":"codex-session"}"#,
            Some(native.id()),
        );
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "SessionStart waited for the native process instead of detaching"
        );

        std::thread::sleep(Duration::from_millis(350));
        let before_exit = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            before_exit.lines().count(),
            1,
            "the watcher cleared while its exact native process was alive: {before_exit:?}"
        );
        assert!(before_exit.starts_with("report-state idle --agent codex|owner=codex-session"));

        // A resumed native process may reuse the same provider session id. Its
        // process generation must still claim a different owner before the old
        // watcher observes the old process exit.
        let mut replacement = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let replacement_report = run_wrapper(
            &wrapper,
            &fake,
            &log,
            "working",
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"codex-session"}"#,
            Some(replacement.id()),
        );
        assert!(replacement_report.status.success());
        let before_old_exit = std::fs::read_to_string(&log).unwrap();
        let before_lines: Vec<_> = before_old_exit.lines().collect();
        assert_eq!(before_lines.len(), 2, "{before_old_exit:?}");
        let owner = |line: &str| {
            line.split("|owner=")
                .nth(1)
                .and_then(|tail| tail.split("|sequence=").next())
                .unwrap()
                .to_string()
        };
        let original_owner = owner(before_lines[0]);
        let replacement_owner = owner(before_lines[1]);
        assert!(original_owner.starts_with("codex-session@"));
        assert!(replacement_owner.starts_with("codex-session@"));
        assert_ne!(
            original_owner, replacement_owner,
            "same session id in a new process needs a new lifecycle generation"
        );

        native.kill().unwrap();
        native.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let calls = loop {
            let calls = std::fs::read_to_string(&log).unwrap_or_default();
            if calls.lines().count() >= 3 || Instant::now() >= deadline {
                break calls;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let lines: Vec<_> = calls.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "the detached watcher did not emit exactly one exit clear: {calls:?}"
        );
        assert!(lines[0].starts_with("report-state idle --agent codex|owner=codex-session"));
        assert!(lines[0].ends_with("|claim=1"));
        assert!(lines[1].starts_with("report-state working --agent codex|owner=codex-session"));
        assert!(lines[1].ends_with("|claim=1"));
        assert!(lines[2].starts_with("report-state clear --agent codex|owner=codex-session"));
        assert!(lines[2].ends_with("|claim=0"));
        assert_eq!(
            owner(lines[2]),
            original_owner,
            "old watcher must retain the old process generation: {calls:?}"
        );
        assert_ne!(owner(lines[2]), replacement_owner);

        let sequence = |line: &str| {
            line.split("|sequence=")
                .nth(1)
                .and_then(|tail| tail.split('|').next())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap()
        };
        assert!(
            sequence(lines[2]) > sequence(lines[0]),
            "exit clear must be newer than SessionStart: {calls:?}"
        );
        replacement.kill().unwrap();
        replacement.wait().unwrap();
    }
}
