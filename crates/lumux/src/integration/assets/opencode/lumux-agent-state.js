// installed by lumux
// managed by lumux; reinstalling or updating the integration overwrites this file.
// add your own plugins beside this file instead of editing it.
// LUMUX_INTEGRATION_ID=opencode
// LUMUX_INTEGRATION_VERSION=1

import { spawn } from "node:child_process";

const AGENT = "opencode";

// Provider hooks race, so every report carries a monotonic sequence the daemon
// uses to discard stale observations. Seed from the clock so a plugin reload
// still orders after the previous instance.
let reportSeq = Date.now() * 1000;
function nextSeq() {
  reportSeq += 1;
  return String(reportSeq);
}

// Subagent (task tool) sessions carry a parentID; the root session does not.
// Their lifecycle would otherwise repaint the pane, so learn their ids from
// session.created/updated and drop their events.
const childSessions = new Set();

function sessionIdOf(properties) {
  const id = properties?.sessionID;
  return typeof id === "string" && id ? id : undefined;
}

// session.status carries { type: "idle" | "busy" | "retry" }; older builds sent
// a bare string.
function stateFromStatus(status) {
  const kind = typeof status === "string" ? status : status?.type;
  if (typeof kind !== "string") return undefined;
  switch (kind.toLowerCase()) {
    case "idle":
      return "idle";
    case "active":
    case "busy":
    case "pending":
    case "running":
    case "streaming":
    case "working":
    case "retry":
      return "working";
    default:
      return undefined;
  }
}

// Everything goes through the same `lumux report-state` seam the shell hooks
// use, so no opencode concepts reach the daemon.
function reportState(state, sessionID, claim) {
  const pane = process.env.LUMUX_PANE;
  if (!pane || !sessionID) return;
  const bin = process.env.LUMUX_BIN || "lumux";
  try {
    const child = spawn(bin, ["report-state", state, "--agent", AGENT], {
      env: {
        ...process.env,
        LUMUX_AGENT_OWNER: sessionID,
        LUMUX_AGENT_SEQUENCE: nextSeq(),
        LUMUX_AGENT_CLAIM: claim ? "1" : "0",
      },
      stdio: "ignore",
      detached: false,
    });
    // A reporting failure must never disturb the agent.
    child.on("error", () => {});
  } catch {
    // ignore
  }
}

export const LumuxAgentStatePlugin = async () => {
  // Outside a lumux pane there is nothing to report to.
  if (!process.env.LUMUX || !process.env.LUMUX_PANE) {
    return {};
  }

  return {
    "chat.message": async ({ sessionID }) => {
      if (sessionID && childSessions.has(sessionID)) return;
      // Submitting a prompt is an explicit foreground assertion.
      reportState("working", sessionID, true);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIdOf(properties);

      const info = properties.info;
      if (info?.id && info.parentID) {
        childSessions.add(info.id);
      }
      // Subagent events are dropped: only the root session owns the pane.
      if (sessionID && childSessions.has(sessionID)) return;

      switch (type) {
        case "session.created":
          // A root session start claims the pane so it appears immediately.
          reportState("idle", sessionID, true);
          break;
        case "session.status": {
          const state = stateFromStatus(properties.status);
          if (state) reportState(state, sessionID, false);
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          reportState("working", sessionID, false);
          break;
        case "permission.asked":
        case "question.asked":
        case "session.error":
          reportState("blocked", sessionID, false);
          break;
        case "session.idle":
          reportState("idle", sessionID, false);
          break;
        default:
          break;
      }
    },
  };
};
