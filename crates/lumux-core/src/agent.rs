//! Per-pane agent status: which agent a pane is running and what it's doing.
//!
//! Unlike herdr — which infers state by scraping each pane's screen against
//! per-agent pattern manifests — lumux has agents **self-report** via the
//! `lumux report-state` CLI wired into each agent's own hooks. The daemon holds
//! the latest report per pane (see the daemon's `agent_status` map) and the
//! sidebar / session chooser surface it. State is live and transient: it is
//! never persisted (a restart makes any "working" stale by construction) and is
//! cleared when the pane's process exits.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// What an agent in a pane is currently doing. Mirrors the states herdr shows,
/// most-urgent last: `Blocked` ("needs your input") is the highest-value glance
/// state — it tells you which agent is waiting on *you*.
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
            AgentState::Working => 3,
            AgentState::Done => 2,
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

/// A pane's reported agent identity plus its current state. `agent` is a free
/// label (e.g. `"claude"`); it isn't validated against a known set, so new
/// agents work without a code change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent: String,
    pub state: AgentState,
}

impl AgentStatus {
    pub fn new(agent: impl Into<String>, state: AgentState) -> Self {
        Self {
            agent: agent.into(),
            state,
        }
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
}
