//! GitHub Copilot CLI lifecycle-hook adapter.
//!
//! Copilot loads user hook files from `~/.copilot/hooks/*.json`. Keeping lumux's
//! generated configuration in its own versioned file avoids mutating or making
//! the validity of the user's strict `settings.json` hooks block our concern.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::common;

#[cfg(not(windows))]
const INTEGRATION_MARKER: &str = "LUMUX_INTEGRATION_ID=copilot";
const CONFIG_FILE: &str = "lumux-agent-state.json";

#[cfg(not(windows))]
const WRAPPER_FILE: &str = "lumux-agent-state.sh";
#[cfg(windows)]
const WRAPPER_FILE: &str = "lumux-agent-state.ps1";

#[cfg(not(windows))]
const WRAPPER: &str = include_str!("assets/copilot/lumux-agent-state.sh");
#[cfg(windows)]
const WRAPPER: &str = include_str!("assets/copilot/lumux-agent-state.ps1");
#[cfg(test)]
const POWERSHELL_WRAPPER: &str = include_str!("assets/copilot/lumux-agent-state.ps1");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HookEvent {
    event: &'static str,
    action: &'static str,
    matcher: Option<&'static str>,
}

/// PascalCase selects Copilot's VS Code-compatible, snake_case payloads; the
/// notification event has only its native lowercase spelling. Explicit actions
/// keep the wrapper independent of payload heuristics for overlapping events.
const HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        event: "SessionStart",
        action: "session-start",
        matcher: None,
    },
    HookEvent {
        event: "UserPromptSubmit",
        action: "working",
        matcher: None,
    },
    HookEvent {
        event: "PreToolUse",
        action: "pre-tool",
        matcher: None,
    },
    HookEvent {
        event: "PermissionRequest",
        action: "blocked",
        matcher: None,
    },
    HookEvent {
        event: "PostToolUse",
        action: "post-tool",
        matcher: None,
    },
    HookEvent {
        event: "PostToolUseFailure",
        action: "post-tool",
        matcher: None,
    },
    HookEvent {
        event: "Stop",
        action: "stop",
        matcher: None,
    },
    HookEvent {
        event: "notification",
        action: "notification",
        matcher: Some("permission_prompt|elicitation_dialog|agent_idle"),
    },
    HookEvent {
        event: "SessionEnd",
        action: "session-end",
        matcher: None,
    },
];

pub(super) fn install() -> anyhow::Result<()> {
    install_at(copilot_dir()?)
}

fn copilot_dir() -> anyhow::Result<PathBuf> {
    copilot_dir_with(|key| std::env::var_os(key))
}

fn copilot_dir_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let (primary_home, fallback_home) = ("USERPROFILE", "HOME");
    #[cfg(not(windows))]
    let (primary_home, fallback_home) = ("HOME", "USERPROFILE");

    copilot_dir_with_home_keys(getenv, primary_home, fallback_home)
}

/// Copilot documents `%USERPROFILE%` as its Windows default even when an
/// MSYS-style `HOME` is also present. Adapt the shared path helper's two home
/// lookups to the provider's platform-specific priority.
fn copilot_dir_with_home_keys(
    getenv: impl Fn(&str) -> Option<OsString>,
    primary_home: &str,
    fallback_home: &str,
) -> anyhow::Result<PathBuf> {
    common::config_dir_with("COPILOT_HOME", ".copilot", |key| match key {
        "HOME" => getenv(primary_home)
            .filter(|value| !value.is_empty())
            .or_else(|| getenv(fallback_home).filter(|value| !value.is_empty())),
        // The ordered lookup above already consumed both candidates.
        "USERPROFILE" => None,
        other => getenv(other),
    })
}

fn install_at(dir: PathBuf) -> anyhow::Result<()> {
    let hooks_dir = dir.join("hooks");
    let wrapper_path = hooks_dir.join(WRAPPER_FILE);
    let config_path = hooks_dir.join(CONFIG_FILE);
    // Resolve the config target before writing the wrapper so a symlink cycle
    // or inaccessible target cannot leave a half-installed adapter.
    let config_write_path = common::resolve_write_path(&config_path)?;
    let mut hooks = Map::new();
    for descriptor in HOOK_EVENTS {
        append_hook(&mut hooks, &wrapper_path, descriptor);
    }
    let root = serde_json::json!({"version": 1, "hooks": hooks});

    common::write_managed_hook(&wrapper_path, WRAPPER)?;
    common::write_json_config(&config_path, &config_write_path, &root)?;

    println!(
        "installed GitHub Copilot CLI state hooks into {} ({} events)",
        config_path.display(),
        HOOK_EVENTS.len()
    );
    Ok(())
}

fn append_hook(hooks: &mut Map<String, Value>, wrapper_path: &Path, descriptor: &HookEvent) {
    hooks
        .entry(descriptor.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("generated hook event is always an array")
        .push(direct_hook(wrapper_path, descriptor));
}

#[cfg(not(windows))]
fn direct_hook(wrapper: &Path, descriptor: &HookEvent) -> serde_json::Value {
    hook_value(
        "bash",
        format!(
            "{INTEGRATION_MARKER} sh {} {} \"$PPID\"",
            super::shell_single_quote(&wrapper.display().to_string()),
            descriptor.action
        ),
        descriptor.matcher,
    )
}

#[cfg(windows)]
fn direct_hook(wrapper: &Path, descriptor: &HookEvent) -> serde_json::Value {
    hook_value(
        "powershell",
        powershell_hook_command(wrapper, descriptor.action),
        descriptor.matcher,
    )
}

fn hook_value(command_field: &str, command: String, matcher: Option<&str>) -> serde_json::Value {
    let mut hook = serde_json::json!({
        "type": "command",
        "timeoutSec": 10,
    });
    let object = hook
        .as_object_mut()
        .expect("the static command-hook template is an object");
    object.insert(command_field.to_string(), command.into());
    if let Some(matcher) = matcher {
        object.insert("matcher".to_string(), matcher.into());
    }
    hook
}

#[cfg(any(windows, test))]
fn powershell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(any(windows, test))]
fn powershell_hook_command(wrapper: &Path, action: &str) -> String {
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"$env:LUMUX_INTEGRATION_ID='copilot'; $hookShellPid=0; $parentPid=0; try {{ $hookShellPid=[int](Get-CimInstance Win32_Process -Filter ('ProcessId = ' + $PID) -ErrorAction Stop).ParentProcessId; $parentPid=[int](Get-CimInstance Win32_Process -Filter ('ProcessId = ' + $hookShellPid) -ErrorAction Stop).ParentProcessId }} catch {{}}; & {} {} $parentPid\"",
        powershell_single_quote(&wrapper.display().to_string()),
        powershell_single_quote(action)
    )
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
    fn copilot_home_overrides_home() {
        assert_eq!(
            copilot_dir_with(env_of(&[("COPILOT_HOME", "custom"), ("HOME", "ignored")])).unwrap(),
            PathBuf::from("custom")
        );
        assert_eq!(
            copilot_dir_with(env_of(&[("COPILOT_HOME", ""), ("HOME", "/home/me")])).unwrap(),
            PathBuf::from("/home/me/.copilot")
        );
        assert_eq!(
            copilot_dir_with(env_of(&[
                ("COPILOT_HOME", "~/custom"),
                ("HOME", "/home/me")
            ]))
            .unwrap(),
            PathBuf::from("/home/me/custom")
        );
    }

    #[test]
    fn windows_default_prefers_userprofile_over_msys_home() {
        assert_eq!(
            copilot_dir_with_home_keys(
                env_of(&[
                    ("USERPROFILE", r"C:\Users\me"),
                    ("HOME", r"C:\msys64\home\me"),
                ]),
                "USERPROFILE",
                "HOME",
            )
            .unwrap(),
            PathBuf::from(r"C:\Users\me").join(".copilot")
        );
        assert_eq!(
            copilot_dir_with_home_keys(
                env_of(&[
                    ("COPILOT_HOME", r"~\copilot-data"),
                    ("USERPROFILE", r"D:\Me")
                ]),
                "USERPROFILE",
                "HOME",
            )
            .unwrap(),
            PathBuf::from(r"D:\Me").join("copilot-data")
        );
    }

    #[test]
    fn powershell_command_literal_quotes_paths_and_actions() {
        let command = powershell_hook_command(
            Path::new(r"C:\Users\O'Brien\$profile`cache\hooks\state.ps1"),
            "session-start",
        );
        assert!(command.contains("Get-CimInstance Win32_Process"));
        assert!(command.contains("ParentProcessId"));
        assert!(command.contains(
            r"& 'C:\Users\O''Brien\$profile`cache\hooks\state.ps1' 'session-start' $parentPid"
        ));
    }

    #[test]
    fn powershell_wrapper_matches_exit_and_turn_boundary_mapping() {
        for expected in [
            r#"$Action -in @("stop", "idle")"#,
            r#"$stopReason -eq "end_turn""#,
            r#"$notification -eq "agent_idle""#,
            r#"$Action -in @("session-end", "clear")"#,
            "including complete/error/timeout",
            "StartTime.ToUniversalTime().Ticks",
            "$nativePidSupplied = $PSBoundParameters.ContainsKey(\"NativePid\")",
            "$nativePidSupplied -and $null -eq $nativeIdentity",
        ] {
            assert!(
                POWERSHELL_WRAPPER.contains(expected),
                "PowerShell wrapper is missing lifecycle mapping: {expected}"
            );
        }
    }

    #[test]
    fn install_writes_an_isolated_current_schema_and_never_touches_settings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("copilot");
        let hooks_dir = root.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let wrapper_path = hooks_dir.join(WRAPPER_FILE);
        let config_path = hooks_dir.join(CONFIG_FILE);
        let settings_path = root.join("settings.json");
        let settings = b"user-owned and intentionally not valid JSON\n";
        std::fs::write(&settings_path, settings).unwrap();
        std::fs::write(&config_path, b"obsolete managed schema\n").unwrap();

        install_at(root.clone()).unwrap();
        let first_config = std::fs::read(&config_path).unwrap();
        let first_wrapper = std::fs::read(&wrapper_path).unwrap();
        install_at(root).unwrap();
        assert_eq!(std::fs::read(&config_path).unwrap(), first_config);
        assert_eq!(std::fs::read(&wrapper_path).unwrap(), first_wrapper);
        assert_eq!(
            std::fs::read(&settings_path).unwrap(),
            settings,
            "the dedicated user hook file must isolate lumux from settings.json"
        );

        let config: Value = serde_json::from_slice(&first_config).unwrap();
        assert_eq!(config["version"], 1);
        let expected = [
            ("SessionStart", "session-start", None),
            ("UserPromptSubmit", "working", None),
            ("PreToolUse", "pre-tool", None),
            ("PermissionRequest", "blocked", None),
            ("PostToolUse", "post-tool", None),
            ("PostToolUseFailure", "post-tool", None),
            ("Stop", "stop", None),
            (
                "notification",
                "notification",
                Some("permission_prompt|elicitation_dialog|agent_idle"),
            ),
            ("SessionEnd", "session-end", None),
        ];
        assert_eq!(HOOK_EVENTS.len(), expected.len());
        assert_eq!(config["hooks"].as_object().unwrap().len(), expected.len());
        for (event, action, matcher) in expected {
            let entries = config["hooks"][event].as_array().unwrap();
            assert_eq!(entries.len(), 1, "expected one generated hook for {event}");
            let hook = &entries[0];
            assert_eq!(hook["type"], "command");
            assert_eq!(hook["timeoutSec"], 10);
            let command = hook[if cfg!(windows) { "powershell" } else { "bash" }]
                .as_str()
                .unwrap();
            assert!(command.contains(&wrapper_path.display().to_string()));
            assert!(command.contains(action));
            assert!(command.contains(if cfg!(windows) { "$parentPid" } else { "$PPID" }));
            assert!(hook.get("command").is_none());
            assert_eq!(hook.get("matcher").and_then(Value::as_str), matcher);
        }
    }

    #[cfg(unix)]
    #[test]
    fn invalid_managed_config_symlink_prevents_partial_wrapper_update() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("copilot");
        let hooks = root.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let wrapper = hooks.join(WRAPPER_FILE);
        let config = hooks.join(CONFIG_FILE);
        std::fs::write(&wrapper, b"old wrapper").unwrap();
        symlink(CONFIG_FILE, &config).unwrap();

        let error = install_at(root).unwrap_err().to_string();

        assert!(error.contains("symlink cycle"), "{error}");
        assert_eq!(std::fs::read(wrapper).unwrap(), b"old wrapper");
    }

    #[cfg(not(windows))]
    fn run_wrapper(
        wrapper: &Path,
        bin: &Path,
        log: &Path,
        action: &str,
        payload: &str,
        inside: bool,
        native_pid: Option<u32>,
    ) -> std::process::Output {
        let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let mut command = std::process::Command::new("sh");
        command
            .arg(wrapper)
            .arg(action)
            .env("PATH", path)
            .env("LUMUX_AGENT_OWNER", "inherited-owner")
            .env("LUMUX_AGENT_SEQUENCE", "7")
            .env("LUMUX_AGENT_CLAIM", "inherited-claim")
            .env("LUMUX_TEST_LOG", log)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(native_pid) = native_pid {
            command.arg(native_pid.to_string());
        }
        if inside {
            command.env("LUMUX", "daemon").env("LUMUX_PANE", "%8");
        } else {
            command.env_remove("LUMUX").env_remove("LUMUX_PANE");
        }
        let mut child = command.spawn().unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    #[cfg(not(windows))]
    #[test]
    fn wrapper_maps_payloads_and_never_disturbs_copilot() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("copilot");
        install_at(root.clone()).unwrap();
        let wrapper = root.join("hooks").join(WRAPPER_FILE);
        let bin = dir.path().join("bin");
        let log = dir.path().join("calls");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("lumux");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s|owner=%s|sequence=%s|claim=%s\\n' \"$*\" \"${LUMUX_AGENT_OWNER-}\" \"${LUMUX_AGENT_SEQUENCE-}\" \"${LUMUX_AGENT_CLAIM-}\" >>\"$LUMUX_TEST_LOG\"\nprintf noise\nprintf error >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();

        let cases = [
            (
                "session-start",
                r#"{"source":"startup","sessionId":"copilot-session"}"#,
                "idle",
                "1",
            ),
            (
                "session-start",
                r#"{"initialPrompt":"fix it","sessionId":"copilot-session"}"#,
                "working",
                "1",
            ),
            (
                "working",
                r#"{"prompt":"continue","sessionId":"copilot-session"}"#,
                "working",
                "1",
            ),
            (
                "pre-tool",
                r#"{"toolName":"ask_user","sessionId":"copilot-session"}"#,
                "blocked",
                "0",
            ),
            (
                "pre-tool",
                r#"{"toolName":"bash","sessionId":"copilot-session"}"#,
                "working",
                "0",
            ),
            (
                "blocked",
                r#"{"hook_event_name":"PermissionRequest","session_id":"copilot-session"}"#,
                "blocked",
                "0",
            ),
            (
                "notification",
                r#"{"notification_type":"permission_prompt","sessionId":"copilot-session"}"#,
                "blocked",
                "0",
            ),
            (
                "notification",
                r#"{"notification_type":"elicitation_dialog","sessionId":"copilot-session"}"#,
                "blocked",
                "0",
            ),
            (
                "notification",
                r#"{"notification_type":"agent_idle","sessionId":"copilot-session"}"#,
                "idle",
                "0",
            ),
            (
                "post-tool",
                r#"{"toolName":"bash","sessionId":"copilot-session"}"#,
                "working",
                "0",
            ),
            (
                "stop",
                r#"{"stopReason":"end_turn","sessionId":"copilot-session"}"#,
                "idle",
                "0",
            ),
            ("stop", r#"{"sessionId":"copilot-session"}"#, "idle", "0"),
            (
                "session-end",
                r#"{"reason":"user_exit","sessionId":"copilot-session"}"#,
                "clear",
                "0",
            ),
            (
                "session-end",
                r#"{"reason":"abort","sessionId":"copilot-session"}"#,
                "clear",
                "0",
            ),
            (
                "session-end",
                r#"{"reason":"complete","sessionId":"copilot-session"}"#,
                "clear",
                "0",
            ),
            (
                "session-end",
                r#"{"reason":"error","sessionId":"copilot-session"}"#,
                "clear",
                "0",
            ),
            (
                "session-end",
                r#"{"reason":"timeout","sessionId":"copilot-session"}"#,
                "clear",
                "0",
            ),
        ];
        for (action, payload, state, expected_claim) in cases {
            let output = run_wrapper(&wrapper, &bin, &log, action, payload, true, None);
            assert!(output.status.success());
            assert!(output.stdout.is_empty());
            assert!(output.stderr.is_empty());
            let calls = std::fs::read_to_string(&log).unwrap();
            let invocation = calls.lines().last().unwrap();
            let (command, metadata) = invocation.split_once("|owner=").unwrap();
            let (owner, ordering) = metadata.split_once("|sequence=").unwrap();
            let (sequence, claim) = ordering.split_once("|claim=").unwrap();
            assert_eq!(command, format!("report-state {state} --agent copilot"));
            assert_eq!(owner, "copilot-session");
            assert!(
                sequence
                    .parse::<u64>()
                    .is_ok_and(|sequence| sequence > 0 && sequence != 7),
                "wrapper must capture a numeric event-start sequence: {sequence:?}"
            );
            assert_eq!(claim, expected_claim, "wrong claim for action {action}");
        }

        let before = std::fs::read(&log).unwrap();
        for (action, inside, payload) in [
            ("pre-tool", true, "not-json"),
            ("pre-tool", false, "{}"),
            ("working", true, r#"{"prompt":"missing session id"}"#),
            (
                "post-tool",
                true,
                r#"{"toolName":"report_intent","sessionId":"copilot-session"}"#,
            ),
            (
                "notification",
                true,
                r#"{"notification_type":"shell_completed","sessionId":"copilot-session"}"#,
            ),
            (
                "stop",
                true,
                r#"{"stopReason":"error","sessionId":"copilot-session"}"#,
            ),
            ("unknown", true, "{}"),
        ] {
            let output = run_wrapper(&wrapper, &bin, &log, action, payload, inside, None);
            assert!(output.status.success());
            assert!(output.stdout.is_empty());
            assert!(output.stderr.is_empty());
        }
        assert_eq!(std::fs::read(&log).unwrap(), before);

        for unknown_pid in [0, u32::MAX] {
            let unknown_generation = run_wrapper(
                &wrapper,
                &bin,
                &log,
                "working",
                r#"{"prompt":"continue","sessionId":"copilot-session"}"#,
                true,
                Some(unknown_pid),
            );
            assert!(unknown_generation.status.success());
            assert!(unknown_generation.stdout.is_empty());
            assert!(unknown_generation.stderr.is_empty());
            assert_eq!(
                std::fs::read(&log).unwrap(),
                before,
                "an explicitly supplied pid without a process identity must not fall back to a bare owner"
            );
        }

        // Resuming the same Copilot session in a new CLI process must be a new
        // lifecycle generation; a delayed SessionEnd from the first process
        // must not match and clear the replacement.
        let mut first_process = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let first = run_wrapper(
            &wrapper,
            &bin,
            &log,
            "working",
            r#"{"prompt":"first","sessionId":"reused-session"}"#,
            true,
            Some(first_process.id()),
        );
        assert!(first.status.success());
        let mut replacement_process = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let replacement = run_wrapper(
            &wrapper,
            &bin,
            &log,
            "working",
            r#"{"prompt":"replacement","sessionId":"reused-session"}"#,
            true,
            Some(replacement_process.id()),
        );
        assert!(replacement.status.success());
        let calls = std::fs::read_to_string(&log).unwrap();
        let generation_owners: Vec<_> = calls
            .lines()
            .rev()
            .take(2)
            .map(|line| {
                line.split("|owner=")
                    .nth(1)
                    .and_then(|tail| tail.split("|sequence=").next())
                    .unwrap()
            })
            .collect();
        assert_eq!(generation_owners.len(), 2);
        assert!(generation_owners
            .iter()
            .all(|owner| owner.starts_with("reused-session@")));
        assert_ne!(generation_owners[0], generation_owners[1]);
        first_process.kill().unwrap();
        first_process.wait().unwrap();
        replacement_process.kill().unwrap();
        replacement_process.wait().unwrap();
    }
}
