//! lumux-core: platform-independent heart of lumux.
//!
//! Contains the object model, layout engine, VT grid, wire protocol, renderer,
//! and keymap. Depends only on the platform-boundary [`traits`]; never on
//! ConPTY or named pipes.

pub mod traits;

pub mod buffers;
pub mod config;
pub mod copymode;
pub mod grid;
pub mod keymap;
pub mod layout;
pub mod model;
pub mod mouse;
pub mod proto;
pub mod render;
pub mod status;

pub use traits::{
    Clipboard, FrameReader, FrameWriter, Listener, Pty, PtySize, PtySystem, PtyWriter,
    ShellCommand, Transport,
};
