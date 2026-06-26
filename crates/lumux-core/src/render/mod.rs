//! Server-rendered VT + damage tracking. (Phase 5)
//!
//! Compose panes + borders + status bar into a [`Screen`], then diff against
//! each client's last-sent screen to emit minimal VT. The client stays a dumb
//! VT renderer.

mod compose;
mod diff;
mod screen;
mod sgr;

pub use compose::{border_attrs, compose, ClientRenderer, Justify, StatusBar, StyledStatus, WindowView};
pub use termwiz::cell::CellAttributes;
pub use diff::{diff, full_repaint};
pub use screen::Screen;
pub use sgr::sgr_for;

#[cfg(test)]
mod tests;
