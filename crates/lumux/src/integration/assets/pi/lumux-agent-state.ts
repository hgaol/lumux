// installed by lumux
// managed by lumux; reinstalling or updating the integration overwrites this file.
// add your own extensions beside this file instead of editing it.
// LUMUX_INTEGRATION_ID=pi
// LUMUX_INTEGRATION_VERSION=1
// @ts-nocheck

import { spawn } from "node:child_process";

const AGENT = "pi";

// Provider hooks race, so every report carries a monotonic sequence the daemon
// uses to discard stale observations. Seed from the clock so an extension
// reload still orders after the previous instance.
let reportSeq = Date.now() * 1000;
function nextSeq() {
  reportSeq += 1;
  return String(reportSeq);
}

// Everything goes through the same `lumux report-state` seam the shell hooks
// use, so no pi concepts reach the daemon.
function report(state, owner, claim) {
  const pane = process.env.LUMUX_PANE;
  if (!pane || !owner) return;
  const bin = process.env.LUMUX_BIN || "lumux";
  try {
    const child = spawn(bin, ["report-state", state, "--agent", AGENT], {
      env: {
        ...process.env,
        LUMUX_AGENT_OWNER: owner,
        LUMUX_AGENT_SEQUENCE: nextSeq(),
        LUMUX_AGENT_CLAIM: claim ? "1" : "0",
      },
      stdio: "ignore",
    });
    // A reporting failure must never disturb the agent.
    child.on("error", () => {});
  } catch {
    // ignore
  }
}

function sessionIdOf(ctx) {
  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    return typeof id === "string" && id.length > 0 ? id : undefined;
  } catch {
    return undefined;
  }
}

export default function (pi) {
  // Outside a lumux pane there is nothing to report to.
  if (!process.env.LUMUX || !process.env.LUMUX_PANE) {
    return;
  }

  // Only the session that owns the UI represents this pane; background
  // sessions must not repaint it.
  let rootSession = false;
  let owner;

  pi.on("session_start", (event, ctx) => {
    if (ctx?.hasUI !== true) return;
    rootSession = true;
    owner = sessionIdOf(ctx);
    // A reload can replace this extension mid-run without another agent_start,
    // so derive the current state rather than assuming idle.
    const busy = ctx?.isIdle?.() === false;
    report(busy ? "working" : "idle", owner, true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession) return;
    owner = sessionIdOf(ctx) ?? owner;
    report("working", owner, true);
  });

  pi.on("agent_settled", (_event, ctx) => {
    if (!rootSession || ctx?.isIdle?.() !== true) return;
    report("idle", owner, false);
  });

  pi.on("session_shutdown", () => {
    if (!rootSession) return;
    // pi's process usually outlives the session inside a live shell, so clear
    // the row explicitly rather than leaving it stale.
    report("clear", owner, false);
    rootSession = false;
  });
}
