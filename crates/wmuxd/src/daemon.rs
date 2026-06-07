//! Daemon-side runtime state: the object tree plus the live per-pane emulators
//! and PTY handles. This is the bridge between `wmux_core`'s pure model and the
//! backend's real PTYs.
//!
//! Generic over a [`PtySystem`] so the identical logic runs on the unix backend
//! (dev/CI) and the Windows ConPTY backend (Phase 10).

use std::collections::BTreeMap;

use wmux_core::config::Config;
use wmux_core::copymode::CopyMode;
use wmux_core::grid::Grid;
use wmux_core::keymap::{CopyKey, Keymap};
use wmux_core::model::{CascadeResult, PaneId, Server, SessionId, SplitDir};
use wmux_core::render::{compose, ClientRenderer, WindowView};
use wmux_core::traits::{Clipboard, Pty, PtySize, PtySystem, PtyWriter, ShellCommand};

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

/// Owns the object model plus the live panes. One per daemon.
pub struct Daemon<S: PtySystem> {
    pub server: Server,
    pty_system: S,
    panes: BTreeMap<PaneId, LivePane<<S::Pty as Pty>::Writer>>,
    /// One keymap per attached client id.
    keymaps: BTreeMap<u64, Keymap>,
    renderers: BTreeMap<u64, ClientRenderer>,
    /// Active copy-mode state per client (absent = not in copy-mode).
    copy: BTreeMap<u64, CopyMode>,
    /// Transient status-line message per client (tmux display-message), shown
    /// until the next render-after-input clears it.
    message: BTreeMap<u64, String>,
    /// Clients currently showing the key-binding help overlay (tmux prefix ?).
    help: std::collections::BTreeSet<u64>,
    /// Clients in the session switcher, with their current highlighted index.
    choosing: BTreeMap<u64, usize>,
    clipboard: Box<dyn Clipboard>,
    config: Config,
}

impl<S: PtySystem> Daemon<S> {
    pub fn new(pty_system: S) -> Self {
        Self::with_clipboard(pty_system, Box::new(NullClipboard))
    }

    pub fn with_clipboard(pty_system: S, clipboard: Box<dyn Clipboard>) -> Self {
        Self {
            server: Server::new(),
            pty_system,
            panes: BTreeMap::new(),
            keymaps: BTreeMap::new(),
            renderers: BTreeMap::new(),
            copy: BTreeMap::new(),
            message: BTreeMap::new(),
            help: std::collections::BTreeSet::new(),
            choosing: BTreeMap::new(),
            clipboard,
            config: Config::default(),
        }
    }

    /// Replace the active config. Existing clients' keymaps are rebuilt from it
    /// so a `source-file` reload takes effect live; scrollback/shell changes
    /// apply to subsequently spawned panes.
    pub fn set_config(&mut self, config: Config) {
        if let Ok(bindings) = config.to_bindings() {
            for km in self.keymaps.values_mut() {
                *km = Keymap::new(bindings.clone());
            }
        }
        self.config = config;
    }

    /// Scrollback lines from config.
    fn scrollback_lines(&self) -> usize {
        self.config.scrollback
    }

    /// Whether mouse reporting is enabled in the active config.
    pub fn mouse_enabled(&self) -> bool {
        self.config.mouse
    }

    /// Resolve a shell argv: explicit profile name, else config default, else
    /// the environment default.
    fn resolve_shell(&self, name: Option<&str>) -> Vec<String> {
        self.config.shell_argv(name).unwrap_or_else(default_shell)
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.panes.keys().copied().collect()
    }

    pub fn live_pane_mut(&mut self, id: PaneId) -> Option<&mut LivePane<<S::Pty as Pty>::Writer>> {
        self.panes.get_mut(&id)
    }

    /// Spawn a PTY for a pane id that exists in the model, sizing it to `size`.
    /// Returns the reader so the event loop can pump the pane's output. `cwd`
    /// sets the child's working directory (used to inherit the active pane's
    /// directory on splits/new-windows).
    fn spawn_pane(
        &mut self,
        id: PaneId,
        shell: &[String],
        size: PtySize,
        cwd: Option<String>,
    ) -> std::io::Result<<S::Pty as Pty>::Reader> {
        let cmd = ShellCommand {
            argv: shell.to_vec(),
            cwd,
        };
        let pty = self.pty_system.spawn(&cmd, size)?;
        let (writer, reader) = pty.split()?;
        let grid = Grid::new(
            size.cols as usize,
            size.rows as usize,
            self.scrollback_lines(),
        );
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

    /// The current working directory of a pane's shell, if resolvable. Used so
    /// splits and new windows open where the active pane is (tmux
    /// `#{pane_current_path}`).
    pub fn pane_cwd(&self, id: PaneId) -> Option<String> {
        let pid = self.panes.get(&id)?.writer.child_pid()?;
        resolve_cwd(pid)
    }

    /// Cwd of the active pane in a session (for inheriting on split/new-window).
    fn active_pane_cwd(&self, session: SessionId) -> Option<String> {
        let pid = self.active_pane(session)?;
        self.pane_cwd(pid)
    }

    /// Create a new session with its first pane spawned. Returns (session, pane)
    /// and the reader for the pane so the event loop can start pumping it.
    pub fn new_session(
        &mut self,
        name: impl Into<String>,
        shell: Option<Vec<String>>,
        size: PtySize,
    ) -> std::io::Result<(SessionId, PaneId, <S::Pty as Pty>::Reader)> {
        let shell = shell.unwrap_or_else(|| self.resolve_shell(None));
        let sid = self.server.new_session(name, shell.clone());
        let pid = {
            let s = self.server.session(sid).unwrap();
            s.window(s.active_window()).unwrap().active_pane()
        };
        let reader = self.spawn_pane(pid, &shell, size, None)?;
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
        let keymap = match self.config.to_bindings() {
            Ok(b) => Keymap::new(b),
            Err(_) => Keymap::with_defaults(),
        };
        self.keymaps.insert(client_id, keymap);
        self.renderers.insert(client_id, ClientRenderer::new());
    }

    pub fn unregister_client(&mut self, client_id: u64) {
        self.keymaps.remove(&client_id);
        self.renderers.remove(&client_id);
        self.copy.remove(&client_id);
        self.message.remove(&client_id);
        self.help.remove(&client_id);
        self.choosing.remove(&client_id);
        self.server.detach_client(client_id);
    }

    /// Toggle the help overlay for a client (tmux prefix ?). Showing it the
    /// first time opens it; any key (which re-emits ShowHelp) closes it.
    pub fn toggle_help(&mut self, client_id: u64) {
        if !self.help.remove(&client_id) {
            self.help.insert(client_id);
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    pub fn in_help(&self, client_id: u64) -> bool {
        self.help.contains(&client_id)
    }

    /// Open the session switcher for a client, highlighting its current session.
    pub fn open_chooser(&mut self, client_id: u64) {
        let sessions = self.server.session_ids();
        let current = self.client_session(client_id);
        let start = current
            .and_then(|sid| sessions.iter().position(|&s| s == sid))
            .unwrap_or(0);
        self.choosing.insert(client_id, start);
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    pub fn in_chooser(&self, client_id: u64) -> bool {
        self.choosing.contains_key(&client_id)
    }

    /// Force a client's next render to be a full repaint (e.g. after switching
    /// session, where the whole screen changes).
    pub fn invalidate_client(&mut self, client_id: u64) {
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Move the switcher selection. `delta` of -1/+1 = up/down; absolute index
    /// via `to`. Clamped to the session list.
    pub fn chooser_move(&mut self, client_id: u64, delta: i32, to: Option<usize>) {
        let n = self.server.session_ids().len();
        if n == 0 {
            return;
        }
        if let Some(sel) = self.choosing.get_mut(&client_id) {
            let next = match to {
                Some(i) => i.min(n - 1),
                None => {
                    let cur = *sel as i32 + delta;
                    cur.clamp(0, n as i32 - 1) as usize
                }
            };
            *sel = next;
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Confirm the switcher: move the client to the highlighted session. Returns
    /// the new session id if it changed.
    pub fn chooser_confirm(&mut self, client_id: u64) -> Option<SessionId> {
        let sel = self.choosing.remove(&client_id)?;
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        let target = *self.server.session_ids().get(sel)?;
        if self.server.set_client_session(client_id, target) {
            Some(target)
        } else {
            None
        }
    }

    /// Cancel the switcher without changing session.
    pub fn chooser_cancel(&mut self, client_id: u64) {
        if self.choosing.remove(&client_id).is_some() {
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    fn client_session(&self, client_id: u64) -> Option<SessionId> {
        self.server.session_ids().into_iter().find(|&sid| {
            self.server
                .clients_of(sid)
                .iter()
                .any(|c| c.id == client_id)
        })
    }

    /// Show a transient status-line message to a client (tmux display-message).
    /// It is rendered on the next frame and cleared after the following input.
    pub fn flash_message(&mut self, client_id: u64, msg: impl Into<String>) {
        self.message.insert(client_id, msg.into());
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Clear a client's transient message (called after the next input event).
    pub fn clear_message(&mut self, client_id: u64) {
        if self.message.remove(&client_id).is_some() {
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    pub fn keymap_mut(&mut self, client_id: u64) -> Option<&mut Keymap> {
        self.keymaps.get_mut(&client_id)
    }

    /// Whether a client is currently in copy-mode.
    pub fn in_copy_mode(&self, client_id: u64) -> bool {
        self.copy.contains_key(&client_id)
    }

    /// Enter copy-mode for `client_id`, anchored at the active pane's live tail.
    pub fn enter_copy_mode(&mut self, client_id: u64, session: SessionId) {
        if let Some(pid) = self.active_pane(session) {
            if let Some(p) = self.panes.get(&pid) {
                self.copy.insert(client_id, CopyMode::enter(&p.grid));
                if let Some(r) = self.renderers.get_mut(&client_id) {
                    r.invalidate();
                }
            }
        }
    }

    /// Feed a copy-mode navigation key. Returns true if still in copy-mode.
    /// `yank` keys (Enter/space handled by caller via start/confirm) are routed
    /// through [`Self::copy_select`] / [`Self::copy_yank`].
    pub fn copy_navigate(&mut self, client_id: u64, session: SessionId, key: CopyKey) -> bool {
        let Some(pid) = self.active_pane(session) else {
            return false;
        };
        let Some(grid) = self.panes.get(&pid).map(|p| &p.grid) else {
            return false;
        };
        let still = match self.copy.get_mut(&client_id) {
            Some(cm) => cm.navigate(key, grid),
            None => false,
        };
        if !still {
            self.copy.remove(&client_id);
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        still
    }

    /// Toggle/begin a selection at the copy cursor.
    pub fn copy_start_selection(&mut self, client_id: u64) {
        if let Some(cm) = self.copy.get_mut(&client_id) {
            cm.start_selection();
        }
    }

    /// Yank the current selection to the clipboard and exit copy-mode. Returns
    /// the OSC-52-or-similar bytes to forward to the client, if any.
    pub fn copy_yank(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        let pid = self.active_pane(session)?;
        let text = {
            let grid = &self.panes.get(&pid)?.grid;
            let cm = self.copy.get(&client_id)?;
            cm.selected_text(grid)
        };
        self.copy.remove(&client_id);
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        if text.is_empty() {
            return None;
        }
        let _ = self.clipboard.set_text(&text);
        Some(text)
    }

    fn active_pane(&self, session: SessionId) -> Option<PaneId> {
        let s = self.server.session(session)?;
        Some(s.window(s.active_window())?.active_pane())
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
        // The help overlay takes over the whole screen when active.
        if self.help.contains(&client_id) {
            return self.render_help(client_id, session);
        }
        // The session switcher likewise takes over the screen.
        if self.choosing.contains_key(&client_id) {
            return self.render_chooser(client_id, session);
        }
        // Copy-mode clients see the scrolled history view instead of live panes.
        if self.copy.contains_key(&client_id) {
            return self.render_copy_mode(client_id, session);
        }
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

        let view = WindowView {
            layout: &layout,
            grids: &grids,
            active_pane,
        };
        // Compose panes without a built-in status row; we paint a styled status
        // (or a transient message) onto the bottom row ourselves.
        let mut screen = compose((size.cols as usize, size.rows as usize), &view, None);
        self.paint_status(&mut screen, client_id, session);
        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Paint the bottom status row: a transient flash message if one is pending,
    /// otherwise the configured styled status bar.
    fn paint_status(
        &self,
        screen: &mut wmux_core::render::Screen,
        client_id: u64,
        session: SessionId,
    ) {
        use wmux_core::render::{Justify, StyledStatus};
        use wmux_core::status::{self, StatusContext};

        // A pending display-message takes over the whole row.
        if let Some(msg) = self.message.get(&client_id) {
            screen.status_line(screen.dimensions().1.saturating_sub(1), msg);
            return;
        }

        let Some(s) = self.server.session(session) else {
            return;
        };
        let window = match s.window(s.active_window()) {
            Some(w) => w,
            None => return,
        };
        let base_idx = self.config.base_index;
        let ctx = StatusContext {
            session: s.name.clone(),
            window: window.name.clone(),
            window_index: window_index(s, window.id) + base_idx,
            pane_index: base_idx,
            host: hostname(),
            time: now_parts(),
        };

        let base = StyledStatus::base_attrs(&self.config.status_bg, &self.config.status_fg);

        // Fall back to status_format if status_left is empty (simple setups).
        let left_fmt = if self.config.status_left.is_empty() {
            &self.config.status_format
        } else {
            &self.config.status_left
        };
        let styled = StyledStatus {
            left: status::format(left_fmt, &ctx),
            centre: {
                let entries: Vec<status::WindowEntry> = s
                    .window_ids()
                    .iter()
                    .enumerate()
                    .filter_map(|(i, wid)| {
                        s.window(*wid).map(|w| status::WindowEntry {
                            index: i as u32 + base_idx,
                            name: w.name.clone(),
                            active: *wid == s.active_window(),
                        })
                    })
                    .collect();
                status::window_list(&entries, &base)
            },
            right: status::format(&self.config.status_right, &ctx),
            base,
            justify: match self.config.status_justify.as_str() {
                "centre" | "center" => Justify::Centre,
                "right" => Justify::Right,
                _ => Justify::Left,
            },
        };
        styled.render(screen);
    }

    /// Render the active pane's scrolled history for a client in copy-mode.
    fn render_copy_mode(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use wmux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let pid = self.active_pane(session)?;
        let grid = &self.panes.get(&pid)?.grid;
        let cm = self.copy.get(&client_id)?;

        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let content_rows = rows.saturating_sub(1);
        let mut screen = Screen::new(cols, rows);

        let top = cm.top();
        for vy in 0..content_rows {
            if let Some(row) = grid.combined_row(top + vy) {
                screen.write_plain(0, vy, &row.to_string_full());
            }
        }
        let label = if cm.has_selection() {
            "-- COPY (selecting) --  arrows/PgUp/PgDn move, Enter yanks, q quits"
        } else {
            "-- COPY --  arrows/PgUp/PgDn move, Space selects, q quits"
        };
        screen.status_line(rows.saturating_sub(1), label);
        // Place the cursor at the copy cursor position (relative to top).
        let cur = cm.cursor();
        if cur.row >= top && cur.row < top + content_rows {
            screen.set_cursor(Some((cur.col.min(cols.saturating_sub(1)), cur.row - top)));
        }

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Render the key-binding help overlay (tmux prefix ?). Lists the active
    /// bindings, generated from this client's keymap so it reflects config.
    fn render_help(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use wmux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let mut screen = Screen::new(cols, rows);

        let entries = self
            .keymaps
            .get(&client_id)
            .map(|k| k.bindings().help_entries())
            .unwrap_or_default();

        screen.write_plain(0, 0, "wmux key bindings");
        // Two-column key/description list starting on row 2.
        let key_width = entries
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(8)
            .min(20);
        let max_y = rows.saturating_sub(1);
        for (i, (key, desc)) in entries.iter().enumerate() {
            let y = 2 + i;
            if y >= max_y {
                break;
            }
            let line = format!("  {key:<key_width$}   {desc}");
            screen.write_plain(0, y, &line);
        }
        screen.status_line(rows.saturating_sub(1), "-- HELP --  press any key to close");
        screen.set_cursor(None);

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Render the session switcher overlay (tmux prefix s): a list of sessions
    /// with the selected one highlighted, plus each session's window count.
    fn render_chooser(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use wmux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let mut screen = Screen::new(cols, rows);
        let sel = *self.choosing.get(&client_id)?;

        screen.write_plain(
            0,
            0,
            "choose a session  (Up/Down or digit, Enter selects, Esc cancels)",
        );
        let max_y = rows.saturating_sub(1);
        for (i, sid) in self.server.session_ids().iter().enumerate() {
            let y = 2 + i;
            if y >= max_y {
                break;
            }
            let Some(s) = self.server.session(*sid) else {
                continue;
            };
            let line = format!("  {}: {} ({} windows)", i, s.name, s.window_count());
            if i == sel {
                // Highlight the selected row across the full width.
                screen.status_line(y, &line);
            } else {
                screen.write_plain(0, y, &line);
            }
        }
        screen.status_line(max_y, "-- SESSIONS --");
        screen.set_cursor(None);

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
        // Capture the current pane's cwd before the split changes the active pane.
        let cwd = self.active_pane_cwd(session);
        let shell = self.resolve_shell(None);
        let Some(pid) = self.server.split_active(session, shell.clone(), dir) else {
            return Ok(None);
        };
        let reader = self.spawn_pane(pid, &shell, size, cwd)?;
        Ok(Some((pid, reader)))
    }

    /// Create a new window in `session`, spawning its first pane's PTY. Returns
    /// the new pane's reader.
    pub fn new_window(
        &mut self,
        session: SessionId,
        size: PtySize,
    ) -> std::io::Result<Option<(PaneId, <S::Pty as Pty>::Reader)>> {
        let cwd = self.active_pane_cwd(session);
        let shell = self.resolve_shell(None);
        let Some(wid) = self.server.new_window(session, "", shell.clone()) else {
            return Ok(None);
        };
        let pid = {
            let s = self.server.session(session).unwrap();
            s.window(wid).unwrap().active_pane()
        };
        let reader = self.spawn_pane(pid, &shell, size, cwd)?;
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

/// A clipboard that discards writes. Used when no system/OSC-52 clipboard is
/// wired (e.g. headless tests). The unix backend supplies `Osc52Clipboard`.
pub struct NullClipboard;

impl Clipboard for NullClipboard {
    fn set_text(&mut self, _text: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// Resolve a process's current working directory.
#[cfg(target_os = "linux")]
fn resolve_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// macOS/BSD: no /proc. cwd inheritance is best-effort and skipped for now (a
/// libproc-based lookup can be added later); new panes open in the daemon's dir.
#[cfg(all(unix, not(target_os = "linux")))]
fn resolve_cwd(_pid: u32) -> Option<String> {
    None
}

/// Windows: process cwd query (e.g. via NtQueryInformationProcess) is a
/// follow-up; new panes open in the daemon's directory for now.
#[cfg(windows)]
fn resolve_cwd(_pid: u32) -> Option<String> {
    None
}

/// 0-based position of `wid` within its session's window list.
fn window_index(session: &wmux_core::model::Session, wid: wmux_core::model::WindowId) -> u32 {
    session
        .window_ids()
        .iter()
        .position(|&w| w == wid)
        .unwrap_or(0) as u32
}

/// Local hostname for the `#H` token.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "localhost".to_string())
}

/// Current local time broken into the parts the status formatter needs.
#[cfg(unix)]
fn now_parts() -> wmux_core::status::TimeParts {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        wmux_core::status::TimeParts {
            hour: tm.tm_hour as u8,
            minute: tm.tm_min as u8,
            second: tm.tm_sec as u8,
            day: tm.tm_mday as u8,
            month: (tm.tm_mon + 1) as u8,
            year: (tm.tm_year + 1900) as u16,
        }
    }
}

#[cfg(not(unix))]
fn now_parts() -> wmux_core::status::TimeParts {
    // Windows: filled by the platform layer in a follow-up; default to zeros so
    // time tokens render as 00:00 rather than failing.
    wmux_core::status::TimeParts::default()
}
