//! Windows backend: ConPTY + named-pipe transport.
//!
//! Production backend for the Windows host. Implemented in Phase 10, after all
//! platform-independent logic is built and tested via the unix backend. The
//! crate compiles on every platform (empty off-Windows) so the workspace builds
//! and CI is green before Phase 10.

#![cfg(windows)]

// Phase 10 will add: WinPtySystem (portable-pty ConPTY path:
// CreatePseudoConsole / STARTUPINFOEX / ResizePseudoConsole), a named-pipe
// Transport/Listener on \\.\pipe\wmux-<user-sid> with overlapped I/O, detached
// daemon spawn, and a Win32 Clipboard implementation.
