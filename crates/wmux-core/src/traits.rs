//! Platform boundary traits.
//!
//! `wmux-core` never references ConPTY or named pipes directly. Backends
//! (`wmux-backend-unix`, `wmux-backend-win`) implement these traits so the
//! entire daemon + client can run on Linux for development against a
//! Unix-PTY / Unix-socket backend, and on Windows against ConPTY / named pipes.

use std::io;

/// Size of a pseudo-terminal / pane viewport, in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl Default for PtySize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// How to launch the child process backing a pane.
#[derive(Debug, Clone)]
pub struct ShellCommand {
    /// argv[0] is the executable; remainder are arguments.
    pub argv: Vec<String>,
    /// Working directory, or None for the daemon's default.
    pub cwd: Option<String>,
}

impl ShellCommand {
    pub fn new(argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: None,
        }
    }
}

/// A spawned pseudo-terminal hosting one shell process.
///
/// The read side is intentionally blocking: `portable-pty`'s reader is a
/// blocking `io::Read`, and on the daemon side each pane owns a dedicated
/// reader thread that bridges bytes into the async event loop via a channel.
/// Keeping the trait blocking maps 1:1 onto both backends without forcing an
/// async PTY abstraction that neither platform provides natively.
pub trait Pty: Send {
    /// A handle for writing input + resizing, separable from the reader so the
    /// reader can be moved to its own thread.
    type Writer: PtyWriter;
    type Reader: io::Read + Send;

    /// Split into an input/control handle and an output reader.
    fn split(self) -> io::Result<(Self::Writer, Self::Reader)>;
}

/// The input/control half of a [`Pty`].
pub trait PtyWriter: Send {
    /// Write bytes to the child's stdin.
    fn write_input(&mut self, data: &[u8]) -> io::Result<()>;
    /// Resize the pseudo-terminal, notifying the child (SIGWINCH-equivalent).
    fn resize(&mut self, size: PtySize) -> io::Result<()>;
    /// Best-effort: has the child exited? `Some(code)` if so.
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
}

/// Spawns [`Pty`] instances. Backend-provided.
pub trait PtySystem: Send + Sync {
    type Pty: Pty;
    fn spawn(&self, cmd: &ShellCommand, size: PtySize) -> io::Result<Self::Pty>;
}

/// A bidirectional, message-framed connection between a client and the daemon.
///
/// Splittable into independent read and write halves so a reader thread can
/// block on the socket while a writer thread concurrently sends frames — no
/// shared lock, no head-of-line blocking. Unix sockets and Windows named pipes
/// both support this via handle cloning. Framing (length prefix) lives in
/// `proto`; these carry already-encoded frames.
pub trait Transport: Send {
    type Reader: FrameReader;
    type Writer: FrameWriter;
    /// Split into (reader, writer) halves backed by the same connection.
    fn split(self) -> io::Result<(Self::Reader, Self::Writer)>;
}

/// The read half of a [`Transport`].
pub trait FrameReader: Send {
    /// Read exactly one length-delimited frame. Returns `Ok(None)` on clean EOF.
    fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>>;
}

/// The write half of a [`Transport`].
pub trait FrameWriter: Send {
    /// Write one already-encoded length-delimited frame.
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()>;
}

/// Accepts incoming client [`Transport`] connections on the daemon side.
pub trait Listener: Send {
    type Conn: Transport;
    /// Block until a client connects.
    fn accept(&mut self) -> io::Result<Self::Conn>;
}

/// Writes copied text to a system (or remote) clipboard. Backend-provided.
///
/// The unix backend uses `xclip`/OSC-52/an in-memory fake; the windows backend
/// uses the Win32 clipboard. OSC-52 lets copy-mode populate the *client's*
/// local clipboard through the terminal, which matters for remote attach.
pub trait Clipboard: Send {
    fn set_text(&mut self, text: &str) -> io::Result<()>;
}
