#!/bin/sh
# installed by lumux
# managed by lumux; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# LUMUX_INTEGRATION_ID=copilot
# LUMUX_INTEGRATION_VERSION=7

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

action="${1:-}"
native_pid="${2:-}"
case "$action" in
  session-start|working|blocked|pre-tool|post-tool|error|stop|notification|session-end|idle|clear) ;;
  *) cat >/dev/null 2>&1 || true; exit 0 ;;
esac

# Copilot command PreToolUse hooks fail closed on hook errors. Every branch in
# this telemetry-only wrapper therefore consumes stdin, suppresses output, and
# returns zero—even with malformed JSON, missing tools, or no lumux pane.
if [ -z "${LUMUX:-}" ] || [ -z "${LUMUX_PANE:-}" ] || \
   ! command -v python3 >/dev/null 2>&1 || ! resolve_lumux_bin; then
  cat >/dev/null 2>&1 || true
  exit 0
fi

LUMUX_COPILOT_ACTION="$action" python3 -c '
import json
import os
import subprocess
import sys
import time

# Timestamp before input parsing so delayed older events retain their order.
sequence = time.time_ns()

try:
    payload = json.load(sys.stdin)
except Exception:
    raise SystemExit(0)
if not isinstance(payload, dict):
    raise SystemExit(0)

action = os.environ.get("LUMUX_COPILOT_ACTION", "")

def process_identity(pid):
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

def first_text(*keys):
    for key in keys:
        value = payload.get(key)
        if isinstance(value, str) and value:
            return value
    return None

state = None
if action == "session-start":
    initial = first_text("initial_prompt", "initialPrompt")
    state = "working" if initial and initial.strip() else "idle"
elif action == "working":
    state = "working"
elif action == "blocked":
    state = "blocked"
elif action == "pre-tool":
    tool = first_text("tool_name", "toolName")
    state = "blocked" if tool in ("ask_user", "exit_plan_mode", "AskUserQuestion", "ExitPlanMode") else "working"
elif action == "post-tool":
    # report_intent can finish after ask_user has already blocked the turn but
    # before the user answers. It must not dismiss that blocker.
    if first_text("tool_name", "toolName") != "report_intent":
        state = "working"
elif action == "notification":
    notification = first_text("notification_type", "notificationType")
    if notification in ("permission_prompt", "elicitation_dialog"):
        state = "blocked"
elif action == "error":
    # Recoverable errors may be handled while the main agent keeps working.
    # A non-recoverable error leaves the interactive session open but needing
    # attention; Stop/SessionEnd will later settle or clear the lifecycle.
    if payload.get("recoverable") is False:
        state = "blocked"
elif action in ("stop", "idle"):
    # Stop is the main-agent turn boundary. Even if a provider version adds a
    # new reason, leaving working/blocked sticky after the turn is worse than
    # settling the pane back to idle.
    state = "idle"
elif action in ("session-end", "clear"):
    # SessionEnd is emitted only when the session terminates. Its documented
    # reasons (including complete/error/timeout) all end this lifecycle.
    state = "clear"

if state is None:
    raise SystemExit(0)
environment = os.environ.copy()
environment.pop("LUMUX_AGENT_OWNER", None)
owner = first_text("sessionId", "session_id")
if not owner or not owner.strip():
    raise SystemExit(0)
pid_text = sys.argv[1]
pid_supplied = bool(pid_text)
try:
    native_pid = int(pid_text)
except Exception:
    native_pid = 0
native_identity = process_identity(native_pid) if native_pid > 0 else None
# Installed commands always supply the native pid. Never downgrade a generated
# owner to its bare session id when that process generation cannot be proven.
if pid_supplied and not native_identity:
    raise SystemExit(0)
if native_identity:
    owner = owner + "@" + native_identity
environment["LUMUX_AGENT_OWNER"] = owner
environment["LUMUX_AGENT_SEQUENCE"] = str(sequence)
environment["LUMUX_AGENT_CLAIM"] = "1" if action in ("session-start", "working") else "0"
try:
    subprocess.run(
        [os.environ["LUMUX_BIN"], "report-state", state, "--agent", "copilot"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=2,
        check=False,
    )
except Exception:
    pass
' "$native_pid" >/dev/null 2>&1 || true
exit 0
