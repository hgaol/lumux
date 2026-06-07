//! wmux daemon library.
//!
//! The daemon owns the ConPTYs/PTYs and the object tree; it survives client
//! disconnects, which is what gives wmux tmux-style persistence. The event loop
//! is assembled in Phase 7. Exposed as a library so integration tests can drive
//! the daemon in-process against the unix backend.

/// Daemon build/version string, surfaced in the protocol handshake.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Placeholder until Phase 7 wires accept-loop + per-pane pumps + render.
pub fn describe() -> &'static str {
    "wmuxd (skeleton)"
}
