#!/bin/sh
# installed by lumux
# managed by lumux; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# LUMUX_INTEGRATION_ID=codex
# LUMUX_INTEGRATION_VERSION=4

# Internal detached watcher mode. SessionStart launches this process with the
# native Codex pid and its creation identity in the environment. It owns no
# terminal handles and clears only the same provider/session after that exact
# process disappears (a reused pid has a different identity).
if [ "${1:-}" = "--watch" ]; then
  if [ -z "${LUMUX:-}" ] || [ -z "${LUMUX_PANE:-}" ] || \
     [ -z "${LUMUX_AGENT_OWNER:-}" ] || [ -z "${LUMUX_CODEX_WATCH_PID:-}" ] || \
     [ -z "${LUMUX_CODEX_WATCH_IDENTITY:-}" ] || \
     ! command -v python3 >/dev/null 2>&1 || ! command -v lumux >/dev/null 2>&1; then
    exit 0
  fi

  python3 -c '
import os
import subprocess
import time

def process_identity(pid):
    # Linux exposes a boot-relative start tick, which is stable and immune to
    # wall-clock changes. The fallback covers macOS and other Unix platforms.
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

try:
    pid = int(os.environ["LUMUX_CODEX_WATCH_PID"])
except Exception:
    raise SystemExit(0)
expected = os.environ.get("LUMUX_CODEX_WATCH_IDENTITY")
while process_identity(pid) == expected:
    time.sleep(0.25)

environment = os.environ.copy()
environment["LUMUX_AGENT_SEQUENCE"] = str(time.time_ns())
environment["LUMUX_AGENT_CLAIM"] = "0"
environment.pop("LUMUX_CODEX_WATCH_PID", None)
environment.pop("LUMUX_CODEX_WATCH_IDENTITY", None)
try:
    subprocess.run(
        ["lumux", "report-state", "clear", "--agent", "codex"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=2,
        check=False,
    )
except Exception:
    pass
' </dev/null >/dev/null 2>&1 || true
  exit 0
fi

state="${1:-}"
native_pid="${2:-}"
case "$state" in
  idle|working|blocked) ;;
  *) cat >/dev/null 2>&1 || true; exit 0 ;;
esac

# Codex supplies one JSON object on stdin. State mapping is owned by hooks.json;
# this adapter extracts only lifecycle ownership and event-start order.
if [ -z "${LUMUX:-}" ] || [ -z "${LUMUX_PANE:-}" ] || \
   ! command -v python3 >/dev/null 2>&1 || ! command -v lumux >/dev/null 2>&1; then
  cat >/dev/null 2>&1 || true
  exit 0
fi

python3 -c '
import json
import os
import subprocess
import sys
import time

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

# Timestamp before input parsing so delayed older events retain their order.
sequence = time.time_ns()
try:
    payload = json.load(sys.stdin)
except Exception:
    raise SystemExit(0)
if not isinstance(payload, dict):
    raise SystemExit(0)

environment = os.environ.copy()
environment.pop("LUMUX_AGENT_OWNER", None)
owner = payload.get("session_id")
if not isinstance(owner, str) or not owner.strip():
    raise SystemExit(0)
pid_text = sys.argv[3]
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
environment["LUMUX_AGENT_CLAIM"] = (
    "1"
    if payload.get("hook_event_name") in ("SessionStart", "UserPromptSubmit")
    else "0"
)

try:
    subprocess.run(
        ["lumux", "report-state", sys.argv[1], "--agent", "codex"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=2,
        check=False,
    )
except Exception:
    pass

# Codex has no documented SessionEnd hook. The outer hook command passes its own
# PPID, which is the long-lived native Codex process (not the Node launcher).
# Detach a watcher only after the SessionStart report so the hook itself returns
# promptly.
if payload.get("hook_event_name") == "SessionStart":
    if native_identity:
        watch_environment = environment.copy()
        watch_environment["LUMUX_AGENT_CLAIM"] = "0"
        watch_environment["LUMUX_CODEX_WATCH_PID"] = str(native_pid)
        watch_environment["LUMUX_CODEX_WATCH_IDENTITY"] = native_identity
        try:
            subprocess.Popen(
                ["sh", sys.argv[2], "--watch"],
                env=watch_environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
                close_fds=True,
            )
        except Exception:
            pass
' "$state" "$0" "$native_pid" >/dev/null 2>&1 || true
exit 0
