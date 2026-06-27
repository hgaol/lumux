//! Object model: Server owns Sessions own Windows own a pane tree; Panes are
//! leaves. Clients are a separate registry referencing a session + viewport.
//!
//! This module is pure state and structural operations — no I/O, no platform
//! types. The daemon (Phase 7) drives these operations in response to client
//! commands and attaches real PTYs/grids to [`Pane`]s.

mod id;
mod tree;

pub use id::{IdParseError, PaneId, SessionId, WindowId};
pub use tree::{LayoutKind, PaneNode, Removed, SplitDir};

use crate::traits::PtySize;
use std::collections::BTreeMap;

/// A leaf in the window's pane tree. In Phase 1 it carries only identity and
/// metadata; the daemon later associates a PTY + VT grid with each pane by id.
#[derive(Debug, Clone)]
pub struct Pane {
    pub id: PaneId,
    /// argv used to spawn this pane's shell (for display / respawn).
    pub shell: Vec<String>,
}

impl Pane {
    pub fn new(id: PaneId, shell: Vec<String>) -> Self {
        Self { id, shell }
    }
}

/// A window: a named "tab" filling the client viewport, holding a pane tree and
/// tracking which pane has input focus.
#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub name: String,
    pub layout: PaneNode,
    panes: BTreeMap<PaneId, Pane>,
    active_pane: PaneId,
    /// The previously-active pane, for tmux's `last-pane` (prefix `;`). Updated
    /// whenever focus moves to a different pane.
    last_pane: Option<PaneId>,
    /// The last preset layout applied via `next-layout` (prefix Space), if any.
    /// `next_layout` cycles from here; manual splits clear it back to None.
    layout_kind: Option<LayoutKind>,
    /// When `Some`, the active pane is zoomed: the renderer shows only this pane
    /// fullscreen (tmux prefix z). Cleared on toggle, on focus change, and when
    /// the zoomed pane goes away.
    zoomed: Option<PaneId>,
    /// When true, input typed into any pane of this window is broadcast to all
    /// of them (tmux `synchronize-panes`).
    synchronized: bool,
}

impl Window {
    fn new(id: WindowId, name: String, first_pane: Pane) -> Self {
        let active_pane = first_pane.id;
        let layout = PaneNode::leaf(first_pane.id);
        let mut panes = BTreeMap::new();
        panes.insert(first_pane.id, first_pane);
        Self {
            id,
            name,
            layout,
            panes,
            active_pane,
            last_pane: None,
            layout_kind: None,
            zoomed: None,
            synchronized: false,
        }
    }

    pub fn active_pane(&self) -> PaneId {
        self.active_pane
    }

    /// Set the active pane, remembering the prior one for `last-pane`. Changing
    /// focus always unzooms (tmux behavior). No-op bookkeeping if it's the same
    /// pane. Use this for every focus change so `last_pane` stays correct.
    fn set_active_pane(&mut self, id: PaneId) {
        if id != self.active_pane {
            self.last_pane = Some(self.active_pane);
            self.active_pane = id;
            self.zoomed = None;
        }
    }

    /// Focus the previously-active pane (tmux `last-pane`, prefix `;`). No-op if
    /// there is no remembered pane or it has since closed.
    pub fn focus_last_pane(&mut self) -> bool {
        if let Some(prev) = self.last_pane {
            if self.panes.contains_key(&prev) {
                self.set_active_pane(prev);
                return true;
            }
        }
        false
    }

    /// The zoomed pane, if any (tmux prefix z). The renderer shows only this pane.
    pub fn zoomed_pane(&self) -> Option<PaneId> {
        self.zoomed
    }

    /// Toggle zoom on the active pane. Zooming a single-pane window is a no-op
    /// (nothing to maximize). Returns the new zoom state.
    pub fn toggle_zoom(&mut self) -> bool {
        if self.zoomed.is_some() {
            self.zoomed = None;
        } else if self.pane_count() > 1 {
            self.zoomed = Some(self.active_pane);
        }
        self.zoomed.is_some()
    }

    /// Whether input is broadcast to all panes (tmux synchronize-panes).
    pub fn is_synchronized(&self) -> bool {
        self.synchronized
    }

    /// Toggle synchronize-panes for this window. Returns the new state.
    pub fn toggle_synchronized(&mut self) -> bool {
        self.synchronized = !self.synchronized;
        self.synchronized
    }

    /// Adjust the divider nearest the active pane (tmux resize-pane). `axis` picks
    /// horizontal vs. vertical splits; `step` is signed ratio space (positive =
    /// toward the second child: right/down).
    pub fn resize_active(&mut self, axis: SplitDir, step: f32) -> bool {
        self.layout.resize_pane(self.active_pane, axis, step)
    }

    /// Apply a preset layout (tmux `select-layout`), rebuilding the pane tree
    /// over the existing panes with even ratios. Pane ids and the active pane are
    /// preserved; unzooms. Records the layout so [`next_layout`] cycles from it.
    pub fn apply_layout(&mut self, kind: LayoutKind) {
        let panes = self.layout.pane_ids();
        self.layout = PaneNode::arrange(kind, &panes);
        self.layout_kind = Some(kind);
        self.zoomed = None;
    }

    /// Cycle to the next preset layout (tmux `next-layout`, prefix Space). Starts
    /// at even-horizontal if no preset is active yet. No-op for a single pane.
    pub fn next_layout(&mut self) {
        if self.pane_count() < 2 {
            return;
        }
        let next = match self.layout_kind {
            Some(k) => k.next(),
            None => LayoutKind::CYCLE[0],
        };
        self.apply_layout(next);
    }

    /// Swap the active pane with pane `other` in this window's layout (tmux
    /// swap-pane). Both panes keep their ids (and thus their grids); only their
    /// positions in the tree exchange. The active pane stays active but moves to
    /// `other`'s old slot. No-op (false) if `other` isn't in this window or is
    /// the active pane itself.
    pub fn swap_with_active(&mut self, other: PaneId) -> bool {
        let active = self.active_pane;
        if self.layout.swap_ids(active, other) {
            // Focus follows the pane (it kept its id), so active_pane is unchanged.
            self.zoomed = None;
            self.layout_kind = None;
            true
        } else {
            false
        }
    }

    /// Remove the active pane from this window WITHOUT discarding it, returning
    /// the [`Pane`] so it can be re-homed in another window (tmux break-pane).
    /// Returns None when this is the only pane — break-pane on a lone pane is a
    /// no-op in tmux, since moving an only-child to a new window changes nothing.
    fn take_active_pane(&mut self) -> Option<Pane> {
        if self.pane_count() == 1 {
            return None;
        }
        let id = self.active_pane;
        let pane = self.panes.get(&id)?.clone();
        self.remove_pane(id);
        Some(pane)
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.layout.pane_ids()
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Split the active pane, inserting `new` next to it. The new pane becomes
    /// active (tmux behavior).
    fn split(&mut self, new: Pane, dir: SplitDir) {
        let target = self.active_pane;
        let new_id = new.id;
        if self.layout.split_leaf(target, new_id, dir) {
            self.panes.insert(new_id, new);
            self.set_active_pane(new_id);
            // A manual split no longer matches any preset layout.
            self.layout_kind = None;
        }
    }

    /// Remove a pane. Returns true if the window is now empty (caller closes
    /// the window).
    fn remove_pane(&mut self, id: PaneId) -> bool {
        // Removing the zoomed pane (or collapsing the layout) clears zoom.
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        // A removed pane can't be the "last" pane to jump back to.
        if self.last_pane == Some(id) {
            self.last_pane = None;
        }
        // The tree changes shape on removal; it no longer matches a preset.
        self.layout_kind = None;
        match self.layout.remove_pane(id) {
            Removed::NotFound => false,
            Removed::Gone => {
                self.panes.remove(&id);
                true
            }
            Removed::Collapsed => {
                self.panes.remove(&id);
                self.zoomed = None;
                if self.active_pane == id {
                    // Focus the first remaining pane.
                    self.active_pane = self.layout.pane_ids()[0];
                }
                false
            }
        }
    }

    /// Move focus to the next pane in traversal order (wraps).
    pub fn focus_next_pane(&mut self) {
        let ids = self.layout.pane_ids();
        if let Some(pos) = ids.iter().position(|&i| i == self.active_pane) {
            self.set_active_pane(ids[(pos + 1) % ids.len()]);
        }
    }

    /// Move focus geographically (tmux select-pane -L/-R/-U/-D) given a viewport
    /// to compute pane rectangles. No-op if there is no pane in that direction.
    pub fn focus_direction(
        &mut self,
        dir: crate::layout::Direction,
        viewport: crate::layout::Rect,
    ) {
        let rects = crate::layout::compute(&self.layout, viewport);
        if let Some(next) = crate::layout::neighbor(&rects, self.active_pane, dir) {
            self.set_active_pane(next);
        }
    }

    pub fn focus_pane(&mut self, id: PaneId) -> bool {
        if self.panes.contains_key(&id) {
            self.set_active_pane(id);
            true
        } else {
            false
        }
    }

    /// Which divider line (if any) the point (col,row) sits on, as a path for
    /// [`Self::drag_divider`]. Called on mouse-press to decide whether a divider
    /// was grabbed. None when the point is in open pane area.
    pub fn divider_at(&self, col: u16, row: u16, viewport: crate::layout::Rect) -> Option<Vec<bool>> {
        crate::layout::divider_at(&self.layout, col, row, viewport)
    }

    /// Drag the previously-grabbed divider (by `path`) to follow the cursor at
    /// (col,row). Returns true if it resolved to a split. The path is captured on
    /// press so the divider tracks the pointer even once it moves off the line.
    pub fn drag_divider(
        &mut self,
        path: &[bool],
        col: u16,
        row: u16,
        viewport: crate::layout::Rect,
    ) -> bool {
        crate::layout::set_ratio_by_path(&mut self.layout, path, col, row, viewport)
    }
}

/// A session: the unit a client attaches to. Owns an ordered list of windows
/// and tracks the active one. Survives client detach.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    windows: Vec<Window>,
    active_window: WindowId,
    /// The previously-active window, for tmux's `last-window` (prefix `l`).
    last_window: Option<WindowId>,
}

impl Session {
    fn new(id: SessionId, name: String, first_window: Window) -> Self {
        let active_window = first_window.id;
        Self {
            id,
            name,
            windows: vec![first_window],
            active_window,
            last_window: None,
        }
    }

    pub fn active_window(&self) -> WindowId {
        self.active_window
    }

    /// Set the active window, remembering the prior one for `last-window`. No-op
    /// bookkeeping when it's already active. All window-focus changes go through
    /// here so `last_window` stays correct.
    fn set_active_window(&mut self, id: WindowId) {
        if id != self.active_window {
            self.last_window = Some(self.active_window);
            self.active_window = id;
        }
    }

    /// Focus the previously-active window (tmux `last-window`, prefix `l`). No-op
    /// if there is no remembered window or it has since closed.
    pub fn focus_last_window(&mut self) -> bool {
        if let Some(prev) = self.last_window {
            if self.windows.iter().any(|w| w.id == prev) {
                self.set_active_window(prev);
                return true;
            }
        }
        false
    }

    pub fn window_ids(&self) -> Vec<WindowId> {
        self.windows.iter().map(|w| w.id).collect()
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn active_window_mut(&mut self) -> &mut Window {
        let id = self.active_window;
        self.window_mut(id).expect("active window always exists")
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    fn add_window(&mut self, w: Window) {
        let id = w.id;
        self.windows.push(w);
        self.set_active_window(id);
    }

    /// Remove a window. Returns true if the session is now empty (caller closes
    /// the session).
    fn remove_window(&mut self, id: WindowId) -> bool {
        let Some(pos) = self.windows.iter().position(|w| w.id == id) else {
            return false;
        };
        self.windows.remove(pos);
        // A removed window can't be the "last" window to jump back to.
        if self.last_window == Some(id) {
            self.last_window = None;
        }
        if self.windows.is_empty() {
            return true;
        }
        if self.active_window == id {
            // Prefer jumping to the remembered last window; else the previous
            // window in order (clamped), tmux-ish.
            let target = self
                .last_window
                .filter(|lw| self.windows.iter().any(|w| w.id == *lw))
                .unwrap_or_else(|| {
                    let new_idx = pos.saturating_sub(1).min(self.windows.len() - 1);
                    self.windows[new_idx].id
                });
            // Set directly: the window being removed shouldn't become last_window.
            self.active_window = target;
        }
        false
    }

    pub fn focus_next_window(&mut self) {
        if let Some(pos) = self.windows.iter().position(|w| w.id == self.active_window) {
            let next = self.windows[(pos + 1) % self.windows.len()].id;
            self.set_active_window(next);
        }
    }

    pub fn focus_prev_window(&mut self) {
        if let Some(pos) = self.windows.iter().position(|w| w.id == self.active_window) {
            let n = self.windows.len();
            let prev = self.windows[(pos + n - 1) % n].id;
            self.set_active_window(prev);
        }
    }

    /// Move the active window one slot earlier (`delta < 0`) or later (`delta >
    /// 0`) in the window list (tmux swap-window with the neighbor). Wraps around
    /// the ends so moving the first window left puts it last, matching the
    /// next/prev navigation feel. No-op for a single window. Returns true if the
    /// order changed.
    pub fn move_active_window(&mut self, delta: i32) -> bool {
        let n = self.windows.len();
        if n < 2 {
            return false;
        }
        let Some(pos) = self.windows.iter().position(|w| w.id == self.active_window) else {
            return false;
        };
        // Target index with wraparound.
        let target = (pos as i32 + delta).rem_euclid(n as i32) as usize;
        if target == pos {
            return false;
        }
        self.windows.swap(pos, target);
        true
    }

    pub fn focus_window(&mut self, id: WindowId) -> bool {
        if self.windows.iter().any(|w| w.id == id) {
            self.set_active_window(id);
            true
        } else {
            false
        }
    }
}

/// An attached client. Not part of the object tree — a separate registry entry.
/// Carries this connection's viewport size and which session it views.
#[derive(Debug, Clone)]
pub struct Client {
    pub id: u64,
    pub session: SessionId,
    pub size: PtySize,
}

/// The whole daemon state: all sessions and all attached clients.
#[derive(Debug, Default)]
pub struct Server {
    sessions: BTreeMap<SessionId, Session>,
    clients: BTreeMap<u64, Client>,
    next_client_id: u64,
}

impl Server {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session_ids(&self) -> Vec<SessionId> {
        self.sessions.keys().copied().collect()
    }

    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    pub fn session_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.sessions.get_mut(&id)
    }

    pub fn find_session_by_name(&self, name: &str) -> Option<SessionId> {
        self.sessions
            .values()
            .find(|s| s.name == name)
            .map(|s| s.id)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Create a new session with one window and one pane. Returns the new ids.
    pub fn new_session(&mut self, name: impl Into<String>, shell: Vec<String>) -> SessionId {
        let sid = SessionId::alloc();
        let wid = WindowId::alloc();
        let pid = PaneId::alloc();
        let win_name = resolve_window_name(String::new(), &shell);
        let pane = Pane::new(pid, shell);
        let window = Window::new(wid, win_name, pane);
        let session = Session::new(sid, name.into(), window);
        self.sessions.insert(sid, session);
        sid
    }

    pub fn kill_session(&mut self, id: SessionId) -> bool {
        let removed = self.sessions.remove(&id).is_some();
        if removed {
            // Drop clients that were viewing it.
            self.clients.retain(|_, c| c.session != id);
        }
        removed
    }

    /// Add a window to a session. Returns the new window id. An empty `name`
    /// defaults to the shell's basename (tmux-style: "powershell", "zsh", …).
    pub fn new_window(
        &mut self,
        sid: SessionId,
        name: impl Into<String>,
        shell: Vec<String>,
    ) -> Option<WindowId> {
        let session = self.sessions.get_mut(&sid)?;
        let wid = WindowId::alloc();
        let pid = PaneId::alloc();
        let name = resolve_window_name(name.into(), &shell);
        let pane = Pane::new(pid, shell);
        let window = Window::new(wid, name, pane);
        session.add_window(window);
        Some(wid)
    }

    /// Split the active pane of a session's active window. Returns new pane id.
    pub fn split_active(
        &mut self,
        sid: SessionId,
        shell: Vec<String>,
        dir: SplitDir,
    ) -> Option<PaneId> {
        let session = self.sessions.get_mut(&sid)?;
        let pid = PaneId::alloc();
        let pane = Pane::new(pid, shell);
        session.active_window_mut().split(pane, dir);
        Some(pid)
    }

    /// Break the active pane of the active window out into a brand-new window
    /// (tmux break-pane, prefix `!`). The pane keeps its id (and grid); a new
    /// window is created to hold it, named after the pane's shell, and becomes
    /// active. No-op (returns None) when the active window has only one pane —
    /// there's nothing to break out, matching tmux. Returns the new window id.
    pub fn break_active_pane(&mut self, sid: SessionId) -> Option<WindowId> {
        let session = self.sessions.get_mut(&sid)?;
        // Don't break a lone pane (would just move an only-child to a new window).
        let pane = session.active_window_mut().take_active_pane()?;
        let wid = WindowId::alloc();
        let name = resolve_window_name(String::new(), &pane.shell);
        let window = Window::new(wid, name, pane);
        session.add_window(window);
        Some(wid)
    }

    /// Swap the active pane with another pane in the SAME window (tmux swap-pane;
    /// here used for prefix `{`/`}` swapping with the previous/next pane). Returns
    /// false if `other` isn't a valid distinct pane in the active window.
    pub fn swap_active_pane(&mut self, sid: SessionId, other: PaneId) -> bool {
        match self.sessions.get_mut(&sid) {
            Some(session) => session.active_window_mut().swap_with_active(other),
            None => false,
        }
    }

    /// The pane that comes before/after the active pane in the active window's
    /// traversal order (wraps), or None for a single-pane window. Used to pick
    /// the swap target for prefix `{` (previous) and `}` (next).
    pub fn sibling_pane(&self, sid: SessionId, next: bool) -> Option<PaneId> {
        let session = self.sessions.get(&sid)?;
        let w = session.window(session.active_window())?;
        let ids = w.pane_ids();
        if ids.len() < 2 {
            return None;
        }
        let pos = ids.iter().position(|&i| i == w.active_pane())?;
        let n = ids.len();
        let idx = if next { (pos + 1) % n } else { (pos + n - 1) % n };
        Some(ids[idx])
    }

    /// Kill a pane anywhere in a session, cascading: emptying a window closes
    /// it; emptying the session closes it. Returns what ultimately happened.
    pub fn kill_pane(&mut self, sid: SessionId, pid: PaneId) -> CascadeResult {
        let Some(session) = self.sessions.get_mut(&sid) else {
            return CascadeResult::NotFound;
        };
        // Find which window holds the pane.
        let Some(wid) = session
            .windows
            .iter()
            .find(|w| w.layout.contains(pid))
            .map(|w| w.id)
        else {
            return CascadeResult::NotFound;
        };
        let window = session.window_mut(wid).unwrap();
        let window_now_empty = window.remove_pane(pid);
        if !window_now_empty {
            return CascadeResult::PaneClosed;
        }
        let session_now_empty = session.remove_window(wid);
        if !session_now_empty {
            return CascadeResult::WindowClosed;
        }
        self.kill_session(sid);
        CascadeResult::SessionClosed
    }

    /// Kill an entire window (tmux `kill-window` / prefix `&`): remove it and all
    /// its panes from `sid`. Returns the panes that were in it (so the caller can
    /// drop their PTYs) and the cascade result — emptying the session closes it.
    /// `WindowClosed` means other windows remain; `SessionClosed` means this was
    /// the last window.
    pub fn kill_window(&mut self, sid: SessionId, wid: WindowId) -> (Vec<PaneId>, CascadeResult) {
        let Some(session) = self.sessions.get_mut(&sid) else {
            return (Vec::new(), CascadeResult::NotFound);
        };
        let Some(window) = session.window(wid) else {
            return (Vec::new(), CascadeResult::NotFound);
        };
        let panes = window.pane_ids();
        let session_now_empty = session.remove_window(wid);
        if !session_now_empty {
            return (panes, CascadeResult::WindowClosed);
        }
        self.kill_session(sid);
        (panes, CascadeResult::SessionClosed)
    }

    // --- client registry ---

    pub fn attach_client(&mut self, session: SessionId, size: PtySize) -> Option<u64> {
        if !self.sessions.contains_key(&session) {
            return None;
        }
        self.next_client_id += 1;
        let id = self.next_client_id;
        self.clients.insert(id, Client { id, session, size });
        Some(id)
    }

    pub fn detach_client(&mut self, id: u64) -> bool {
        self.clients.remove(&id).is_some()
    }

    /// Re-point an attached client at a different session (tmux switch-client).
    /// Returns false if either the client or the target session is unknown.
    pub fn set_client_session(&mut self, id: u64, session: SessionId) -> bool {
        if !self.sessions.contains_key(&session) {
            return false;
        }
        match self.clients.get_mut(&id) {
            Some(c) => {
                c.session = session;
                true
            }
            None => false,
        }
    }

    /// Update an attached client's terminal size (on a client resize). Returns
    /// false if the client is unknown. The session's [`effective_size`] is the
    /// min over clients, so this is what makes a resize actually change the
    /// rendered width (panes *and* the status bar).
    pub fn set_client_size(&mut self, id: u64, size: PtySize) -> bool {
        match self.clients.get_mut(&id) {
            Some(c) => {
                c.size = size;
                true
            }
            None => false,
        }
    }

    pub fn clients_of(&self, session: SessionId) -> Vec<&Client> {
        self.clients
            .values()
            .filter(|c| c.session == session)
            .collect()
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Effective render size for a session: element-wise min over attached
    /// clients ("smallest client wins"). None if no clients are attached.
    pub fn effective_size(&self, session: SessionId) -> Option<PtySize> {
        let mut it = self.clients_of(session).into_iter().peekable();
        it.peek()?;
        let mut cols = u16::MAX;
        let mut rows = u16::MAX;
        for c in it {
            cols = cols.min(c.size.cols);
            rows = rows.min(c.size.rows);
        }
        Some(PtySize { cols, rows })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeResult {
    PaneClosed,
    WindowClosed,
    SessionClosed,
    NotFound,
}

/// Pick a window name: use `name` if non-empty, otherwise derive a tmux-style
/// default from the shell argv's basename — e.g. `C:\…\powershell.exe` →
/// "powershell", `/bin/zsh` → "zsh", `pwsh` → "pwsh". Falls back to "shell" if
/// the argv is empty or yields nothing useful.
pub fn resolve_window_name(name: String, shell: &[String]) -> String {
    if !name.is_empty() {
        return name;
    }
    shell
        .first()
        .map(|argv0| {
            // Split on both separators so Windows paths work regardless of host.
            let base = argv0
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(argv0.as_str());
            // Strip a trailing .exe/.com/.bat/.cmd extension (case-insensitive).
            let stem = base
                .rsplit_once('.')
                .filter(|(_, ext)| {
                    matches!(ext.to_ascii_lowercase().as_str(), "exe" | "com" | "bat" | "cmd")
                })
                .map(|(stem, _)| stem)
                .unwrap_or(base);
            if stem.is_empty() {
                "shell".to_string()
            } else {
                stem.to_string()
            }
        })
        .unwrap_or_else(|| "shell".to_string())
}

#[cfg(test)]
mod tests;
