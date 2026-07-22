//! Server-rendered VT + damage tracking. (Phase 5)
//!
//! Compose panes + borders + status bar into a [`Screen`], then diff against
//! each client's last-sent screen to emit minimal VT. The client stays a dumb
//! VT renderer.

mod compose;
mod diff;
mod screen;
mod sgr;

pub use compose::{
    blit_window_layout, border_attrs, compose, ClientRenderer, Justify, StatusBar, StyledStatus,
    WindowView,
};
pub use diff::{diff, full_repaint};
pub use screen::{display_width, Screen};
pub use sgr::sgr_for;
pub use termwiz::cell::{Cell, CellAttributes};

#[cfg(test)]
mod tests;
