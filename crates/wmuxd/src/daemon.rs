//! Daemon-side runtime state: the object tree plus the live per-pane emulators
//! and PTY handles. This is the bridge between `wmux_core`'s pure model and the
//! backend's real PTYs.
//!
//! Generic over a [`PtySystem`] so the identical logic runs on the unix backend
//! (dev/CI) and the Windows ConPTY backend (Phase 10).

use std::collections::BTreeMap;

use wmux_core::grid::Grid;
use wmux_core::keymap::Keymap;
use wmux_core::model::{CascadeResult, PaneId, SessionId, Server, SplitDir};
use wmux_core::render::{compose, ClientRenderer, StatusBar, WindowView};
use wmux_core::traits::{Pty, PtySize, PtySystem, PtyWriter, ShellCommand};

/// Per-pane live state: the emulator grid and the PTY input/control handle.
pub struct LivePane<W: PtyWriter> {
    pub grid: Grid,
    pub writer: W,
    pub dead: bool,
}

/// The default shell argv when a client doesn't specify one.
pub fn default_shell() -> Vec<String> {
    if let Ok(sh) = std::env::var("SHELL") {
        vec![sh]
    } else {
        vec!["/bin/sh".to_string()]
    }
}

const SCROLLBACK: usize = 2000;

/// Owns the object model plus the live panes. One per daemon.
pub struct Daemon<S: PtySystem> {
    pub server: Server,
    pty_system: S,
    panes: BTreeMap<PaneId, LivePane<<S::Pty as Pty>::Writer>>,
    /// One keymap per attached client id.
    keymaps: BTreeMap<u64, Keymap>,
    renderers: BTreeMap<u64, ClientRenderer>,
}

impl<S: PtySystem> Daemon<S> {
    pub fn new(pty_system: S) -> Self {
        Self {
            server: Server::new(),
            pty_system,
            panes: BTreeMap::new(),
            keymaps: BTreeMap::new(),
            renderers: BTreeMap::new(),
        }
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.panes.keys().copied().collect()
    }

    pub fn live_pane_mut(
        &mut self,
        id: PaneId,
    ) -> Option<&mut LivePane<<S::Pty as Pty>::Writer>> {
        self.panes.get_mut(&id)
    }

    /// Spawn a PTY for a pane id that exists in the model, sizing it to `size`.
    /// Returns the reader so the event loop can pump the pane's output.
    fn spawn_pane(
        &mut self,
        id: PaneId,
        shell: &[String],
        size: PtySize,
    ) -> std::io::Result<<S::Pty as Pty>::Reader> {
        let cmd = ShellCommand {
            argv: shell.to_vec(),
            cwd: None,
        };
        let pty = self.pty_system.spawn(&cmd, size)?;
        let (writer, reader) = pty.split()?;
        let grid = Grid::new(size.cols as usize, size.rows as usize, SCROLLBACK);
        self.panes.insert(
            id,
            LivePane {
                grid,
                writer,
                dead: false,
            },
        );
        Ok(reader)
    }

    /// Create a new session with its first pane spawned. Returns (session, pane)
    /// and the reader for the pane so the event loop can start pumping it.
    pub fn new_session(
        &mut self,
        name: impl Into<String>,
        shell: Option<Vec<String>>,
        size: PtySize,
    ) -> std::io::Result<(SessionId, PaneId, <S::Pty as Pty>::Reader)> {
        let shell = shell.unwrap_or_else(default_shell);
        let sid = self.server.new_session(name, shell.clone());
        let pid = {
            let s = self.server.session(sid).unwrap();
            s.window(s.active_window()).unwrap().active_pane()
        };
        let reader = self.spawn_pane(pid, &shell, size)?;
        Ok((sid, pid, reader))
    }

    /// Feed PTY output bytes into a pane's emulator.
    pub fn feed_pane(&mut self, id: PaneId, bytes: &[u8]) {
        if let Some(p) = self.panes.get_mut(&id) {
            p.grid.feed(bytes);
        }
    }

    /// Write user input to a pane's PTY.
    pub fn write_pane(&mut self, id: PaneId, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(p) = self.panes.get_mut(&id) {
            p.writer.write_input(bytes)?;
        }
        Ok(())
    }

    /// Register a freshly attached client: give it a keymap and renderer.
    pub fn register_client(&mut self, client_id: u64) {
        self.keymaps.insert(client_id, Keymap::with_defaults());
        self.renderers.insert(client_id, ClientRenderer::new());
    }

    pub fn unregister_client(&mut self, client_id: u64) {
        self.keymaps.remove(&client_id);
        self.renderers.remove(&client_id);
        self.server.detach_client(client_id);
    }

    pub fn keymap_mut(&mut self, client_id: u64) -> Option<&mut Keymap> {
        self.keymaps.get_mut(&client_id)
    }

    /// Mark a pane dead (its child exited) and cascade-close it in the model.
    /// Returns the cascade result so the loop can notify/close clients.
    pub fn close_pane(&mut self, session: SessionId, pane: PaneId) -> CascadeResult {
        if let Some(p) = self.panes.get_mut(&pane) {
            p.dead = true;
        }
        self.panes.remove(&pane);
        self.server.kill_pane(session, pane)
    }

    /// Render the active window of `session` for `client_id`, returning VT bytes
    /// to send (empty if nothing changed).
    pub fn render_for_client(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        let size = self.server.effective_size(session)?;
        let s = self.server.session(session)?;
        let window = s.window(s.active_window())?;
        let layout = window.layout.clone();
        let active_pane = window.active_pane();

        // Collect references to the grids referenced by this window.
        let mut grids = BTreeMap::new();
        for pid in window.pane_ids() {
            if let Some(p) = self.panes.get(&pid) {
                grids.insert(pid, &p.grid);
            }
        }

        let status = StatusBar {
            left: format!("[{}] {}", s.name, window.name),
            right: String::new(),
        };
        let view = WindowView {
            layout: &layout,
            grids: &grids,
            active_pane,
        };
        let screen = compose((size.cols as usize, size.rows as usize), &view, Some(&status));
        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Apply a structural split to the active window of `session`, spawning the
    /// new pane's PTY. Returns the new pane's reader for pumping.
    pub fn split_active(
        &mut self,
        session: SessionId,
        dir: SplitDir,
        size: PtySize,
    ) -> std::io::Result<Option<(PaneId, <S::Pty as Pty>::Reader)>> {
        let shell = default_shell();
        let Some(pid) = self.server.split_active(session, shell.clone(), dir) else {
            return Ok(None);
        };
        let reader = self.spawn_pane(pid, &shell, size)?;
        Ok(Some((pid, reader)))
    }

    /// Create a new window in `session`, spawning its first pane's PTY. Returns
    /// the new pane's reader.
    pub fn new_window(
        &mut self,
        session: SessionId,
        size: PtySize,
    ) -> std::io::Result<Option<(PaneId, <S::Pty as Pty>::Reader)>> {
        let shell = default_shell();
        let Some(wid) = self.server.new_window(session, "", shell.clone()) else {
            return Ok(None);
        };
        let pid = {
            let s = self.server.session(session).unwrap();
            s.window(wid).unwrap().active_pane()
        };
        let reader = self.spawn_pane(pid, &shell, size)?;
        Ok(Some((pid, reader)))
    }

    /// Resize all panes of a session's active window to fit `size`.
    pub fn resize_session(&mut self, session: SessionId, size: PtySize) {
        let Some(s) = self.server.session(session) else {
            return;
        };
        let pane_ids = {
            let w = s.window(s.active_window());
            w.map(|w| w.pane_ids()).unwrap_or_default()
        };
        // For v1 each pane is resized to the full content area; precise
        // per-rect sizing is computed in the renderer. We at least keep the PTY
        // and grid in step with the layout rectangles.
        let viewport = wmux_core::layout::Rect::new(
            0,
            0,
            size.cols,
            size.rows.saturating_sub(1), // status bar row
        );
        if let Some(w) = s.window(s.active_window()) {
            let rects = wmux_core::layout::compute(&w.layout, viewport);
            for pid in pane_ids {
                if let (Some(rect), Some(p)) = (rects.get(&pid), self.panes.get_mut(&pid)) {
                    let psz = PtySize::new(rect.cols.max(1), rect.rows.max(1));
                    let _ = p.writer.resize(psz);
                    p.grid.resize(psz.cols as usize, psz.rows as usize);
                }
            }
        }
        // Mark renderers dirty so the next frame is a full repaint at new size.
        for r in self.renderers.values_mut() {
            r.invalidate();
        }
    }
}
