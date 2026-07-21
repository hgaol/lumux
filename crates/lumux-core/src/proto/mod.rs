//! Message types exchanged between the `lumux` client and the `lumux_server` daemon.
//!
//! These are serialized with `bincode` and carried as length-delimited frames
//! (see [`framing`]). The split is: [`ClientMsg`] flows client -> daemon,
//! [`ServerMsg`] flows daemon -> client. A [`Hello`] handshake precedes the
//! stream so version skew fails loudly instead of corrupting state.

use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u16 = 1;

/// First frame each side sends after connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    /// Human-readable build id, for logs/diagnostics.
    pub agent: String,
}

impl Hello {
    pub fn current(agent: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            agent: agent.into(),
        }
    }

    /// Check a peer's hello against our version.
    pub fn check(&self) -> Result<(), VersionMismatch> {
        if self.protocol_version == PROTOCOL_VERSION {
            Ok(())
        } else {
            Err(VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs: self.protocol_version,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionMismatch {
    pub ours: u16,
    pub theirs: u16,
}

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "protocol version mismatch: this build speaks v{}, peer speaks v{}",
            self.ours, self.theirs
        )
    }
}

impl std::error::Error for VersionMismatch {}

/// A pixel/cell viewport size on the wire (mirror of [`crate::traits::PtySize`]
/// but serializable and protocol-owned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSize {
    pub cols: u16,
    pub rows: u16,
}

impl From<crate::traits::PtySize> for WireSize {
    fn from(s: crate::traits::PtySize) -> Self {
        Self {
            cols: s.cols,
            rows: s.rows,
        }
    }
}

impl From<WireSize> for crate::traits::PtySize {
    fn from(s: WireSize) -> Self {
        Self {
            cols: s.cols,
            rows: s.rows,
        }
    }
}

/// Client -> daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Attach to a session by name (None = most-recent / create default).
    Attach {
        session: Option<String>,
        size: WireSize,
    },
    /// Create a new session and attach.
    NewSession {
        name: Option<String>,
        shell: Option<String>,
        size: WireSize,
    },
    /// Raw input bytes from the user's terminal (keystrokes).
    Input(Vec<u8>),
    /// The client's terminal was resized.
    Resize(WireSize),
    /// A structured command (the CLI verbs share this path).
    Command(Command),
    /// Detach this client; the daemon keeps the session alive.
    Detach,
}

/// Structured control commands (used by both interactive keybindings and the
/// CLI). Kept separate from raw `Input` so scripting doesn't go through key
/// encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    ListSessions,
    KillSession { target: String },
    KillServer,
    NewWindow { name: Option<String> },
    SplitWindow { horizontal: bool },
    SelectWindow { index: u32 },
    NextWindow,
    PrevWindow,
    /// Kill the active window of the client's session (tmux `kill-window`).
    KillWindow,
    SourceFile { path: String },
    SendKeys { keys: Vec<u8> },
    /// Rename the active window of the client's session.
    RenameWindow { name: String },
    /// Rename the client's session.
    RenameSession { name: String },
    /// Report a pane's agent state (tmux has no equivalent). Unlike every other
    /// verb — which acts on the *client's* active session/window — this carries
    /// an explicit `pane` target, because the reporting process (an agent hook)
    /// runs detached from the interactive client and may not be the active pane.
    /// `pane` is the `%N` id string an agent reads from `$LUMUX_PANE`.
    ReportAgentState {
        pane: String,
        agent: String,
        state: crate::agent::AgentState,
    },
}

/// Daemon -> client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Confirms an attach; carries the assigned client id and initial size.
    Attached { client_id: u64, size: WireSize },
    /// Rendered VT bytes to write to the client's terminal (damage-tracked).
    Frame(Vec<u8>),
    /// An out-of-band event the client may react to.
    Event(Event),
    /// A reply to a `Command` (e.g. session list text).
    Reply(String),
    /// The daemon is detaching/closing this client.
    Detached,
    /// A non-fatal error string for display.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// The active window or its layout changed (client may refresh title).
    LayoutChanged,
    /// A pane's process exited.
    PaneExited { pane: String, status: i32 },
    /// Terminal bell.
    Bell,
    /// The session this client viewed was destroyed.
    SessionClosed,
}

pub mod framing;

pub use framing::{decode, encode, FrameCodec, FrameError};

#[cfg(test)]
mod tests;
