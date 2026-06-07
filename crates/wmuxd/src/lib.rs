//! wmux daemon library.
//!
//! The daemon owns the PTYs and the object tree; it survives client
//! disconnects, which is what gives wmux tmux-style persistence. Exposed as a
//! library so integration tests can drive it in-process against the unix
//! backend.

pub mod daemon;
pub mod eventloop;

pub use daemon::Daemon;
pub use eventloop::run;

/// Daemon build/version string, surfaced in the protocol handshake.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
