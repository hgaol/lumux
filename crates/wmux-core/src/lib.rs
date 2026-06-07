//! wmux-core: platform-independent heart of wmux.
//!
//! Contains the object model, layout engine, VT grid, wire protocol, renderer,
//! and keymap. Depends only on the platform-boundary [`traits`]; never on
//! ConPTY or named pipes.

pub mod traits;

pub mod model;
pub mod layout;
pub mod grid;
pub mod proto;
pub mod render;
pub mod keymap;

pub use traits::{
    Clipboard, FrameReader, FrameWriter, Listener, Pty, PtySize, PtySystem, PtyWriter,
    ShellCommand, Transport,
};
