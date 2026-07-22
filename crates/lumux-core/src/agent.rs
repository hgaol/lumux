//! Per-pane agent status: which agent a pane is running and what it's doing.
//!
//! Unlike herdr — which infers state by scraping each pane's screen against
//! per-agent pattern manifests — lumux has agents **self-report** via the
//! `lumux report-state` CLI wired into each agent's own hooks. The daemon holds
//! the latest owned lifecycle per pane and the sidebar / session chooser
//! surface it. State is live and transient: it is
//! never persisted (a restart makes any "working" stale by construction) and is
//! cleared when the pane's process exits.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// What an agent in a pane is currently doing. Mirrors the states herdr shows;
/// `Blocked` ("needs your input") is the highest-value glance state because it
/// tells you which agent is waiting on *you*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// Agent finished, prompt visible, nothing happening.
    Idle,
    /// Agent is actively working / processing.
    Working,
    /// Agent needs human input and is blocked on a response.
    Blocked,
    /// Agent's task completed (a terminal, self-reported "done").
    Done,
    /// Plain shell or an unrecognized / unreported program.
    Unknown,
}

impl AgentState {
    /// The canonical lowercase name, used on the wire and by the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Unknown => "unknown",
        }
    }

    /// Ranking for "most urgent wins" when several panes' states are summarized
    /// into one glyph (e.g. a window row in the chooser). Higher = more urgent.
    /// Blocked outranks everything because it's the state that needs you now.
    pub fn urgency(self) -> u8 {
        match self {
            AgentState::Blocked => 4,
            // Match Herdr's attention ordering: an unseen completion should be
            // surfaced before background work when panes roll up to one row.
            AgentState::Done => 3,
            AgentState::Working => 2,
            AgentState::Idle => 1,
            AgentState::Unknown => 0,
        }
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error from parsing an [`AgentState`] name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStateParseError(pub String);

impl fmt::Display for AgentStateParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid agent state {:?} (expected idle, working, blocked, done, or unknown)",
            self.0
        )
    }
}

impl std::error::Error for AgentStateParseError {}

impl FromStr for AgentState {
    type Err = AgentStateParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "idle" => Ok(AgentState::Idle),
            "working" => Ok(AgentState::Working),
            "blocked" => Ok(AgentState::Blocked),
            "done" => Ok(AgentState::Done),
            "unknown" => Ok(AgentState::Unknown),
            _ => Err(AgentStateParseError(s.to_string())),
        }
    }
}

/// Stable identity for one provider lifecycle in a pane.
///
/// The provider label is intentionally part of the identity: a delayed Claude
/// exit must not clear a Codex process that subsequently took over the same
/// shell. `owner` distinguishes consecutive sessions of the same provider.
/// `None` identifies legacy/manual reporters and only matches an equally
/// unowned lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub agent: String,
    pub owner: Option<String>,
}

impl AgentIdentity {
    pub fn new(agent: impl Into<String>, owner: Option<String>) -> Self {
        Self {
            agent: agent.into(),
            owner,
        }
    }
}

/// One ordered agent-state observation for a pane.
///
/// Provider hooks run in independent processes, so `sequence` orders their
/// observations. `claim` is an explicit foreground-ownership assertion (for
/// example SessionStart or UserPromptSubmit); only a claim may replace a
/// different current lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    pub identity: AgentIdentity,
    pub claim: bool,
    pub state: AgentState,
    pub sequence: u64,
}

impl AgentReport {
    pub fn new(identity: AgentIdentity, claim: bool, state: AgentState, sequence: u64) -> Self {
        Self {
            identity,
            claim,
            state,
            sequence,
        }
    }
}

/// One ordered request to end an exact agent lifecycle in a pane.
///
/// The daemon retains accepted clears as tombstones so an older in-flight
/// report cannot resurrect an exited agent. Identity matching prevents a
/// delayed exit from clearing a replacement lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentClear {
    pub identity: AgentIdentity,
    pub sequence: u64,
}

impl AgentClear {
    pub fn new(identity: AgentIdentity, sequence: u64) -> Self {
        Self { identity, sequence }
    }
}

/// A pane's reported agent identity plus its current semantic state. `agent` is
/// a free label (e.g. `"claude"`); it isn't validated against a known set, so
/// new agents work without a code change.
///
/// Completion is deliberately represented separately from the reported state:
/// an agent reports `working -> idle`, while the UI displays that idle state as
/// `done` until the user focuses the pane. This mirrors the notification model
/// used by herdr and avoids asking integrations to invent a durable state that
/// really means "the user has not looked yet".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub agent: String,
    /// The semantic state reported by the agent. `Done` is accepted at the
    /// input seam for backwards compatibility, but normalized to `Idle` plus an
    /// unacknowledged completion by [`Self::new`] / [`Self::apply_report`].
    state: AgentState,
    acknowledged: bool,
}

impl AgentStatus {
    pub fn new(agent: impl Into<String>, state: AgentState) -> Self {
        let mut status = Self {
            agent: agent.into(),
            state: AgentState::Unknown,
            acknowledged: true,
        };
        status.apply_report_inner(state, false, false);
        status
    }

    /// Apply a newer report to this pane. A transition from active (or from an
    /// inconclusive `Unknown` observation for the same agent) to idle creates an
    /// unacknowledged completion; repeated idle reports preserve it.
    pub fn apply_report(&mut self, agent: impl Into<String>, state: AgentState) {
        let agent = agent.into();
        let same_agent = self.agent == agent;
        self.agent = agent;
        self.apply_report_inner(state, true, same_agent);
    }

    fn apply_report_inner(&mut self, state: AgentState, has_previous: bool, same_agent: bool) {
        let became_idle_after_activity = has_previous
            && (matches!(self.state, AgentState::Working | AgentState::Blocked)
                // Herdr treats the first conclusive idle observation for an
                // already-known agent as a completion too. Agent identity is
                // load-bearing: replacing an unknown agent with a new idle one
                // is initial state, not a notification.
                || (same_agent && self.state == AgentState::Unknown));
        let was_unacknowledged_idle =
            has_previous && self.state == AgentState::Idle && !self.acknowledged;

        match state {
            // Keep accepting the original public `done` report, but store it in
            // the same shape as a derived completion so acknowledgement has one
            // implementation.
            AgentState::Done => {
                self.state = AgentState::Idle;
                self.acknowledged = false;
            }
            AgentState::Idle => {
                self.state = AgentState::Idle;
                self.acknowledged = if became_idle_after_activity {
                    false
                } else if was_unacknowledged_idle {
                    // Duplicate Stop/idle hooks must not dismiss the badge.
                    false
                } else {
                    true
                };
            }
            other => {
                self.state = other;
                self.acknowledged = true;
            }
        }
    }

    /// State shown in the sidebar and chooser. An idle-but-unacknowledged pane
    /// is the derived `Done` presentation state.
    pub fn display_state(&self) -> AgentState {
        if self.state == AgentState::Idle && !self.acknowledged {
            AgentState::Done
        } else {
            self.state
        }
    }

    /// The lifecycle state reported by the integration, before the derived
    /// unseen-completion presentation is applied.
    pub fn semantic_state(&self) -> AgentState {
        self.state
    }

    /// Mark a completion as seen. Returns true only when the visible state
    /// changed (`Done -> Idle`), allowing callers to avoid needless repaints.
    pub fn acknowledge(&mut self) -> bool {
        if self.state == AgentState::Idle && !self.acknowledged {
            self.acknowledged = true;
            true
        } else {
            false
        }
    }

    pub fn is_acknowledged(&self) -> bool {
        self.acknowledged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_string_roundtrips() {
        for s in [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Done,
            AgentState::Unknown,
        ] {
            assert_eq!(s.to_string().parse::<AgentState>(), Ok(s));
        }
    }

    #[test]
    fn state_parse_is_case_insensitive_and_trims() {
        assert_eq!("WORKING".parse::<AgentState>(), Ok(AgentState::Working));
        assert_eq!("  blocked ".parse::<AgentState>(), Ok(AgentState::Blocked));
    }

    #[test]
    fn unknown_string_is_an_error() {
        let err = "frobnicating".parse::<AgentState>().unwrap_err();
        assert_eq!(err.0, "frobnicating");
        // The message names the valid options so the CLI can surface it.
        assert!(err.to_string().contains("idle, working, blocked"));
    }

    #[test]
    fn blocked_is_the_most_urgent() {
        let states = [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Done,
            AgentState::Unknown,
        ];
        for s in states {
            assert!(
                AgentState::Blocked.urgency() > s.urgency(),
                "blocked must outrank {s}"
            );
        }
    }

    #[test]
    fn unseen_completion_outranks_working() {
        assert!(AgentState::Done.urgency() > AgentState::Working.urgency());
    }

    #[test]
    fn active_to_idle_is_done_until_acknowledged() {
        let mut status = AgentStatus::new("claude", AgentState::Working);
        status.apply_report("claude", AgentState::Idle);

        assert_eq!(status.semantic_state(), AgentState::Idle);
        assert_eq!(status.display_state(), AgentState::Done);
        assert!(!status.is_acknowledged());
        assert!(status.acknowledge());
        assert_eq!(status.display_state(), AgentState::Idle);
        assert!(!status.acknowledge(), "acknowledgement is idempotent");
    }

    #[test]
    fn initial_idle_is_not_a_completion() {
        let status = AgentStatus::new("claude", AgentState::Idle);
        assert_eq!(status.display_state(), AgentState::Idle);
        assert!(status.is_acknowledged());
    }

    #[test]
    fn unknown_to_idle_for_the_same_agent_is_an_unseen_completion() {
        let mut status = AgentStatus::new("claude", AgentState::Unknown);

        status.apply_report("claude", AgentState::Idle);

        assert_eq!(status.semantic_state(), AgentState::Idle);
        assert_eq!(status.display_state(), AgentState::Done);
        assert!(!status.is_acknowledged());
    }

    #[test]
    fn unknown_to_idle_after_an_agent_replacement_is_initial_idle() {
        let mut status = AgentStatus::new("claude", AgentState::Unknown);

        status.apply_report("codex", AgentState::Idle);

        assert_eq!(status.display_state(), AgentState::Idle);
        assert!(status.is_acknowledged());
    }

    #[test]
    fn duplicate_idle_does_not_dismiss_completion() {
        let mut status = AgentStatus::new("claude", AgentState::Working);
        status.apply_report("claude", AgentState::Idle);
        status.apply_report("claude", AgentState::Idle);
        assert_eq!(status.display_state(), AgentState::Done);
    }

    #[test]
    fn explicit_done_uses_the_same_acknowledgement_path() {
        let mut status = AgentStatus::new("custom", AgentState::Done);
        assert_eq!(status.semantic_state(), AgentState::Idle);
        assert_eq!(status.display_state(), AgentState::Done);
        assert!(status.acknowledge());
        assert_eq!(status.display_state(), AgentState::Idle);
    }
}
