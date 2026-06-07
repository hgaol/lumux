//! Unix backend: real Unix PTY + Unix-domain socket transport.
//!
//! This is the development/CI backend. It lets the entire wmux daemon and
//! client run end-to-end on Linux, so every platform-independent concern is
//! exercised before the Windows (ConPTY/named-pipe) backend is written.
//!
//! Implementations land in Phase 7. This file currently establishes the crate
//! and its platform gate.

#![cfg(unix)]

// Phase 7 will add: UnixPtySystem (portable-pty native path),
// UnixSocketTransport / UnixSocketListener, and a unix Clipboard
// (xclip / OSC-52 / in-memory fake).
