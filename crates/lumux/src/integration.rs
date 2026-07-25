//! `lumux report-state` payload building and `lumux integration <agent>` hook
//! installation.
//!
//! Agents self-report their state (idle/working/blocked/done) so the sidebar
//! and session chooser can show it. `report-state` turns a state name plus the
//! `$LUMUX_PANE` the shell was spawned with into a [`ReportAgentState`] command;
//! `integration <agent>` installs a provider adapter that calls `report-state`
//! at the agent's native lifecycle points.

mod codex;
mod common;
mod copilot;
mod cursor;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lumux_core::agent::{AgentClear, AgentIdentity, AgentReport, AgentState};
use lumux_core::model::PaneId;
use lumux_core::proto::Command as Cmd;

/// Build the `ReportAgentState` command for `lumux report-state`. Reads the
/// target pane from `$LUMUX_PANE` (set on every pane the daemon spawns), the
/// agent label from `--agent`, else `$LUMUX_AGENT`, else `"agent"`, and optional
/// lifecycle ownership metadata captured by the provider wrapper. `getenv` is
/// injected so this is unit-testable without touching the process environment.
pub fn build_report_command(
    state: &str,
    agent: Option<&str>,
    getenv: impl Fn(&str) -> Option<OsString>,
) -> anyhow::Result<Option<Cmd>> {
    build_report_command_at(state, agent, getenv, report_sequence())
}

/// Build a report with a fallback sequence. Provider wrappers pass an
/// event-start timestamp through `$LUMUX_AGENT_SEQUENCE`; the fallback keeps
/// direct/manual invocations ordered. Keeping the clock behind this private
/// seam makes command construction deterministic in unit tests.
fn build_report_command_at(
    state: &str,
    agent: Option<&str>,
    getenv: impl Fn(&str) -> Option<OsString>,
    sequence: u64,
) -> anyhow::Result<Option<Cmd>> {
    let pane = env_text(&getenv, "LUMUX_PANE");
    // Claude's user-level hooks also run in terminals that are not owned by
    // lumux. Telemetry is inapplicable there, not an error: match herdr's
    // best-effort hook contract and exit silently.
    let Some(pane) = pane else {
        return Ok(None);
    };
    let pane: PaneId = pane
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid $LUMUX_PANE: {error}"))?;
    let sequence = match env_text(&getenv, "LUMUX_AGENT_SEQUENCE") {
        Some(sequence) => sequence.parse::<u64>().map_err(|error| {
            anyhow::anyhow!("invalid $LUMUX_AGENT_SEQUENCE {sequence:?}: {error}")
        })?,
        None => sequence,
    };
    let agent = agent
        .map(str::to_string)
        .or_else(|| env_text(&getenv, "LUMUX_AGENT"))
        .filter(|agent| !agent.is_empty())
        .unwrap_or_else(|| "agent".to_string());
    let owner = env_text(&getenv, "LUMUX_AGENT_OWNER").filter(|owner| !owner.trim().is_empty());
    // `clear` removes the pane from the agents list (the agent exited but the
    // shell/pane lives on, so nothing else would drop it).
    if state.eq_ignore_ascii_case("clear") {
        return Ok(Some(Cmd::ClearAgentState {
            pane,
            clear: AgentClear::new(AgentIdentity::new(agent, owner), sequence),
        }));
    }
    let claim = match env_text(&getenv, "LUMUX_AGENT_CLAIM") {
        Some(value) if value == "1" || value.eq_ignore_ascii_case("true") => true,
        Some(value) if value == "0" || value.eq_ignore_ascii_case("false") => false,
        Some(value) => {
            anyhow::bail!("invalid $LUMUX_AGENT_CLAIM {value:?}: expected 1, 0, true, or false")
        }
        // Reports without a provider session are direct/manual assertions and
        // must remain able to replace stale lifecycle-owned state. Provider
        // wrappers always set this explicitly.
        None => owner.is_none(),
    };
    let state: AgentState = state
        .parse()
        .map_err(|e: lumux_core::agent::AgentStateParseError| anyhow::anyhow!(e.to_string()))?;
    Ok(Some(Cmd::ReportAgentState {
        pane,
        report: AgentReport::new(AgentIdentity::new(agent, owner), claim, state, sequence),
    }))
}

fn env_text(getenv: &impl Fn(&str) -> Option<OsString>, key: &str) -> Option<String> {
    getenv(key)
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
}

/// Sequence reports emitted by independent hook processes. A timestamp is used
/// instead of process-local counters because each Claude event starts a fresh
/// wrapper/CLI process; the daemon retains the latest value as a tombstone.
fn report_sequence() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// A Claude lifecycle hook installed by lumux.
///
/// `matcher` is absent for events whose hook schema does not support one. This
/// distinction matters: Claude rejects or inconsistently handles wildcard
/// matchers on matcher-less events such as `Stop` and `UserPromptSubmit`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaudeHookEvent {
    pub event: &'static str,
    pub state: &'static str,
    pub matcher: Option<&'static str>,
}

/// Claude lifecycle-event → reported-state descriptors. Kept as data so the
/// event, state, and matcher contract can be asserted together in tests and
/// rendered into settings without event-specific installer branches.
pub const CLAUDE_HOOK_EVENTS: &[ClaudeHookEvent] = &[
    // The agent just launched → show it immediately (idle until the first
    // prompt), so it appears in the sidebar the moment Claude Code starts.
    ClaudeHookEvent {
        event: "SessionStart",
        state: "idle",
        matcher: Some("*"),
    },
    // The user submitted a prompt / a tool is about to run → the agent is busy.
    ClaudeHookEvent {
        event: "UserPromptSubmit",
        state: "working",
        matcher: None,
    },
    ClaudeHookEvent {
        event: "PreToolUse",
        state: "working",
        matcher: Some("*"),
    },
    // Permission prompts and notifications that require user input → blocked.
    ClaudeHookEvent {
        event: "PermissionRequest",
        state: "blocked",
        matcher: Some("*"),
    },
    ClaudeHookEvent {
        event: "Notification",
        state: "blocked",
        matcher: Some("permission_prompt|idle_prompt|elicitation_dialog|agent_needs_input"),
    },
    // Whether the turn succeeds or fails, the prompt is available again → idle.
    ClaudeHookEvent {
        event: "Stop",
        state: "idle",
        matcher: None,
    },
    ClaudeHookEvent {
        event: "StopFailure",
        state: "idle",
        matcher: Some("*"),
    },
    // The agent exited but its pane/shell stays alive, so remove it from the
    // agents list (nothing else would clear it — close_pane only fires on a
    // pane's own death).
    ClaudeHookEvent {
        event: "SessionEnd",
        state: "clear",
        matcher: Some("*"),
    },
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
# LUMUX_INTEGRATION_VERSION=5

resolve_lumux_bin() {
  lumux_bin="${LUMUX_BIN:-}"
  if [ -z "$lumux_bin" ]; then
    lumux_bin="$(command -v lumux 2>/dev/null)" || return 1
  fi
  case "$lumux_bin" in
    /*) ;;
    *) return 1 ;;
  esac
  [ -x "$lumux_bin" ] || return 1
  LUMUX_BIN="$lumux_bin"
  export LUMUX_BIN
}

state="${1:-}"
native_pid="${2:-}"
case "$state" in
  idle|working|blocked|done|clear) ;;
  *) cat >/dev/null 2>&1 || true; exit 0 ;;
esac

[ -n "${LUMUX:-}" ] || { cat >/dev/null 2>&1 || true; exit 0; }
[ -n "${LUMUX_PANE:-}" ] || { cat >/dev/null 2>&1 || true; exit 0; }
resolve_lumux_bin || { cat >/dev/null 2>&1 || true; exit 0; }
command -v python3 >/dev/null 2>&1 || { cat >/dev/null 2>&1 || true; exit 0; }

# Claude runs these global hooks for both the root conversation and nested
# agents. Only the root agent owns the pane's lifecycle; a nested Stop must not
# race the root Stop and overwrite its status. Python gives us a real JSON parse
# instead of grepping arbitrary hook payload text. It also consumes all stdin
# and invokes lumux with the provider session that owns this lifecycle.
python3 -c '
import json
import os
import subprocess
import sys
import time

def process_identity(pid):
    # Linux exposes a boot-relative start tick. The ps fallback keeps the same
    # generation contract on macOS and other Unix platforms.
    try:
        with open("/proc/{}/stat".format(pid), "rb") as stat_file:
            stat = stat_file.read()
        close_paren = stat.rfind(b")")
        fields = stat[close_paren + 2:].split()
        if close_paren >= 0 and len(fields) > 19:
            return "proc:" + fields[19].decode("ascii")
    except Exception:
        pass
    try:
        result = subprocess.run(
            ["ps", "-o", "lstart=", "-p", str(pid)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=2,
            check=False,
        )
        started = result.stdout.decode("utf-8", "replace").strip()
        if result.returncode == 0 and started:
            return "ps:" + started
    except Exception:
        pass
    return None

# Capture order before reading/parsing hook input. A slow older hook must keep
# its original order rather than receive a new timestamp when parsing ends.
sequence = time.time_ns()

try:
    payload = json.load(sys.stdin)
except Exception:
    raise SystemExit(0)

if not isinstance(payload, dict):
    raise SystemExit(0)
# The root conversation owns the pane lifecycle. Nested hook processes can be
# delayed past a root Stop; accepting any of them would let an older subagent
# event receive a newer report timestamp and resurrect stale pane state.
if payload.get("agent_id"):
    raise SystemExit(0)
if payload.get("hook_event_name") == "SubagentStop":
    raise SystemExit(0)

environment = os.environ.copy()
environment.pop("LUMUX_AGENT_OWNER", None)
owner = payload.get("session_id")
if not isinstance(owner, str) or not owner.strip():
    raise SystemExit(0)
pid_text = sys.argv[2]
pid_supplied = bool(pid_text)
try:
    native_pid = int(pid_text)
except Exception:
    native_pid = 0
native_identity = process_identity(native_pid) if native_pid > 0 else None
# Installed hook commands always supply the native pid. Never let a failed
# lookup collapse process-generation ownership back to the bare session id.
if pid_supplied and not native_identity:
    raise SystemExit(0)
if native_identity:
    owner = owner + "@" + native_identity
environment["LUMUX_AGENT_OWNER"] = owner
environment["LUMUX_AGENT_SEQUENCE"] = str(sequence)
environment["LUMUX_AGENT_CLAIM"] = (
    "1"
    if payload.get("hook_event_name") in ("SessionStart", "UserPromptSubmit")
    else "0"
)

try:
    subprocess.run(
        [os.environ["LUMUX_BIN"], "report-state", sys.argv[1], "--agent", "claude"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=2,
        check=False,
    )
except Exception:
    pass
' "$state" "$native_pid" >/dev/null 2>&1 || true

exit 0
"#;

// PowerShell uses the kernel-recorded process creation FILETIME rather than a
// clock read after JSON parsing. This preserves event-start order across hook
// processes and converts to the same Unix-nanosecond scale as Rust/Python.
#[cfg(any(windows, test))]
const CLAUDE_WINDOWS_HOOK_WRAPPER: &str = "@echo off\r\n\
rem installed by lumux\r\n\
rem managed by lumux; reinstalling or updating the integration overwrites this file.\r\n\
rem add custom hooks beside this file instead of editing it.\r\n\
rem LUMUX_INTEGRATION_ID=claude\r\n\
rem LUMUX_INTEGRATION_VERSION=5\r\n\
set \"state=%~1\"\r\n\
set \"native_pid=%~2\"\r\n\
if /I \"%state%\"==\"idle\" goto valid_state\r\n\
if /I \"%state%\"==\"working\" goto valid_state\r\n\
if /I \"%state%\"==\"blocked\" goto valid_state\r\n\
if /I \"%state%\"==\"done\" goto valid_state\r\n\
if /I \"%state%\"==\"clear\" goto valid_state\r\n\
goto drain_and_exit\r\n\
:valid_state\r\n\
if not defined LUMUX goto drain_and_exit\r\n\
if not defined LUMUX_PANE goto drain_and_exit\r\n\
where powershell.exe >nul 2>nul || goto drain_and_exit\r\n\
powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"$sequence = ([System.Diagnostics.Process]::GetCurrentProcess().StartTime.ToUniversalTime().Ticks - 621355968000000000L) * 100L; $inputText = [Console]::In.ReadToEnd(); $lumuxBin=$env:LUMUX_BIN; if ([string]::IsNullOrWhiteSpace($lumuxBin)) { $command=Get-Command lumux -CommandType Application -ErrorAction SilentlyContinue; if ($null -eq $command) { exit 0 }; $lumuxBin=$command.Source } elseif (-not (Test-Path -LiteralPath $lumuxBin -PathType Leaf)) { exit 0 }; try { $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json -ErrorAction Stop } } catch { exit 0 }; if ($null -eq $payload) { exit 0 }; if (-not [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }; if ($payload.hook_event_name -eq 'SubagentStop') { exit 0 }; if ($payload.session_id -isnot [string] -or [string]::IsNullOrWhiteSpace($payload.session_id)) { exit 0 }; $nativePidText='%native_pid%'; $nativePidSupplied=-not [string]::IsNullOrWhiteSpace($nativePidText); $nativeIdentity=$null; if ($nativePidSupplied) { try { $nativePid=[int]$nativePidText; if ($nativePid -le 0) { exit 0 }; $native=Get-Process -Id $nativePid -ErrorAction Stop; $nativeIdentity=$native.StartTime.ToUniversalTime().Ticks } catch { exit 0 } }; Remove-Item -Path Env:LUMUX_AGENT_OWNER -ErrorAction SilentlyContinue; $owner=[string]$payload.session_id; if ($null -ne $nativeIdentity) { $owner=\"${owner}@win:${nativeIdentity}\" }; $env:LUMUX_AGENT_OWNER=$owner; $env:LUMUX_AGENT_SEQUENCE = [string]$sequence; $env:LUMUX_AGENT_CLAIM = if ($payload.hook_event_name -in @('SessionStart', 'UserPromptSubmit')) { '1' } else { '0' }; try { & $lumuxBin report-state '%state%' --agent claude *> $null } catch {}; exit 0\" >nul 2>nul\r\n\
exit /b 0\r\n\
:drain_and_exit\r\n\
more >nul 2>nul\r\n\
exit /b 0\r\n";

#[cfg(windows)]
const CLAUDE_HOOK_WRAPPER: &str = CLAUDE_WINDOWS_HOOK_WRAPPER;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntegrationTarget {
    Claude,
    Codex,
    Copilot,
    Cursor,
}

impl IntegrationTarget {
    fn parse(agent: &str) -> anyhow::Result<Self> {
        match agent.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" | "codex-cli" | "openai-codex" => Ok(Self::Codex),
            "copilot" | "copilot-cli" | "github-copilot" | "github-copilot-cli" => {
                Ok(Self::Copilot)
            }
            "cursor" | "cursor-agent" | "cursor-cli" => Ok(Self::Cursor),
            other => anyhow::bail!(
                "integration for {other:?} is not supported (choose `claude`, `codex`, `copilot`, or `cursor`)"
            ),
        }
    }
}

/// Install one provider adapter behind the shared `report-state` seam. Native
/// hook schemas and lifecycle semantics remain private to each adapter.
pub fn install(agent: &str) -> anyhow::Result<()> {
    let target = IntegrationTarget::parse(agent)?;
    match target {
        IntegrationTarget::Claude => install_claude(claude_settings_path()?),
        IntegrationTarget::Codex => codex::install(),
        IntegrationTarget::Copilot => copilot::install(),
        IntegrationTarget::Cursor => cursor::install(),
    }?;
    for step in activation_steps(target, |key| std::env::var_os(key)) {
        println!("{step}");
    }
    Ok(())
}

/// Provider config is read by the provider process, while the reporter path is
/// inherited when a pane is created. Installation cannot reload either piece
/// of external state, so make the activation contract explicit and honest.
fn activation_steps(
    target: IntegrationTarget,
    getenv: impl Fn(&str) -> Option<OsString>,
) -> Vec<&'static str> {
    let mut steps = Vec::new();
    let reporter_is_current = getenv("LUMUX_BIN")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .is_some_and(|path| reporter_is_usable(&path));
    let inside_lumux = getenv("LUMUX").is_some_and(|value| !value.is_empty());
    if inside_lumux && !reporter_is_current {
        steps.push(
            "Restart the Lumux daemon and recreate this pane; it predates reliable hook delivery.",
        );
    }
    match target {
        IntegrationTarget::Claude => {
            steps.push("Next: Restart Claude Code; running processes do not reload hooks.");
        }
        IntegrationTarget::Codex => {
            steps.push("Next: Restart Codex; running processes do not reload hooks.");
            steps.push(
                "Codex hooks execute zero times until you open `/hooks` and trust every Lumux entry.",
            );
        }
        IntegrationTarget::Copilot => {
            steps.push("Next: Restart GitHub Copilot CLI; running processes do not reload hooks.");
        }
        IntegrationTarget::Cursor => {
            steps.push("Next: Restart the Cursor agent CLI; running processes do not reload hooks.");
        }
    }
    steps
}

fn reporter_is_usable(path: &Path) -> bool {
    if !path.is_absolute() || !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Path to Claude Code's user settings file. `CLAUDE_CONFIG_DIR` relocates the
/// entire Claude configuration tree; otherwise use `~/.claude/settings.json`.
fn claude_settings_path() -> anyhow::Result<PathBuf> {
    claude_settings_path_with(|key| std::env::var_os(key))
}

fn claude_settings_path_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    Ok(common::config_dir_with("CLAUDE_CONFIG_DIR", ".claude", getenv)?.join("settings.json"))
}

/// Merge lumux's report-state hooks into the Claude settings JSON at `path`,
/// preserving every other key and any non-lumux hooks the user already has.
/// Idempotent: re-running replaces only lumux's own hook entries.
fn install_claude(path: PathBuf) -> anyhow::Result<()> {
    use serde_json::{Map, Value};

    let (write_path, mut root) = common::read_json_config(&path)?;
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
    common::prune_managed_nested_hooks(hooks, is_managed_claude_command);

    let wrapper_path = install_claude_wrapper(&path)?;

    for hook_event in CLAUDE_HOOK_EVENTS {
        let command = claude_hook_command(&wrapper_path, hook_event.state);
        let entry = lumux_hook_entry(&command, hook_event.matcher);
        let list = hooks
            .entry(hook_event.event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = list.as_array_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "hooks.{} in {} is not an array",
                hook_event.event,
                path.display()
            )
        })?;
        arr.push(entry);
    }

    common::write_json_config(&path, &write_path, &root)?;
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
    common::write_managed_hook(&wrapper_path, CLAUDE_HOOK_WRAPPER)?;

    Ok(wrapper_path)
}

#[cfg(not(windows))]
fn claude_hook_command(wrapper_path: &Path, state: &str) -> String {
    format!(
        "sh {} {state} \"$PPID\"",
        shell_single_quote(&wrapper_path.display().to_string())
    )
}

#[cfg(not(windows))]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(any(windows, test))]
fn powershell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(any(windows, test))]
fn claude_windows_hook_command(wrapper_path: &Path, state: &str) -> String {
    // Claude starts hook commands through a command shell. Resolve that
    // shell's parent while it is still alive and pass the native Claude pid to
    // the managed wrapper; the wrapper adds its kernel start time so pid reuse
    // cannot let an old SessionEnd clear a replacement process.
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"$hookShellPid=0; $parentPid=0; try {{ $hookShellPid=[int](Get-CimInstance Win32_Process -Filter ('ProcessId = ' + $PID) -ErrorAction Stop).ParentProcessId; $parentPid=[int](Get-CimInstance Win32_Process -Filter ('ProcessId = ' + $hookShellPid) -ErrorAction Stop).ParentProcessId }} catch {{}}; & {} {} $parentPid\"",
        powershell_single_quote(&wrapper_path.display().to_string()),
        powershell_single_quote(state)
    )
}

#[cfg(windows)]
fn claude_hook_command(wrapper_path: &Path, state: &str) -> String {
    claude_windows_hook_command(wrapper_path, state)
}

/// A single Claude hook entry that runs `command`, tagged so it can be
/// recognized and replaced on re-install. Matcher-less events omit the key
/// instead of serializing a wildcard unsupported by their schema.
fn lumux_hook_entry(command: &str, matcher: Option<&str>) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": command,
            // Herdr uses the same budget. Windows must cold-start PowerShell to
            // parse Claude's JSON before launching lumux; one second is too
            // brittle on machines with antivirus/process-startup overhead.
            "timeout": 10,
            "lumux_managed": true,
        }],
    });
    if let Some(matcher) = matcher {
        entry
            .as_object_mut()
            .expect("hook entry is constructed as an object")
            .insert("matcher".to_string(), matcher.into());
    }
    entry
}

fn is_managed_claude_command(hook: &serde_json::Value) -> bool {
    hook.get("lumux_managed").and_then(|m| m.as_bool()) == Some(true)
}

/// Whether a hook-list entry is one lumux installed (so re-install replaces it
/// instead of duplicating). Ownership is explicit: command text alone is never
/// enough to distinguish a user-owned hook from one installed by lumux.
#[cfg(test)]
fn is_lumux_hook(v: &serde_json::Value) -> bool {
    v.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| arr.iter().any(is_managed_claude_command))
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
    fn claude_config_dir_overrides_home_for_hook_installation() {
        let path = claude_settings_path_with(env_of(&[
            ("CLAUDE_CONFIG_DIR", "custom-claude"),
            ("HOME", "ignored-home"),
        ]))
        .unwrap();
        assert_eq!(path, PathBuf::from("custom-claude/settings.json"));
    }

    #[test]
    fn empty_claude_config_dir_falls_back_to_home() {
        let path = claude_settings_path_with(env_of(&[
            ("CLAUDE_CONFIG_DIR", ""),
            ("HOME", "fallback-home"),
        ]))
        .unwrap();
        assert_eq!(path, PathBuf::from("fallback-home/.claude/settings.json"));
    }

    #[test]
    fn report_command_reads_pane_agent_owner_and_sequence_from_env() {
        let cmd = build_report_command_at(
            "working",
            None,
            env_of(&[
                ("LUMUX_PANE", "%42"),
                ("LUMUX_AGENT", "claude"),
                ("LUMUX_AGENT_OWNER", "claude-session"),
                ("LUMUX_AGENT_SEQUENCE", "29"),
                ("LUMUX_AGENT_CLAIM", "1"),
            ]),
            17,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            cmd,
            Cmd::ReportAgentState {
                pane: PaneId(42),
                report: AgentReport::new(
                    AgentIdentity::new("claude", Some("claude-session".into())),
                    true,
                    AgentState::Working,
                    29,
                ),
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
            Cmd::ReportAgentState { report, .. } => {
                assert_eq!(report.identity.agent, "codex");
                assert_eq!(report.state, AgentState::Blocked);
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
            Cmd::ReportAgentState { report, .. } => {
                assert_eq!(report.identity.agent, "agent");
                assert_eq!(report.identity.owner, None);
                assert!(
                    report.claim,
                    "unowned direct reports must be able to replace state"
                );
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn owned_report_requires_an_explicit_claim_signal() {
        let cmd = build_report_command_at(
            "idle",
            Some("claude"),
            env_of(&[("LUMUX_PANE", "%3"), ("LUMUX_AGENT_OWNER", "session-1")]),
            17,
        )
        .unwrap()
        .unwrap();
        match cmd {
            Cmd::ReportAgentState { report, .. } => {
                assert_eq!(report.identity.owner.as_deref(), Some("session-1"));
                assert!(!report.claim);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn missing_pane_is_a_silent_noop() {
        assert_eq!(
            build_report_command(
                "idle",
                None,
                env_of(&[("LUMUX_AGENT_SEQUENCE", "not-a-number")])
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn malformed_hook_sequence_is_rejected_inside_a_pane() {
        let error = build_report_command_at(
            "idle",
            None,
            env_of(&[
                ("LUMUX_PANE", "%3"),
                ("LUMUX_AGENT_SEQUENCE", "not-a-number"),
            ]),
            17,
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid $LUMUX_AGENT_SEQUENCE"));
    }

    #[test]
    fn malformed_hook_claim_is_rejected_for_reports() {
        let error = build_report_command_at(
            "idle",
            None,
            env_of(&[("LUMUX_PANE", "%3"), ("LUMUX_AGENT_CLAIM", "sometimes")]),
            17,
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid $LUMUX_AGENT_CLAIM"));
    }

    #[test]
    fn bad_state_is_an_error() {
        let err =
            build_report_command("frobbing", None, env_of(&[("LUMUX_PANE", "%1")])).unwrap_err();
        assert!(err.to_string().contains("idle, working, blocked"));
    }

    #[test]
    fn clear_builds_a_clear_command() {
        let cmd = build_report_command_at(
            "clear",
            Some("claude"),
            env_of(&[
                ("LUMUX_PANE", "%9"),
                ("LUMUX_AGENT_OWNER", "old-session"),
                ("LUMUX_AGENT_SEQUENCE", "31"),
                ("LUMUX_AGENT_CLAIM", "not-used-for-clear"),
            ]),
            23,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            cmd,
            Cmd::ClearAgentState {
                pane: PaneId(9),
                clear: AgentClear::new(
                    AgentIdentity::new("claude", Some("old-session".into())),
                    31,
                ),
            }
        );
    }

    #[test]
    fn malformed_pane_id_is_rejected_before_reporting() {
        let err =
            build_report_command("idle", None, env_of(&[("LUMUX_PANE", "pane-9")])).unwrap_err();
        assert!(err.to_string().contains("invalid $LUMUX_PANE"));
    }

    #[test]
    fn integration_target_accepts_provider_cli_aliases() {
        for alias in ["claude", "claude-code"] {
            assert_eq!(
                IntegrationTarget::parse(alias).unwrap(),
                IntegrationTarget::Claude
            );
        }
        for alias in ["codex", "codex-cli", "openai-codex"] {
            assert_eq!(
                IntegrationTarget::parse(alias).unwrap(),
                IntegrationTarget::Codex
            );
        }
        for alias in ["cursor", "cursor-agent", "cursor-cli"] {
            assert_eq!(
                IntegrationTarget::parse(alias).unwrap(),
                IntegrationTarget::Cursor
            );
        }
        for alias in [
            "copilot",
            "copilot-cli",
            "github-copilot",
            "github-copilot-cli",
        ] {
            assert_eq!(
                IntegrationTarget::parse(alias).unwrap(),
                IntegrationTarget::Copilot
            );
        }
    }

    #[test]
    fn activation_steps_cover_provider_reload_trust_and_old_panes() {
        let codex = activation_steps(IntegrationTarget::Codex, env_of(&[]));
        assert!(codex.iter().any(|step| step.contains("Restart Codex")));
        assert!(codex
            .iter()
            .any(|step| step.contains("hooks execute zero times until")));

        let copilot = activation_steps(IntegrationTarget::Copilot, env_of(&[]));
        assert!(copilot
            .iter()
            .any(|step| step.contains("Restart GitHub Copilot CLI")));
        assert!(!copilot.iter().any(|step| step.contains("/hooks")));

        let old_pane = activation_steps(
            IntegrationTarget::Copilot,
            env_of(&[("LUMUX", "/run/lumux.sock")]),
        );
        assert!(old_pane
            .iter()
            .any(|step| step.contains("Restart the Lumux daemon")));

        let empty_sentinel = activation_steps(
            IntegrationTarget::Copilot,
            env_of(&[("LUMUX", "")]),
        );
        assert!(!empty_sentinel
            .iter()
            .any(|step| step.contains("Restart the Lumux daemon")));

        let reporter = std::env::current_exe().unwrap();
        let reporter = reporter.to_str().unwrap();
        let current_pane = activation_steps(
            IntegrationTarget::Copilot,
            env_of(&[("LUMUX", "/run/lumux.sock"), ("LUMUX_BIN", reporter)]),
        );
        assert!(!current_pane
            .iter()
            .any(|step| step.contains("Restart the Lumux daemon")));

        for invalid in ["", "relative/lumux", "/missing/lumux"] {
            let stale = activation_steps(
                IntegrationTarget::Copilot,
                env_of(&[("LUMUX", "/run/lumux.sock"), ("LUMUX_BIN", invalid)]),
            );
            assert!(stale
                .iter()
                .any(|step| step.contains("Restart the Lumux daemon")));
        }

        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let non_executable = dir.path().join("lumux");
            std::fs::write(&non_executable, b"not executable").unwrap();
            let non_executable = non_executable.to_str().unwrap();
            let stale = activation_steps(
                IntegrationTarget::Copilot,
                env_of(&[
                    ("LUMUX", "/run/lumux.sock"),
                    ("LUMUX_BIN", non_executable),
                ]),
            );
            assert!(stale
                .iter()
                .any(|step| step.contains("Restart the Lumux daemon")));
        }
    }

    #[test]
    fn claude_hook_mapping_covers_lifecycle_and_blocking_events() {
        let map: std::collections::HashMap<_, _> = CLAUDE_HOOK_EVENTS
            .iter()
            .map(|hook_event| (hook_event.event, hook_event))
            .collect();
        assert_eq!(
            map.len(),
            CLAUDE_HOOK_EVENTS.len(),
            "Claude hook event descriptors must be unique"
        );

        let mapping = |event| {
            map.get(event)
                .map(|hook_event| (hook_event.state, hook_event.matcher))
        };
        assert_eq!(mapping("SessionStart"), Some(("idle", Some("*"))));
        assert_eq!(mapping("UserPromptSubmit"), Some(("working", None)));
        assert_eq!(mapping("PreToolUse"), Some(("working", Some("*"))));
        assert_eq!(mapping("PermissionRequest"), Some(("blocked", Some("*"))));
        assert_eq!(
            mapping("Notification"),
            Some((
                "blocked",
                Some("permission_prompt|idle_prompt|elicitation_dialog|agent_needs_input")
            ))
        );
        assert_eq!(mapping("Stop"), Some(("idle", None)));
        assert_eq!(mapping("StopFailure"), Some(("idle", Some("*"))));
        assert_eq!(mapping("SessionEnd"), Some(("clear", Some("*"))));
    }

    #[test]
    fn hook_entry_serializes_only_supported_matchers() {
        let restricted = lumux_hook_entry("report blocked", Some("one|two"));
        assert_eq!(restricted["matcher"], "one|two");

        let matcherless = lumux_hook_entry("report idle", None);
        assert!(
            matcherless.get("matcher").is_none(),
            "matcher-less Claude events must omit the key rather than use null or `*`"
        );
    }

    #[test]
    fn windows_wrappers_capture_unix_nanoseconds_before_reading_input() {
        const UNIX_NANOS: &str = concat!(
            "([System.Diagnostics.Process]::GetCurrentProcess()",
            ".StartTime.ToUniversalTime().Ticks - 621355968000000000L) * 100L"
        );
        for (provider, wrapper) in [
            ("claude", CLAUDE_WINDOWS_HOOK_WRAPPER),
            (
                "codex",
                include_str!("integration/assets/codex/lumux-agent-state.ps1"),
            ),
            (
                "copilot",
                include_str!("integration/assets/copilot/lumux-agent-state.ps1"),
            ),
        ] {
            let sequence = wrapper
                .find(UNIX_NANOS)
                .unwrap_or_else(|| panic!("{provider} must use Unix-epoch nanoseconds"));
            let input = wrapper
                .find("[Console]::In.ReadToEnd()")
                .unwrap_or_else(|| panic!("{provider} must consume hook input"));
            assert!(
                sequence < input,
                "{provider} must timestamp the event before input can delay it"
            );
        }
    }

    #[test]
    fn claude_windows_hooks_bind_reports_to_the_native_process_generation() {
        let command = claude_windows_hook_command(
            Path::new(r"C:\Users\O'Brien\$profile`cache\hooks\state.cmd"),
            "clear",
        );
        assert!(command.contains("Get-CimInstance Win32_Process"));
        assert!(command.contains("ParentProcessId"));
        assert!(command
            .contains(r"& 'C:\Users\O''Brien\$profile`cache\hooks\state.cmd' 'clear' $parentPid"));

        for expected in [
            "set \"native_pid=%~2\"",
            "$nativePidSupplied=-not [string]::IsNullOrWhiteSpace($nativePidText)",
            "Get-Process -Id $nativePid -ErrorAction Stop",
            "$native.StartTime.ToUniversalTime().Ticks",
            "${owner}@win:${nativeIdentity}",
            "if ($nativePid -le 0) { exit 0 }",
            "$lumuxBin=$env:LUMUX_BIN",
            "& $lumuxBin report-state '%state%' --agent claude",
        ] {
            assert!(
                CLAUDE_WINDOWS_HOOK_WRAPPER.contains(expected),
                "Windows Claude wrapper is missing process-generation guard: {expected}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_wrappers_capture_unix_nanoseconds_before_reading_input() {
        for (provider, wrapper) in [
            ("claude", CLAUDE_HOOK_WRAPPER),
            (
                "codex",
                include_str!("integration/assets/codex/lumux-agent-state.sh"),
            ),
            (
                "copilot",
                include_str!("integration/assets/copilot/lumux-agent-state.sh"),
            ),
        ] {
            let sequence = wrapper
                .find("sequence = time.time_ns()")
                .unwrap_or_else(|| panic!("{provider} must use Unix-epoch nanoseconds"));
            let input = wrapper
                .find("json.load(sys.stdin)")
                .unwrap_or_else(|| panic!("{provider} must parse hook input as JSON"));
            assert!(
                sequence < input,
                "{provider} must timestamp the event before input can delay it"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn hook_command_shell_quotes_wrapper_path() {
        let path = Path::new("/tmp/claude's hooks/lumux-agent-state.sh");
        assert_eq!(
            claude_hook_command(path, "idle"),
            "sh '/tmp/claude'\"'\"'s hooks/lumux-agent-state.sh' idle \"$PPID\""
        );
    }

    #[cfg(not(windows))]
    fn run_claude_wrapper(
        wrapper: &Path,
        fake_bin: &Path,
        log: &Path,
        state: &str,
        payload: &str,
    ) -> std::process::Output {
        run_claude_wrapper_with_pid(wrapper, fake_bin, log, state, payload, None)
    }

    #[cfg(not(windows))]
    fn run_claude_wrapper_with_pid(
        wrapper: &Path,
        fake_bin: &Path,
        log: &Path,
        state: &str,
        payload: &str,
        native_pid: Option<&str>,
    ) -> std::process::Output {
        let mut command = std::process::Command::new("sh");
        command.arg(wrapper).arg(state);
        if let Some(native_pid) = native_pid {
            command.arg(native_pid);
        }
        let mut child = command
            .env("LUMUX", "test-daemon")
            .env("LUMUX_PANE", "%1")
            .env("LUMUX_AGENT_OWNER", "inherited-owner")
            .env("LUMUX_AGENT_SEQUENCE", "7")
            .env("LUMUX_AGENT_CLAIM", "inherited-claim")
            .env("LUMUX_TEST_LOG", log)
            .env("LUMUX_BIN", fake_bin.join("lumux"))
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        std::io::Write::write_all(&mut stdin, payload.as_bytes()).unwrap();
        drop(stdin);
        child.wait_with_output().unwrap()
    }

    #[cfg(not(windows))]
    #[test]
    fn claude_wrapper_forwards_root_events_and_ignores_subagents() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir();
        let settings_path = dir.join("settings.json");
        install_claude(settings_path).unwrap();
        let wrapper = dir.join("hooks").join("lumux-agent-state.sh");
        let fake_bin = dir.join("bin");
        let fake_lumux = fake_bin.join("lumux");
        let log = dir.join("lumux-invocation");
        std::fs::create_dir_all(&fake_bin).unwrap();
        std::fs::write(
            &fake_lumux,
            "#!/bin/sh\nprintf '%s|owner=%s|sequence=%s|claim=%s\\n' \"$*\" \"${LUMUX_AGENT_OWNER-}\" \"${LUMUX_AGENT_SEQUENCE-}\" \"${LUMUX_AGENT_CLAIM-}\" >\"$LUMUX_TEST_LOG\"\nprintf noisy-stdout\nprintf noisy-stderr >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_lumux).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_lumux, permissions).unwrap();

        let root = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "idle",
            r#"{"hook_event_name":"SessionStart","session_id":"root-session"}"#,
        );
        assert!(root.status.success());
        assert!(root.stdout.is_empty());
        assert!(root.stderr.is_empty());
        let invocation = std::fs::read_to_string(&log).unwrap();
        let (command, metadata) = invocation.trim().split_once("|owner=").unwrap();
        let (owner, ordering) = metadata.split_once("|sequence=").unwrap();
        let (sequence, claim) = ordering.split_once("|claim=").unwrap();
        assert_eq!(command, "report-state idle --agent claude");
        assert_eq!(owner, "root-session");
        assert!(
            sequence
                .parse::<u64>()
                .is_ok_and(|sequence| sequence > 0 && sequence != 7),
            "wrapper must capture a numeric event-start sequence: {sequence:?}"
        );
        assert_eq!(claim, "1", "SessionStart must claim lifecycle ownership");

        let ordinary = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "idle",
            r#"{"hook_event_name":"Stop","session_id":"root-session"}"#,
        );
        assert!(ordinary.status.success());
        let invocation = std::fs::read_to_string(&log).unwrap();
        assert!(
            invocation.trim().ends_with("|claim=0"),
            "ordinary lifecycle reports must not take over another owner: {invocation:?}"
        );

        let foreground = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "working",
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"root-session"}"#,
        );
        assert!(foreground.status.success());
        let invocation = std::fs::read_to_string(&log).unwrap();
        assert!(
            invocation.trim().ends_with("|claim=1"),
            "UserPromptSubmit must claim lifecycle ownership: {invocation:?}"
        );

        std::fs::remove_file(&log).unwrap();
        let ownerless = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "idle",
            r#"{"hook_event_name":"SessionStart"}"#,
        );
        assert!(ownerless.status.success());
        assert!(
            !log.exists(),
            "a provider report without a session id must not replace owned state"
        );

        let nested = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "idle",
            r#"{"hook_event_name":"Stop","agent_id":"subagent-1"}"#,
        );
        assert!(nested.status.success());
        assert!(nested.stdout.is_empty());
        assert!(nested.stderr.is_empty());
        assert!(!log.exists(), "a nested agent must not update pane state");

        let nested_working = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "working",
            r#"{"hook_event_name":"PreToolUse","agent_id":"subagent-1"}"#,
        );
        assert!(nested_working.status.success());
        assert!(
            !log.exists(),
            "a nested active report must not race the root pane lifecycle"
        );

        let nested_blocked = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "blocked",
            r#"{"hook_event_name":"PermissionRequest","agent_id":"subagent-1"}"#,
        );
        assert!(nested_blocked.status.success());
        assert!(
            !log.exists(),
            "a nested blocked report must not take ownership from the root agent"
        );

        let nested_stop = run_claude_wrapper(
            &wrapper,
            &fake_bin,
            &log,
            "idle",
            r#"{"hook_event_name":"SubagentStop"}"#,
        );
        assert!(nested_stop.status.success());
        assert!(nested_stop.stdout.is_empty());
        assert!(nested_stop.stderr.is_empty());
        assert!(
            !log.exists(),
            "SubagentStop must not overwrite the root agent's state"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn claude_wrapper_owners_include_native_process_generation_and_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir();
        let settings_path = dir.join("settings.json");
        install_claude(settings_path).unwrap();
        let wrapper = dir.join("hooks").join("lumux-agent-state.sh");
        let fake_bin = dir.join("bin");
        let fake_lumux = fake_bin.join("lumux");
        let log = dir.join("lumux-invocation");
        std::fs::create_dir_all(&fake_bin).unwrap();
        std::fs::write(
            &fake_lumux,
            "#!/bin/sh\nprintf '%s|owner=%s\\n' \"$*\" \"${LUMUX_AGENT_OWNER-}\" >\"$LUMUX_TEST_LOG\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_lumux).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_lumux, permissions).unwrap();

        let first_pid = std::process::id().to_string();
        let first = run_claude_wrapper_with_pid(
            &wrapper,
            &fake_bin,
            &log,
            "idle",
            r#"{"hook_event_name":"SessionStart","session_id":"reused-session"}"#,
            Some(&first_pid),
        );
        assert!(first.status.success());
        let first_owner = std::fs::read_to_string(&log)
            .unwrap()
            .trim()
            .split_once("|owner=")
            .unwrap()
            .1
            .to_string();
        assert!(
            first_owner.starts_with("reused-session@"),
            "owner must include the native process generation: {first_owner:?}"
        );

        let mut replacement = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let replacement_pid = replacement.id().to_string();
        let second = run_claude_wrapper_with_pid(
            &wrapper,
            &fake_bin,
            &log,
            "idle",
            r#"{"hook_event_name":"SessionStart","session_id":"reused-session"}"#,
            Some(&replacement_pid),
        );
        let _ = replacement.kill();
        let _ = replacement.wait();
        assert!(second.status.success());
        let second_owner = std::fs::read_to_string(&log)
            .unwrap()
            .trim()
            .split_once("|owner=")
            .unwrap()
            .1
            .to_string();
        assert_ne!(
            first_owner, second_owner,
            "the same Claude session id in a replacement process needs a new owner"
        );

        std::fs::remove_file(&log).unwrap();
        let invalid = run_claude_wrapper_with_pid(
            &wrapper,
            &fake_bin,
            &log,
            "clear",
            r#"{"hook_event_name":"SessionEnd","session_id":"reused-session"}"#,
            Some("0"),
        );
        assert!(invalid.status.success());
        assert!(
            !log.exists(),
            "an explicit but unresolvable native pid must not downgrade to a bare owner"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
                            {"type": "command", "command": "lumux report-state idle --agent claude", "lumux_managed": true},
                            {"type": "command", "command": "lumux report-state blocked --agent custom"}
                        ]
                    }],
                    "PostToolUse": [{
                        "matcher": "tool",
                        "hooks": [
                            {"type": "command", "command": "lumux report-state working --agent claude", "lumux_managed": true},
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
        assert!(
            stop.iter().any(|group| {
                group["hooks"].as_array().is_some_and(|hooks| {
                    hooks
                        .iter()
                        .any(|hook| hook["command"] == "lumux report-state blocked --agent custom")
                })
            }),
            "an untagged user hook must never be claimed by lumux"
        );
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
        for hook_event in CLAUDE_HOOK_EVENTS {
            let arr = v["hooks"][hook_event.event].as_array().unwrap();
            assert_eq!(
                arr.iter().filter(|hook| is_lumux_hook(hook)).count(),
                1,
                "event {} must have exactly one lumux hook",
                hook_event.event
            );
            let ours = arr.iter().find(|h| is_lumux_hook(h)).unwrap();
            let command = ours["hooks"][0]["command"].as_str().unwrap();
            assert!(command.contains("lumux-agent-state"));
            #[cfg(not(windows))]
            assert!(command.ends_with(&format!("{} \"$PPID\"", hook_event.state)));
            #[cfg(windows)]
            assert!(command.contains(&format!("'{}' $parentPid", hook_event.state)));
            assert_eq!(ours["hooks"][0]["timeout"], 10);
            match hook_event.matcher {
                Some(matcher) => assert_eq!(ours["matcher"], matcher),
                None => assert!(
                    ours.get("matcher").is_none(),
                    "event {} must omit unsupported matcher key",
                    hook_event.event
                ),
            }
        }

        let wrapper = std::fs::read_to_string(&wrapper_path).unwrap();
        assert!(wrapper.contains("LUMUX_INTEGRATION_ID=claude"));
        assert!(wrapper.contains("LUMUX_PANE"));
        assert!(wrapper.contains("agent_id"));
        assert!(wrapper.contains("SubagentStop"));
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;

            assert!(wrapper.contains("${LUMUX:-}"));
            assert!(wrapper.contains("json.load(sys.stdin)"));
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
            assert!(wrapper.contains("[Console]::In.ReadToEnd()"));
            assert!(wrapper.contains("ConvertFrom-Json"));
            assert!(wrapper.contains(">nul 2>nul"));
        }

        // Re-run: idempotent — exactly one lumux hook per event, no duplicates.
        install_claude(path.clone()).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for hook_event in CLAUDE_HOOK_EVENTS {
            let arr = v2["hooks"][hook_event.event].as_array().unwrap();
            assert_eq!(
                arr.iter().filter(|h| is_lumux_hook(h)).count(),
                1,
                "event {} must have exactly one lumux hook after re-install",
                hook_event.event
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

    #[cfg(unix)]
    #[test]
    fn install_claude_preserves_symlinked_settings_path() {
        use std::os::unix::fs::symlink;

        let dir = unique_temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        let actual_dir = dir.join("actual");
        let config_dir = actual_dir.join("claude");
        let links_dir = actual_dir.join("links");
        let shared_dir = actual_dir.join("shared");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&links_dir).unwrap();
        std::fs::create_dir_all(&shared_dir).unwrap();

        // Reach settings through a symlinked config directory as well. The
        // relative targets below must be interpreted after that parent link is
        // resolved, rather than collapsed lexically against `dir`.
        let config_alias = dir.join("claude-config");
        symlink(Path::new("actual/claude"), &config_alias).unwrap();

        let target = shared_dir.join("settings.json");
        std::fs::write(&target, r#"{"theme":"dark"}"#).unwrap();
        let intermediate = links_dir.join("claude-settings.json");
        let intermediate_target = Path::new("../shared/settings.json");
        symlink(intermediate_target, &intermediate).unwrap();
        let settings_path = config_alias.join("settings.json");
        let relative_target = Path::new("../links/claude-settings.json");
        symlink(relative_target, &settings_path).unwrap();

        install_claude(settings_path.clone()).unwrap();
        assert!(
            std::fs::symlink_metadata(&settings_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "installing hooks must not replace the user's settings symlink"
        );
        assert_eq!(std::fs::read_link(&settings_path).unwrap(), relative_target);
        assert_eq!(
            std::fs::read_link(&config_alias).unwrap(),
            Path::new("actual/claude")
        );
        assert_eq!(
            std::fs::read_link(&intermediate).unwrap(),
            intermediate_target
        );
        let first_install = std::fs::read(&target).unwrap();
        let settings: serde_json::Value = serde_json::from_slice(&first_install).unwrap();
        assert_eq!(settings["theme"], "dark");
        for hook_event in CLAUDE_HOOK_EVENTS {
            let groups = settings["hooks"][hook_event.event].as_array().unwrap();
            assert_eq!(
                groups.iter().filter(|group| is_lumux_hook(group)).count(),
                1
            );
        }

        install_claude(settings_path.clone()).unwrap();
        assert!(std::fs::symlink_metadata(&settings_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&settings_path).unwrap(), relative_target);
        assert_eq!(
            std::fs::read_link(&config_alias).unwrap(),
            Path::new("actual/claude")
        );
        assert_eq!(
            std::fs::read_link(&intermediate).unwrap(),
            intermediate_target
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            first_install,
            "re-installing through a symlink must be idempotent"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn install_claude_rejects_settings_symlink_cycles_without_mutation() {
        use std::os::unix::fs::symlink;

        let dir = unique_temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings_path = dir.join("settings.json");
        let other_path = dir.join("other-settings.json");
        symlink("other-settings.json", &settings_path).unwrap();
        symlink("settings.json", &other_path).unwrap();

        let error = install_claude(settings_path.clone()).unwrap_err();
        assert!(
            error.to_string().contains("symlink cycle"),
            "the unsafe settings path should be diagnosed clearly: {error:#}"
        );
        assert_eq!(
            std::fs::read_link(&settings_path).unwrap(),
            Path::new("other-settings.json")
        );
        assert_eq!(
            std::fs::read_link(&other_path).unwrap(),
            Path::new("settings.json")
        );
        assert!(
            !dir.join("hooks").exists(),
            "a rejected settings path must not partially install the wrapper"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_claude_preserves_unreadable_settings_bytes() {
        let dir = unique_temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = vec![0xff, 0xfe, 0xfd];
        std::fs::write(&path, &original).unwrap();

        let error = install_claude(path.clone()).unwrap_err();
        assert!(
            error.to_string().contains("read") || error.to_string().contains("UTF-8"),
            "the read failure should be surfaced: {error:#}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "a settings file that could not be read must remain byte-for-byte intact"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_agent_errors() {
        let err = install("unsupported-agent").unwrap_err();
        assert!(err.to_string().contains("is not supported"));
    }
}
