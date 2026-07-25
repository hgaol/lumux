#!/bin/sh
# installed by lumux
# managed by lumux; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# LUMUX_INTEGRATION_ID=kimi
# LUMUX_INTEGRATION_VERSION=1

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
case "$state" in
  idle|working|blocked|clear) ;;
  *) cat >/dev/null 2>&1 || true; exit 0 ;;
esac

# Kimi supplies one JSON object on stdin (Claude-style hook payload).
# State mapping is owned by settings.json; this adapter extracts only lifecycle
# ownership and event-start order.
if [ -z "${LUMUX:-}" ] || [ -z "${LUMUX_PANE:-}" ] || \
   ! command -v python3 >/dev/null 2>&1 || ! resolve_lumux_bin; then
  cat >/dev/null 2>&1 || true
  exit 0
fi

python3 -c '
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

# Background subagents must never repaint the foreground pane.
if payload.get("agent_id"):
    raise SystemExit(0)

owner = payload.get("session_id")
if not isinstance(owner, str) or not owner.strip():
    raise SystemExit(0)

state = sys.argv[1]
event = payload.get("hook_event_name") or ""

environment = os.environ.copy()
environment["LUMUX_AGENT_OWNER"] = owner
environment["LUMUX_AGENT_SEQUENCE"] = str(sequence)
# Only an explicit foreground assertion may take a pane from another lifecycle.
environment["LUMUX_AGENT_CLAIM"] = (
    "1" if event in ("SessionStart", "UserPromptSubmit") else "0"
)

try:
    subprocess.run(
        [os.environ["LUMUX_BIN"], "report-state", state, "--agent", "kimi"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=2,
        check=False,
    )
except Exception:
    pass
' "$state" >/dev/null 2>&1 || true
exit 0
