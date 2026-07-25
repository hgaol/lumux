//! Daemon-side runtime state: the object tree plus the live per-pane emulators
//! and PTY handles. This is the bridge between `lumux_core`'s pure model and the
//! backend's real PTYs.
//!
//! Generic over a [`PtySystem`] so the identical logic runs on the unix backend
//! (dev/CI) and the Windows ConPTY backend (Phase 10).

use std::collections::BTreeMap;
use std::ffi::OsString;

use lumux_core::agent::{AgentClear, AgentIdentity, AgentReport};
use lumux_core::config::Config;
use lumux_core::copymode::CopyMode;
use lumux_core::grid::Grid;
use lumux_core::keymap::{CopyKey, Keymap};
use lumux_core::model::{
    CascadeResult, PaneId, PaneNode, Server, SessionId, SplitDir, Window, WindowId,
};
use lumux_core::render::{compose, ClientRenderer, WindowView};
use lumux_core::traits::{Clipboard, Pty, PtySize, PtySystem, PtyWriter, ShellCommand};

/// Per-pane live state: the emulator grid and the PTY input/control handle.
pub struct LivePane<W: PtyWriter> {
    pub grid: Grid,
    pub writer: W,
    pub dead: bool,
}

/// The pane tree actually projected onto the content plane. A zoomed window
/// retains its split tree for restoration, but only its zoomed leaf is visible
/// and therefore interactive.
fn visible_layout(window: &Window) -> PaneNode {
    match window.zoomed_pane() {
        Some(pane) => PaneNode::leaf(pane),
        None => window.layout.clone(),
    }
}

/// The default shell argv when a client doesn't specify one and the config sets
/// no `default_shell`.
///
/// On Windows we always use Windows PowerShell, resolved to its absolute path
/// under `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`. Using the
/// absolute path (rather than a bare `powershell.exe`) means the spawn never
/// depends on the daemon's PATH, which a detached background process can't be
/// assumed to have. A Unix-style `SHELL` value (e.g. `/bin/bash.exe` from
/// Git-bash) is deliberately ignored on Windows — it isn't a path ConPTY can
/// launch, so honoring it would leave a dead pane.
#[cfg(windows)]
pub fn default_shell() -> Vec<String> {
    vec![powershell_path()]
}

/// Absolute path to Windows PowerShell, with sensible fallbacks. PowerShell ships
/// in System32 on every supported Windows, so the first form effectively always
/// exists; the bare-name fallbacks only matter in unusual stripped environments.
#[cfg(windows)]
fn powershell_path() -> String {
    if let Ok(root) = std::env::var("SystemRoot") {
        let p = std::path::Path::new(&root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if p.is_file() {
            return p.to_string_lossy().into_owned();
        }
    }
    // Fall back to PATH resolution, then COMSPEC (cmd) as a last resort.
    if which_on_path("powershell.exe") {
        "powershell.exe".to_string()
    } else {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
}

/// Best-effort check that an executable is resolvable on PATH (Windows).
#[cfg(windows)]
fn which_on_path(exe: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(exe).is_file())
}

/// The default shell argv when a client doesn't specify one (Unix).
#[cfg(unix)]
pub fn default_shell() -> Vec<String> {
    if let Ok(sh) = std::env::var("SHELL") {
        vec![sh]
    } else {
        vec!["/bin/sh".to_string()]
    }
}

/// Authoritative runtime contract inherited by every pane and detached hook.
///
/// The endpoint comes from the bound [`lumux_core::traits::Listener`], not the
/// daemon process environment. Keeping endpoint and reporter together ensures
/// provider adapters need only one stable pane interface: `LUMUX`,
/// `LUMUX_PANE`, and `LUMUX_BIN`.
#[derive(Clone, Debug)]
pub(crate) struct PaneRuntime {
    endpoint: Option<OsString>,
    reporter: Option<std::path::PathBuf>,
}

impl PaneRuntime {
    pub(crate) fn for_listener(endpoint: Option<OsString>) -> Self {
        Self {
            endpoint: endpoint.filter(|value| !value.is_empty()),
            reporter: std::env::current_exe().ok(),
        }
    }

    fn from_process() -> Self {
        #[cfg(unix)]
        let endpoint = std::env::var_os("LUMUX_SOCK");
        #[cfg(windows)]
        let endpoint = std::env::var_os("LUMUX_PIPE");
        #[cfg(not(any(unix, windows)))]
        let endpoint = std::env::var_os("LUMUX_PIPE").or_else(|| std::env::var_os("LUMUX_SOCK"));
        Self::for_listener(endpoint)
    }

    fn environment(&self, pane: PaneId) -> Vec<(String, String)> {
        let endpoint = self
            .endpoint
            .clone()
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "1".to_string());
        let mut env = vec![
            ("LUMUX".to_string(), endpoint),
            ("LUMUX_PANE".to_string(), pane.to_string()),
        ];
        if let Some(reporter) = self
            .reporter
            .as_ref()
            .filter(|path| path.is_absolute())
            .and_then(|path| path.to_str())
            .filter(|path| !path.is_empty())
        {
            env.push(("LUMUX_BIN".to_string(), reporter.to_string()));
        }
        env
    }
}

/// Latest accepted lifecycle event for a pane. Keeping identity, sequence, and
/// optional visible status together makes a cleared entry a complete tombstone:
/// it can reject both stale reports and delayed clears from replaced owners.
struct AgentLifecycle {
    identity: AgentIdentity,
    sequence: u64,
    status: Option<lumux_core::agent::AgentStatus>,
}

impl AgentLifecycle {
    fn reported(report: AgentReport) -> Self {
        let AgentReport {
            identity,
            state,
            sequence,
            ..
        } = report;
        let status = lumux_core::agent::AgentStatus::new(identity.agent.clone(), state);
        Self {
            identity,
            sequence,
            status: Some(status),
        }
    }

    fn cleared(clear: AgentClear) -> Self {
        let AgentClear {
            identity, sequence, ..
        } = clear;
        Self {
            identity,
            sequence,
            status: None,
        }
    }

    /// Apply a newer report, returning whether its user-visible status changed.
    fn report(&mut self, report: AgentReport) -> bool {
        let AgentReport {
            identity,
            claim,
            state,
            sequence,
            ..
        } = report;
        if sequence <= self.sequence {
            return false;
        }

        let replacement = self.identity != identity;
        let ended_owned_lifecycle = self.status.is_none() && self.identity.owner.is_some();
        if (replacement || ended_owned_lifecycle) && !claim {
            // Ordinary tool/stop/notification hooks can arrive after a newer
            // provider session has claimed the pane, or after their own owned
            // session ended. Reject them without advancing the current
            // lifecycle's sequence. Unowned/manual tombstones retain their
            // legacy ability to resume on a newer report.
            return false;
        }

        let before = self.status.clone();
        self.sequence = sequence;
        if !replacement {
            match self.status.as_mut() {
                Some(status) => status.apply_report(identity.agent.clone(), state),
                None => {
                    self.status = Some(lumux_core::agent::AgentStatus::new(
                        identity.agent.clone(),
                        state,
                    ));
                }
            }
        } else {
            // A replacement lifecycle starts fresh. In particular, an idle
            // report from a new owner must not derive Done from the prior
            // owner's activity.
            self.identity = identity.clone();
            self.status = Some(lumux_core::agent::AgentStatus::new(
                identity.agent.clone(),
                state,
            ));
        }
        self.status != before
    }

    /// Clear only this exact lifecycle. `Option` equality is deliberate:
    /// legacy/manual `None` owners interoperate with one another, but cannot
    /// tear down an explicitly owned provider session (and vice versa).
    fn clear(&mut self, clear: AgentClear) -> bool {
        if self.identity != clear.identity || clear.sequence <= self.sequence {
            return false;
        }
        self.sequence = clear.sequence;
        self.status.take().is_some()
    }
}

/// Owns the object model plus the live panes. One per daemon.
pub struct Daemon<S: PtySystem> {
    pub server: Server,
    pty_system: S,
    pane_runtime: PaneRuntime,
    panes: BTreeMap<PaneId, LivePane<<S::Pty as Pty>::Writer>>,
    /// One keymap per attached client id.
    keymaps: BTreeMap<u64, Keymap>,
    renderers: BTreeMap<u64, ClientRenderer>,
    /// Active copy-mode state per client (absent = not in copy-mode).
    copy: BTreeMap<u64, CopySession>,
    /// Transient status-line message per client (tmux display-message), shown
    /// until the next render-after-input clears it.
    message: BTreeMap<u64, String>,
    /// Clients currently showing the key-binding help overlay (tmux prefix ?).
    help: std::collections::BTreeSet<u64>,
    /// Per-client scroll offset (first visible binding row) for the help overlay.
    help_offset: BTreeMap<u64, usize>,
    /// Clients in the session switcher (choose-tree), with their tree state.
    choosing: BTreeMap<u64, ChooseTree>,
    /// Clients with an open text prompt (rename-window/-session): the target and
    /// the buffer typed so far.
    prompt: BTreeMap<u64, Prompt>,
    /// Clients typing a copy-mode search query (after `/`/`?`): the in-progress
    /// query and the direction to search when confirmed.
    search: BTreeMap<u64, SearchInput>,
    /// Clients mid-divider-drag: the path to the divider grabbed on mouse-press,
    /// so subsequent drag motion moves that same divider (even off its line).
    dragging: BTreeMap<u64, DividerDrag>,
    /// Clients mid mouse-drag text-selection (tmux drag-to-copy). Armed on a
    /// left-press over a selectable pane; promoted to Dragging on the first drag
    /// motion, which enters copy-mode and starts the selection.
    mouse_sel: BTreeMap<u64, MouseSel>,
    /// Server-global paste buffers (tmux paste-buffer stack), shared by all
    /// sessions and clients. Yanks push here; prefix `]`/`=` read from it.
    buffers: lumux_core::buffers::PasteBuffers,
    /// Per-client highlighted index in the open paste-buffer chooser.
    choosing_buffer: BTreeMap<u64, usize>,
    /// Clients showing the display-panes number overlay (tmux prefix q).
    showing_panes: std::collections::BTreeSet<u64>,
    /// Clients showing the big-digit clock overlay (tmux `clock-mode`, prefix
    /// `t`); any key closes it.
    clock: std::collections::BTreeSet<u64>,
    /// The server-global marked pane (tmux `select-pane -m` / prefix `m`), if
    /// any. join-pane / swap-pane with no explicit source default to it, so a
    /// pane can be marked in one window and pulled/swapped into another.
    marked_pane: Option<(SessionId, PaneId)>,
    /// Latest lifecycle event per pane. Cleared entries remain as tombstones so
    /// independently-running hook processes cannot resurrect or remove a newer
    /// provider session.
    agent_lifecycles: BTreeMap<PaneId, AgentLifecycle>,
    /// Agents found by inspecting each pane's descendant processes, so an agent
    /// appears the moment it launches instead of waiting for its first hook.
    /// Kept separate from `agent_lifecycles` on purpose: hook reports own the
    /// state machine (ownership, sequencing, tombstones), while this is only a
    /// presence fallback consulted when no hook status exists.
    detected_agents: BTreeMap<PaneId, lumux_core::agent::AgentStatus>,
    /// Frame counter for the working-agent spinner, advanced on the daemon tick.
    spinner_tick: u64,
    /// Open right-click context menu per client (absent = none).
    menus: BTreeMap<u64, ContextMenu>,
    /// Per-session sidebar visibility override (tmux-style `:set sidebar on`).
    /// Absent = fall back to the config default. Session-global by design: under
    /// the shared PTY, one client's toggle reflows every client of the session.
    sidebar_on: BTreeMap<SessionId, bool>,
    /// Per-session sidebar collapse state. When shown, the sidebar can be either
    /// expanded (full width) or collapsed to a thin clickable rail. Absent =
    /// expanded. Independent of `sidebar_on` (fully off).
    sidebar_collapsed: BTreeMap<SessionId, bool>,
    /// Independent sidebar viewports per interactive client. Visibility and
    /// collapse alter shared PTY geometry and are therefore session-owned, but
    /// scrolling is purely presentational: one observer must not move another
    /// observer's session or agent list.
    sidebar_scroll: BTreeMap<u64, SidebarScroll>,
    clipboard: Box<dyn Clipboard>,
    config: Config,
}

/// Mouse text-selection drag state (tmux drag-to-copy). A left-press over a
/// selectable pane records `Armed` with the press cell; the first drag motion
/// promotes it to `Dragging`, entering copy-mode and starting the selection.
#[derive(Clone, Copy)]
enum MouseSel {
    /// Pressed but not yet moved; remembers where the drag would start.
    Armed {
        session: SessionId,
        window: WindowId,
        pane: PaneId,
        origin_col: u16,
        origin_row: u16,
        /// Copy viewport represented by the press frame. `None` means the
        /// press occurred in live mode, before the drag creates CopySession.
        origin_top: Option<usize>,
    },
    /// Drag in progress: copy-mode is open and the selection is being extended.
    Dragging {
        session: SessionId,
        window: WindowId,
        pane: PaneId,
    },
}

#[derive(Clone)]
struct DividerDrag {
    session: SessionId,
    window: WindowId,
    viewport: lumux_core::layout::Rect,
    layout: PaneNode,
    path: Vec<bool>,
}

/// Copy-mode state is meaningful only for the exact pane buffer where it was
/// entered. Packing target identity with the cursor/selection prevents every
/// navigation/yank caller from independently re-resolving the live active pane.
struct CopySession {
    session: SessionId,
    window: WindowId,
    pane: PaneId,
    mode: CopyMode,
}

impl std::ops::Deref for CopySession {
    type Target = CopyMode;

    fn deref(&self) -> &Self::Target {
        &self.mode
    }
}

impl std::ops::DerefMut for CopySession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.mode
    }
}

/// Side effects of feeding PTY output into a pane, returned by
/// [`Daemon::feed_pane`] for the event loop to act on.
#[derive(Default)]
pub struct PaneFeed {
    /// The output rang the terminal bell (BEL).
    pub bell: bool,
    /// Text the app copied to the clipboard via OSC 52, to forward to clients.
    pub clipboard: Option<String>,
}

/// An open rename prompt: what it renames, plus the in-progress text.
#[derive(Clone)]
pub struct Prompt {
    pub target: PromptTarget,
    pub buffer: String,
}

/// An open copy-mode search input: the query typed so far and which way to look.
#[derive(Clone)]
pub struct SearchInput {
    pub buffer: String,
    pub dir: lumux_core::copymode::SearchDir,
}

/// What an open prompt does when confirmed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptTarget {
    Window,
    Session,
    /// Search window names for the typed query and switch to the first match
    /// (tmux find-window). Seeded empty rather than with the current name.
    FindWindow,
    /// The tmux command-prompt (prefix `:`): the typed line is parsed and
    /// dispatched by the event loop. Seeded empty.
    Command,
}

/// Navigation/dispatch intent produced when a prompt is confirmed. The daemon
/// owns prompt editing and lookup, while the event loop applies focus through
/// the shared acknowledgement lifecycle and dispatches command lines.
pub enum PromptOutcome {
    FocusWindow(WindowId),
    Command(String),
    RenameSession(String),
    RenameWindow { window: WindowId, name: String },
}

/// State of an open choose-tree (session switcher, prefix `s`): which sessions
/// are expanded to reveal their windows, and where the cursor sits in the
/// flattened list of visible rows.
#[derive(Clone, Default)]
pub struct ChooseTree {
    /// Sessions currently expanded to show their windows.
    expanded: std::collections::BTreeSet<SessionId>,
    /// Index into the flattened visible-row list (see [`Daemon::tree_rows`]).
    cursor: usize,
}

/// One visible row in the choose-tree: a session, or a window nested under one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TreeRow {
    Session(SessionId),
    Window(SessionId, WindowId),
}

/// What the choose-tree selected when confirmed (Enter).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChooserPick {
    /// Switch to this session (its active window).
    Session(SessionId),
    /// Switch to this session AND focus this specific window (tmux choose-tree).
    Window(SessionId, WindowId),
}

/// Exact navigation target produced by the persistent sidebar. Agent entries
/// retain their pane id so clicking can focus and acknowledge that pane, not
/// merely its containing window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarPick {
    Session(SessionId),
    Agent {
        session: SessionId,
        window: WindowId,
        pane: PaneId,
    },
}

/// Presentation-only offsets for one client's two fixed-height sidebar
/// sections. Both offsets share one lifecycle: create lazily, retain across
/// session switches, and discard when the client detaches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SidebarScroll {
    sessions: usize,
    agents: usize,
}

impl SidebarScroll {
    fn offset_mut(&mut self, section: SidebarSectionKind) -> &mut usize {
        match section {
            SidebarSectionKind::Sessions => &mut self.sessions,
            SidebarSectionKind::Agents => &mut self.agents,
        }
    }
}

/// Stable identity of one of the sidebar's independently scrollable sections.
/// Keeping this typed prevents screen-row routing from being represented as an
/// ad-hoc boolean at each caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarSectionKind {
    Sessions,
    Agents,
}

impl SidebarSectionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Sessions => "SESSIONS",
            Self::Agents => "AGENTS",
        }
    }
}

/// One session row in the sidebar's complete (not yet clipped) projection.
struct SidebarSessionEntry {
    sid: SessionId,
    name: String,
    windows: usize,
    current: bool,
}

/// One agent row in the sidebar's complete (not yet clipped) projection.
struct SidebarAgentEntry {
    sid: SessionId,
    wid: WindowId,
    pid: PaneId,
    agent: String,
    state: lumux_core::agent::AgentState,
    session_name: String,
}

/// Geometry, content, and clamped viewport for one sidebar section. The header
/// owns the first row; `capacity` is therefore the remaining body height.
struct SidebarSection<T> {
    start_row: usize,
    height: usize,
    offset: usize,
    items: Vec<T>,
}

impl<T> SidebarSection<T> {
    fn new(start_row: usize, height: usize, requested_offset: usize, items: Vec<T>) -> Self {
        let capacity = height.saturating_sub(1);
        let offset = requested_offset.min(items.len().saturating_sub(capacity));
        Self {
            start_row,
            height,
            offset,
            items,
        }
    }

    fn contains(&self, row: usize) -> bool {
        row >= self.start_row && row < self.start_row.saturating_add(self.height)
    }

    fn capacity(&self) -> usize {
        self.height.saturating_sub(1)
    }

    fn max_offset(&self) -> usize {
        self.items.len().saturating_sub(self.capacity())
    }

    fn scrolled_offset(&self, up: bool, step: usize) -> usize {
        if up {
            self.offset.saturating_sub(step)
        } else {
            self.offset.saturating_add(step).min(self.max_offset())
        }
    }

    fn offset_revealing(&self, index: usize) -> Option<usize> {
        let capacity = self.capacity();
        if capacity == 0 || index >= self.items.len() {
            return None;
        }
        let next = if index < self.offset {
            index
        } else if index >= self.offset.saturating_add(capacity) {
            index.saturating_add(1).saturating_sub(capacity)
        } else {
            self.offset
        };
        Some(next.min(self.max_offset()))
    }

    fn visible_items(&self) -> impl Iterator<Item = (usize, &T)> {
        let first_row = self.start_row.saturating_add(1);
        self.items
            .iter()
            .skip(self.offset)
            .take(self.capacity())
            .enumerate()
            .map(move |(index, item)| (first_row.saturating_add(index), item))
    }

    fn item_at(&self, row: usize) -> Option<&T> {
        if !self.contains(row) || row == self.start_row {
            return None;
        }
        let visible_index = row.saturating_sub(self.start_row.saturating_add(1));
        if visible_index >= self.capacity() {
            return None;
        }
        self.items.get(self.offset.saturating_add(visible_index))
    }
}

/// A borrowed row exposed by [`SidebarProjection`]. Rendering and hit-testing
/// consume this same view, while scrolling and ensure-visible use the section
/// metadata that produced it.
enum SidebarProjectedRow<'a> {
    Header(&'static str),
    Session(&'a SidebarSessionEntry),
    Agent(&'a SidebarAgentEntry),
}

/// Complete sidebar projection for one client, outer height, and current
/// session. It is the sole authority for section boundaries, capacities,
/// enumeration order, clamped offsets, visible rows, and row hit-testing.
struct SidebarProjection {
    sessions: SidebarSection<SidebarSessionEntry>,
    agents: SidebarSection<SidebarAgentEntry>,
}

impl SidebarProjection {
    fn new(
        content_height: usize,
        scroll: SidebarScroll,
        sessions: Vec<SidebarSessionEntry>,
        agents: Vec<SidebarAgentEntry>,
    ) -> Self {
        // With an odd content height the sessions section receives the extra
        // row, preserving the original 1:1 split.
        let agents_start = content_height.div_ceil(2);
        Self {
            sessions: SidebarSection::new(0, agents_start, scroll.sessions, sessions),
            agents: SidebarSection::new(
                agents_start,
                content_height.saturating_sub(agents_start),
                scroll.agents,
                agents,
            ),
        }
    }

    fn section_at(&self, row: usize) -> Option<SidebarSectionKind> {
        if self.sessions.contains(row) {
            Some(SidebarSectionKind::Sessions)
        } else if self.agents.contains(row) {
            Some(SidebarSectionKind::Agents)
        } else {
            None
        }
    }

    fn current_offset(&self, section: SidebarSectionKind) -> usize {
        match section {
            SidebarSectionKind::Sessions => self.sessions.offset,
            SidebarSectionKind::Agents => self.agents.offset,
        }
    }

    fn scrolled_offset(&self, section: SidebarSectionKind, up: bool, step: usize) -> usize {
        match section {
            SidebarSectionKind::Sessions => self.sessions.scrolled_offset(up, step),
            SidebarSectionKind::Agents => self.agents.scrolled_offset(up, step),
        }
    }

    fn session_offset_revealing(&self, session: SessionId) -> Option<usize> {
        let index = self
            .sessions
            .items
            .iter()
            .position(|entry| entry.sid == session)?;
        self.sessions.offset_revealing(index)
    }

    fn rows(&self) -> Vec<(usize, SidebarProjectedRow<'_>)> {
        let mut rows = Vec::with_capacity(
            self.sessions
                .capacity()
                .saturating_add(self.agents.capacity())
                + 2,
        );
        if self.sessions.height > 0 {
            rows.push((
                self.sessions.start_row,
                SidebarProjectedRow::Header(SidebarSectionKind::Sessions.label()),
            ));
            rows.extend(
                self.sessions
                    .visible_items()
                    .map(|(row, entry)| (row, SidebarProjectedRow::Session(entry))),
            );
        }
        if self.agents.height > 0 {
            rows.push((
                self.agents.start_row,
                SidebarProjectedRow::Header(SidebarSectionKind::Agents.label()),
            ));
            rows.extend(
                self.agents
                    .visible_items()
                    .map(|(row, entry)| (row, SidebarProjectedRow::Agent(entry))),
            );
        }
        rows
    }

    fn row_at(&self, row: usize) -> Option<SidebarProjectedRow<'_>> {
        match self.section_at(row)? {
            SidebarSectionKind::Sessions if row == self.sessions.start_row => Some(
                SidebarProjectedRow::Header(SidebarSectionKind::Sessions.label()),
            ),
            SidebarSectionKind::Sessions => {
                self.sessions.item_at(row).map(SidebarProjectedRow::Session)
            }
            SidebarSectionKind::Agents if row == self.agents.start_row => Some(
                SidebarProjectedRow::Header(SidebarSectionKind::Agents.label()),
            ),
            SidebarSectionKind::Agents => self.agents.item_at(row).map(SidebarProjectedRow::Agent),
        }
    }

    fn normalized_scroll(&self) -> SidebarScroll {
        SidebarScroll {
            sessions: self.sessions.offset,
            agents: self.agents.offset,
        }
    }
}

/// Immutable sidebar interaction map captured alongside one composed terminal
/// frame. It contains stable model identities, not labels, so a later list
/// mutation cannot retarget a click to whichever row happens to occupy the same
/// coordinates when the input reaches the control loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SidebarFrame {
    session: SessionId,
    width: u16,
    content_height: usize,
    collapsed: bool,
    picks: Vec<Option<SidebarPick>>,
    sections: Vec<Option<SidebarSectionKind>>,
}

/// One pane exactly as it was projected in a composed frame. Coordinates and
/// mouse-mode flags belong to that frame; stable ids are revalidated before any
/// later input mutates the live model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InteractionPane {
    session: SessionId,
    window: WindowId,
    pane: PaneId,
    rect: lumux_core::layout::Rect,
    wants_mouse: bool,
    alt_screen: bool,
}

impl InteractionPane {
    pub(crate) fn session(self) -> SessionId {
        self.session
    }

    pub(crate) fn window(self) -> WindowId {
        self.window
    }

    pub(crate) fn pane(self) -> PaneId {
        self.pane
    }

    pub(crate) fn rect(self) -> lumux_core::layout::Rect {
        self.rect
    }

    pub(crate) fn wants_mouse(self) -> bool {
        self.wants_mouse
    }

    pub(crate) fn alt_screen(self) -> bool {
        self.alt_screen
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatusHit {
    start: u16,
    end: u16,
    window: WindowId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StatusFrame {
    row: u16,
    hits: Vec<StatusHit>,
}

/// Stable identity of a divider captured from one rendered layout. Paths alone
/// are positional, so the complete visible tree and window id travel with the
/// path and are compared to live topology before a drag can begin.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InteractionDivider {
    session: SessionId,
    window: WindowId,
    viewport: lumux_core::layout::Rect,
    layout: PaneNode,
    path: Vec<bool>,
}

/// Immutable hit map for one composed client frame. This is the sole interface
/// used by epoch-tagged input; legacy untagged input deliberately keeps live
/// hit-testing for backwards compatibility.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InteractionMap {
    session: SessionId,
    size: PtySize,
    modal: bool,
    copy_mode: bool,
    copy_pane: Option<PaneId>,
    /// Combined-buffer row represented at the top of this copy-mode frame.
    copy_top: Option<usize>,
    sidebar: Option<SidebarFrame>,
    window: Option<WindowId>,
    viewport: Option<lumux_core::layout::Rect>,
    layout: Option<PaneNode>,
    panes: Vec<InteractionPane>,
    status: StatusFrame,
    /// Geometry of the open context menu, so a click resolves against exactly
    /// the popup the user is looking at.
    menu: Option<MenuFrame>,
}

/// The item rows of an open context menu, captured with the frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuFrame {
    origin: (u16, u16),
    width: u16,
    items: Vec<MenuAction>,
}

impl MenuFrame {
    fn height(&self) -> u16 {
        self.items.len() as u16 + 2
    }

    pub(crate) fn contains(&self, col: u16, row: u16) -> bool {
        let (x, y) = self.origin;
        col >= x && col < x + self.width && row >= y && row < y + self.height()
    }

    pub(crate) fn item_at(&self, col: u16, row: u16) -> Option<MenuAction> {
        let (x, y) = self.origin;
        if col < x || col >= x + self.width || row <= y || row >= y + self.height() - 1 {
            return None;
        }
        self.items.get((row - y - 1) as usize).copied()
    }
}

impl InteractionMap {
    pub(crate) fn session(&self) -> SessionId {
        self.session
    }

    pub(crate) fn size(&self) -> PtySize {
        self.size
    }

    pub(crate) fn is_modal(&self) -> bool {
        self.modal
    }

    pub(crate) fn is_copy_mode(&self) -> bool {
        self.copy_mode
    }

    pub(crate) fn copy_pane(&self) -> Option<InteractionPane> {
        self.pane(self.copy_pane?)
    }

    /// The copy-buffer row represented at the top of this exact pane target.
    /// Keeping the identity check here prevents a historical viewport offset
    /// from being applied to a different pane that later occupies the cells.
    fn copy_top_for(&self, target: InteractionPane) -> Option<usize> {
        let copy_target = self.copy_pane()?;
        ((copy_target.session, copy_target.window, copy_target.pane)
            == (target.session, target.window, target.pane))
            .then_some(self.copy_top?)
    }

    pub(crate) fn menu(&self) -> Option<&MenuFrame> {
        self.menu.as_ref()
    }

    pub(crate) fn sidebar_click(&self, col: u16, row: u16) -> Option<SidebarClick> {
        self.sidebar.as_ref()?.click_at(col, row)
    }

    pub(crate) fn sidebar(&self) -> Option<&SidebarFrame> {
        self.sidebar.as_ref()
    }

    pub(crate) fn status_window_at(&self, col: u16, row: u16) -> Option<WindowId> {
        if row != self.status.row {
            return None;
        }
        self.status
            .hits
            .iter()
            .find(|hit| col >= hit.start && col < hit.end)
            .map(|hit| hit.window)
    }

    pub(crate) fn on_status_row(&self, row: u16) -> bool {
        row == self.status.row
    }

    pub(crate) fn pane_at(&self, col: u16, row: u16) -> Option<InteractionPane> {
        self.panes
            .iter()
            .copied()
            .find(|pane| pane.rect.contains_point(col, row))
    }

    pub(crate) fn pane(&self, pane: PaneId) -> Option<InteractionPane> {
        self.panes
            .iter()
            .copied()
            .find(|target| target.pane == pane)
    }

    pub(crate) fn divider_at(&self, col: u16, row: u16) -> Option<InteractionDivider> {
        let (Some(window), Some(viewport), Some(layout)) =
            (self.window, self.viewport, self.layout.as_ref())
        else {
            return None;
        };
        let path = lumux_core::layout::divider_at(layout, col, row, viewport)?;
        Some(InteractionDivider {
            session: self.session,
            window,
            viewport,
            layout: layout.clone(),
            path,
        })
    }
}

pub(crate) struct RenderedClientFrame {
    pub(crate) bytes: String,
    pub(crate) interactions: InteractionMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarClick {
    Toggle { collapsed: bool },
    /// The `+` button on the SESSIONS header: create and switch to a session.
    NewSession,
    Pick(SidebarPick),
    Chrome,
}

/// What a right-click landed on, and therefore which operations the context
/// menu offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuTarget {
    Session(SessionId),
    Window(SessionId, WindowId),
    Pane(SessionId, WindowId, PaneId),
}

/// One operation offered by the context menu. Kept as data (rather than a
/// closure) so the render, the hit-test, and the executor all agree on the
/// exact item list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    RenameSession,
    NewWindow,
    KillSession,
    RenameWindow,
    CloseWindow,
    SplitHorizontal,
    SplitVertical,
    ZoomPane,
    ClosePane,
}

impl MenuAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            MenuAction::RenameSession => "Rename session",
            MenuAction::NewWindow => "New window",
            MenuAction::KillSession => "Kill session",
            MenuAction::RenameWindow => "Rename window",
            MenuAction::CloseWindow => "Close window",
            MenuAction::SplitHorizontal => "Split left/right",
            MenuAction::SplitVertical => "Split top/bottom",
            MenuAction::ZoomPane => "Zoom pane",
            MenuAction::ClosePane => "Close pane",
        }
    }
}

/// An open context menu for one client.
pub(crate) struct ContextMenu {
    pub(crate) target: MenuTarget,
    pub(crate) items: Vec<MenuAction>,
    /// Top-left cell of the popup, already clamped to the screen.
    pub(crate) origin: (u16, u16),
    /// Item under the pointer, highlighted so the menu responds to hover.
    pub(crate) hover: Option<usize>,
}

impl ContextMenu {
    fn items_for(target: MenuTarget) -> Vec<MenuAction> {
        match target {
            MenuTarget::Session(_) => vec![
                MenuAction::RenameSession,
                MenuAction::NewWindow,
                MenuAction::KillSession,
            ],
            MenuTarget::Window(..) => vec![MenuAction::RenameWindow, MenuAction::CloseWindow],
            MenuTarget::Pane(..) => vec![
                MenuAction::SplitHorizontal,
                MenuAction::SplitVertical,
                MenuAction::ZoomPane,
                MenuAction::ClosePane,
            ],
        }
    }

    /// Popup width: the widest label plus the border and padding.
    pub(crate) fn width(&self) -> u16 {
        let widest = self
            .items
            .iter()
            .map(|item| item.label().chars().count())
            .max()
            .unwrap_or(0);
        (widest + 4) as u16
    }

    /// Popup height: one row per item plus the top and bottom border.
    pub(crate) fn height(&self) -> u16 {
        self.items.len() as u16 + 2
    }

    /// The item at a screen row, if the point is inside the popup's item area.
    pub(crate) fn item_at(&self, col: u16, row: u16) -> Option<MenuAction> {
        let (x, y) = self.origin;
        if col < x || col >= x + self.width() || row <= y || row >= y + self.height() - 1 {
            return None;
        }
        self.items.get((row - y - 1) as usize).copied()
    }

    pub(crate) fn contains(&self, col: u16, row: u16) -> bool {
        let (x, y) = self.origin;
        col >= x && col < x + self.width() && row >= y && row < y + self.height()
    }
}

/// Braille spinner frames for a working agent, advanced once per daemon tick.
const AGENT_SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Columns the header must have before the `+` new-session button is drawn: the
/// button, a gap, and the collapse button. Below this the collapse button wins,
/// since collapsing is the more essential control.
const NEW_SESSION_BUTTON_SPAN: usize = 3;

impl SidebarFrame {
    fn new(
        session: SessionId,
        width: u16,
        content_height: usize,
        collapsed: bool,
        projection: &SidebarProjection,
    ) -> Self {
        let mut picks = vec![None; content_height];
        let mut sections = vec![None; content_height];
        for row in 0..content_height {
            sections[row] = projection.section_at(row);
            picks[row] = match projection.row_at(row) {
                Some(SidebarProjectedRow::Session(entry)) => Some(SidebarPick::Session(entry.sid)),
                Some(SidebarProjectedRow::Agent(entry)) => Some(SidebarPick::Agent {
                    session: entry.sid,
                    window: entry.wid,
                    pane: entry.pid,
                }),
                Some(SidebarProjectedRow::Header(_)) | None => None,
            };
        }
        Self {
            session,
            width,
            content_height,
            collapsed,
            picks,
            sections,
        }
    }

    pub(crate) fn contains(&self, col: u16, row: u16) -> bool {
        col < self.width && (row as usize) < self.content_height
    }

    pub(crate) fn click_at(&self, col: u16, row: u16) -> Option<SidebarClick> {
        if !self.contains(col, row) {
            return None;
        }
        let toggle = if self.collapsed || self.width == 1 {
            Some(false)
        } else {
            let text_width = self.width.saturating_sub(1);
            (row == 0 && text_width >= 1 && col == text_width - 1).then_some(true)
        };
        if let Some(collapsed) = toggle {
            return Some(SidebarClick::Toggle { collapsed });
        }
        if !self.collapsed && self.width > 1 {
            let text_width = self.width.saturating_sub(1) as usize;
            if row == 0 && text_width >= NEW_SESSION_BUTTON_SPAN && col as usize == text_width - 3 {
                return Some(SidebarClick::NewSession);
            }
        }
        Some(
            self.picks
                .get(row as usize)
                .copied()
                .flatten()
                .map(SidebarClick::Pick)
                .unwrap_or(SidebarClick::Chrome),
        )
    }

    pub(crate) fn section_at(&self, col: u16, row: u16) -> Option<SidebarSectionKind> {
        if !self.contains(col, row) || self.collapsed {
            return None;
        }
        self.sections.get(row as usize).copied().flatten()
    }
}

impl<S: PtySystem> Daemon<S> {
    pub fn new(pty_system: S) -> Self {
        Self::with_clipboard_and_runtime(
            pty_system,
            Box::new(NullClipboard),
            PaneRuntime::from_process(),
        )
    }

    pub fn with_clipboard(pty_system: S, clipboard: Box<dyn Clipboard>) -> Self {
        Self::with_clipboard_and_runtime(pty_system, clipboard, PaneRuntime::from_process())
    }

    pub(crate) fn with_pane_runtime(pty_system: S, pane_runtime: PaneRuntime) -> Self {
        Self::with_clipboard_and_runtime(pty_system, Box::new(NullClipboard), pane_runtime)
    }

    fn with_clipboard_and_runtime(
        pty_system: S,
        clipboard: Box<dyn Clipboard>,
        pane_runtime: PaneRuntime,
    ) -> Self {
        Self {
            server: Server::new(),
            pty_system,
            pane_runtime,
            panes: BTreeMap::new(),
            keymaps: BTreeMap::new(),
            renderers: BTreeMap::new(),
            copy: BTreeMap::new(),
            message: BTreeMap::new(),
            help: std::collections::BTreeSet::new(),
            help_offset: BTreeMap::new(),
            choosing: BTreeMap::new(),
            prompt: BTreeMap::new(),
            search: BTreeMap::new(),
            dragging: BTreeMap::new(),
            mouse_sel: BTreeMap::new(),
            buffers: lumux_core::buffers::PasteBuffers::new(),
            choosing_buffer: BTreeMap::new(),
            showing_panes: std::collections::BTreeSet::new(),
            clock: std::collections::BTreeSet::new(),
            marked_pane: None,
            agent_lifecycles: BTreeMap::new(),
            detected_agents: BTreeMap::new(),
            spinner_tick: 0,
            menus: BTreeMap::new(),
            sidebar_on: BTreeMap::new(),
            sidebar_collapsed: BTreeMap::new(),
            sidebar_scroll: BTreeMap::new(),
            clipboard,
            config: Config::default(),
        }
    }

    /// Replace the active config. Existing clients' keymaps are rebuilt from it
    /// so a `source-file` reload takes effect live; scrollback/shell changes
    /// apply to subsequently spawned panes.
    pub fn set_config(&mut self, config: Config) {
        if let Ok(bindings) = config.to_bindings() {
            // Rebuild every client's keymap from the new bindings, then re-apply
            // the copy-mode key style — otherwise a reload (or a runtime `:set
            // mode-keys emacs`) would silently reset every attached client back
            // to the default vi keys, since Keymap::new starts from vi.
            let emacs = config.mode_keys.eq_ignore_ascii_case("emacs");
            for km in self.keymaps.values_mut() {
                *km = Keymap::new(bindings.clone());
                if emacs {
                    km.set_mode_keys(lumux_core::keymap::ModeKeys::Emacs);
                }
            }
        }
        self.config = config;
    }

    /// Apply a single `set-option` name/value at runtime (tmux `:set`), reusing
    /// the same [`Config::set_option`] mapping the config-file loader uses, then
    /// re-applying the new config live (rebuilds keymaps for a changed prefix /
    /// mode-keys; later renders pick up colors, formats, and base-index). Returns
    /// `Err(message)` for an unknown option so the caller can flash it. The
    /// caller is responsible for any client-terminal side effect a specific
    /// option needs (e.g. pushing mouse enable/disable when `mouse` flips).
    pub fn set_option(&mut self, option: &str, value: &str) -> Result<(), String> {
        let mut config = self.config.clone();
        config.set_option(option, value)?;
        self.set_config(config);
        Ok(())
    }

    /// Scrollback lines from config.
    fn scrollback_lines(&self) -> usize {
        self.config.scrollback
    }

    /// Whether mouse reporting is enabled in the active config.
    pub fn mouse_enabled(&self) -> bool {
        self.config.mouse
    }

    /// Border attributes for the active pane (tmux pane-active-border-style),
    /// built from `pane_active_border_fg`. None when the config disables it
    /// (empty string), so no highlight is drawn.
    fn active_border_attrs(&self) -> Option<lumux_core::render::CellAttributes> {
        lumux_core::render::border_attrs(&self.config.pane_active_border_fg)
    }

    /// Attributes for inactive pane borders (tmux pane-border-style). None when
    /// unconfigured, so borders keep the terminal default.
    fn inactive_border_attrs(&self) -> Option<lumux_core::render::CellAttributes> {
        lumux_core::render::border_attrs(&self.config.pane_border_fg)
    }

    /// Lowest window/pane number shown to the user (tmux base-index). Window
    /// selection keys/commands are offset by this so the digit a user presses
    /// matches the number rendered in the status bar.
    pub fn base_index(&self) -> u32 {
        self.config.base_index
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
    ///
    /// `size` is the full client viewport; the bottom row is reserved for the
    /// status bar, so the PTY/grid is sized to `rows - 1`. Without this the shell
    /// believes it has the full height and writes its last line onto the status
    /// row. Multi-pane windows are then refined to exact per-rect sizes by the
    /// caller via [`Self::resize_session`].
    fn spawn_pane(
        &mut self,
        id: PaneId,
        shell: &[String],
        size: PtySize,
        cwd: Option<String>,
    ) -> std::io::Result<<S::Pty as Pty>::Reader> {
        let content = PtySize::new(size.cols, size.rows.saturating_sub(1).max(1));
        let cmd = ShellCommand {
            argv: shell.to_vec(),
            cwd,
            env: self.pane_runtime.environment(id),
        };
        let pty = self.pty_system.spawn(&cmd, content)?;
        let (writer, reader) = pty.split()?;
        let grid = Grid::new(
            content.cols as usize,
            content.rows as usize,
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

    /// Capture the structure of all sessions into a serializable snapshot (tmux-
    /// resurrect save). Records, per pane, its shell argv and current working
    /// directory so the shell can be relaunched on restore. The live pane ids are
    /// remapped to dense snapshot-local ids so the file is independent of the
    /// daemon's id counter.
    pub fn snapshot(&self) -> lumux_core::persist::StateFile {
        use lumux_core::persist::{PaneSnap, SessionSnap, StateFile, WindowSnap};
        let mut sessions = Vec::new();
        for sid in self.server.session_ids() {
            let Some(s) = self.server.session(sid) else {
                continue;
            };
            let win_ids = s.window_ids();
            let active_window = win_ids
                .iter()
                .position(|&w| w == s.active_window())
                .unwrap_or(0);
            let mut windows = Vec::new();
            for &wid in &win_ids {
                let Some(w) = s.window(wid) else { continue };
                let pane_ids = w.pane_ids();
                // Map each real PaneId -> dense snapshot id (its position).
                let id_of =
                    |pid: PaneId| pane_ids.iter().position(|&p| p == pid).unwrap_or(0) as u32;
                let active_pane = pane_ids
                    .iter()
                    .position(|&p| p == w.active_pane())
                    .unwrap_or(0);
                let panes = pane_ids
                    .iter()
                    .map(|&pid| PaneSnap {
                        layout_id: id_of(pid),
                        shell: w.pane(pid).map(|p| p.shell.clone()).unwrap_or_default(),
                        cwd: self.pane_cwd(pid),
                    })
                    .collect();
                // Clone the layout tree with leaf ids remapped to snapshot ids.
                let layout = remap_layout(&w.layout, &id_of);
                windows.push(WindowSnap {
                    name: w.name.clone(),
                    layout,
                    panes,
                    active_pane,
                    synchronized: w.is_synchronized(),
                    auto_rename: w.auto_rename(),
                });
            }
            sessions.push(SessionSnap {
                name: s.name.clone(),
                windows,
                active_window,
            });
        }
        StateFile::new(sessions)
    }

    /// Save the current snapshot to `path` atomically (write a temp file then
    /// rename, so a crash mid-write can't corrupt the state file). Returns Ok on
    /// success. An empty snapshot still writes (so a deliberately-emptied state
    /// is honored on restart).
    pub fn save_state(&self, path: &std::path::Path) -> std::io::Result<()> {
        let bytes = self
            .snapshot()
            .encode()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Rebuild one session from a snapshot (tmux-resurrect restore): construct the
    /// model via [`Server::restore_session`], then spawn a PTY for every pane in
    /// its saved working directory. Returns each pane's id + reader so the event
    /// loop can start pumping it. Panes whose PTY fails to spawn are skipped
    /// (logged), leaving the rest of the session intact.
    // The nested (id, reader) vec is inherent to the generic PTY type; factoring
    // a generic alias would add noise without aiding readability.
    #[allow(clippy::type_complexity)]
    pub fn restore_session(
        &mut self,
        snap: &lumux_core::persist::SessionSnap,
        size: PtySize,
    ) -> Option<(SessionId, Vec<(PaneId, <S::Pty as Pty>::Reader)>)> {
        let (sid, spawns) = self.server.restore_session(snap)?;
        let mut readers = Vec::new();
        for sp in spawns {
            match self.spawn_pane(sp.id, &sp.shell, size, sp.cwd) {
                Ok(reader) => readers.push((sp.id, reader)),
                Err(e) => tracing::warn!("restore: failed to spawn pane {:?}: {e}", sp.id),
            }
        }
        Some((sid, readers))
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

    /// Feed PTY output bytes into a pane's emulator. Returns a [`PaneFeed`] with
    /// any side effects the caller must act on: the bell flag (to notify clients)
    /// and any OSC 52 clipboard text the app copied (to forward to clients).
    ///
    /// Also drains any terminal-query replies the emulator produced (cursor
    /// position / device status / device attributes) and writes them straight
    /// back to this pane's PTY — i.e. to the shell's stdin — so line editors like
    /// PSReadLine that query the cursor get their answer.
    pub fn feed_pane(&mut self, id: PaneId, bytes: &[u8]) -> PaneFeed {
        if let Some(p) = self.panes.get_mut(&id) {
            // Debug aid: append raw PTY bytes to a capture file so we can inspect
            // exactly what ConPTY emits (e.g. PSReadLine ListView redraws). The
            // target is a FIXED path checked live every feed — not an env var —
            // so an already-running (detached) daemon starts capturing the moment
            // the file's directory marker exists, with no restart needed. Capture
            // is active whenever the sentinel `<config-dir>/lumux-capture.on`
            // exists; bytes go to `<config-dir>/lumux-capture.bin`. Off (zero cost
            // beyond one stat) when the sentinel is absent.
            Self::maybe_capture(bytes);
            p.grid.feed(bytes);
            let responses = p.grid.take_responses();
            if !responses.is_empty() {
                let _ = p.writer.write_input(&responses);
            }
            let bell = p.grid.take_bell();
            let clipboard = p.grid.take_clipboard();
            // automatic-rename: if the app set an OSC title and this pane is the
            // active pane of its window (with auto-rename on), adopt it as the
            // window name.
            if let Some(title) = p.grid.title().map(str::to_string) {
                self.apply_auto_title(id, &title);
            }
            // An OSC 52 copy also lands in lumux's own paste-buffer stack (tmux
            // does this), so `prefix ]` can paste what the app copied.
            if let Some(text) = &clipboard {
                self.buffers.push(text.clone());
            }
            PaneFeed { bell, clipboard }
        } else {
            PaneFeed::default()
        }
    }

    /// Update the window owning `pid` to `title` when that pane is the window's
    /// active pane and automatic-rename is on. Searches all sessions for the
    /// pane's window (panes are keyed globally, windows aren't indexed by pane).
    fn apply_auto_title(&mut self, pid: PaneId, title: &str) {
        for sid in self.server.session_ids() {
            let Some(s) = self.server.session_mut(sid) else {
                continue;
            };
            for wid in s.window_ids() {
                if let Some(w) = s.window_mut(wid) {
                    if w.active_pane() == pid {
                        w.apply_auto_title(title);
                        return;
                    }
                }
            }
        }
    }

    /// Append `bytes` to the capture file when capture is enabled. Enabled by the
    /// presence of `LUMUX_CAPTURE` (env, names the output file) OR a sentinel file
    /// `%USERPROFILE%\lumux-capture.on` (Windows) / `$HOME/lumux-capture.on`, in
    /// which case bytes go to `lumux-capture.bin` beside it. The sentinel path
    /// works even for a daemon that was already running before capture was asked
    /// for, sidestepping detached-process env inheritance.
    fn maybe_capture(bytes: &[u8]) {
        use std::io::Write as _;
        let out: Option<std::path::PathBuf> = if let Some(p) = std::env::var_os("LUMUX_CAPTURE") {
            Some(std::path::PathBuf::from(p))
        } else {
            let home = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(std::path::PathBuf::from);
            home.and_then(|h| {
                if h.join("lumux-capture.on").exists() {
                    Some(h.join("lumux-capture.bin"))
                } else {
                    None
                }
            })
        };
        if let Some(path) = out {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = f.write_all(bytes);
            }
        }
    }

    /// Write user input to a pane's PTY.
    pub fn write_pane(&mut self, id: PaneId, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(p) = self.panes.get_mut(&id) {
            p.writer.write_input(bytes)?;
        }
        Ok(())
    }

    /// Write user input to the active pane, OR — when the active window has
    /// synchronize-panes on (tmux) — to every pane in that window. Returns true
    /// if input was broadcast (so the caller knows multiple panes changed).
    pub fn write_input(&mut self, session: SessionId, bytes: &[u8]) -> bool {
        let (synced, pane_ids, active) = {
            let Some(s) = self.server.session(session) else {
                return false;
            };
            let Some(w) = s.window(s.active_window()) else {
                return false;
            };
            (w.is_synchronized(), w.pane_ids(), w.active_pane())
        };
        if synced {
            for pid in pane_ids {
                let _ = self.write_pane(pid, bytes);
            }
            true
        } else {
            let _ = self.write_pane(active, bytes);
            false
        }
    }

    /// Toggle synchronize-panes for the active window (tmux). Returns the new
    /// state. Flashes a confirmation and forces a repaint so the status reflects it.
    pub fn toggle_sync(&mut self, client_id: u64, session: SessionId) -> bool {
        let on = self
            .server
            .session_mut(session)
            .map(|s| s.active_window_mut().toggle_synchronized())
            .unwrap_or(false);
        self.flash_message(
            client_id,
            if on {
                "synchronize-panes: on"
            } else {
                "synchronize-panes: off"
            },
        );
        on
    }

    /// Whether the active window of `session` has synchronize-panes on (for the
    /// status indicator).
    pub fn is_synchronized(&self, session: SessionId) -> bool {
        self.server
            .session(session)
            .and_then(|s| s.window(s.active_window()))
            .map(|w| w.is_synchronized())
            .unwrap_or(false)
    }

    /// Register a freshly attached client: give it a keymap and renderer.
    pub fn register_client(&mut self, client_id: u64) {
        let mut keymap = match self.config.to_bindings() {
            Ok(b) => Keymap::new(b),
            Err(_) => Keymap::with_defaults(),
        };
        // Apply the copy-mode key style (tmux mode-keys).
        if self.config.mode_keys.eq_ignore_ascii_case("emacs") {
            keymap.set_mode_keys(lumux_core::keymap::ModeKeys::Emacs);
        }
        self.keymaps.insert(client_id, keymap);
        self.renderers.insert(client_id, ClientRenderer::new());
    }

    pub fn unregister_client(&mut self, client_id: u64) {
        self.keymaps.remove(&client_id);
        self.renderers.remove(&client_id);
        self.copy.remove(&client_id);
        self.message.remove(&client_id);
        self.help.remove(&client_id);
        self.help_offset.remove(&client_id);
        self.choosing.remove(&client_id);
        self.choosing_buffer.remove(&client_id);
        self.showing_panes.remove(&client_id);
        self.prompt.remove(&client_id);
        self.search.remove(&client_id);
        self.dragging.remove(&client_id);
        self.sidebar_scroll.remove(&client_id);
        self.server.detach_client(client_id);
    }

    /// Open a rename prompt for a client, seeded with the current name so the
    /// user can edit rather than retype it (tmux prefix , / $).
    pub fn open_prompt(&mut self, client_id: u64, session: SessionId, target: PromptTarget) {
        let seed = match target {
            PromptTarget::Session => self.server.session(session).map(|s| s.name.clone()),
            PromptTarget::Window => self
                .server
                .session(session)
                .and_then(|s| s.window(s.active_window()).map(|w| w.name.clone())),
            // find-window starts from an empty query, not the current name.
            PromptTarget::FindWindow => Some(String::new()),
            // The command-prompt starts empty.
            PromptTarget::Command => Some(String::new()),
        }
        .unwrap_or_default();
        self.prompt.insert(
            client_id,
            Prompt {
                target,
                buffer: seed,
            },
        );
        // Keep the keymap in lockstep. A prompt opened from the keyboard already
        // transitioned via `feed`, but one opened by a context-menu click did
        // not — without this its keystrokes would go to the pane and the prompt
        // could never be typed into or dismissed.
        if let Some(k) = self.keymaps.get_mut(&client_id) {
            k.enter_prompt_mode();
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    pub fn in_prompt(&self, client_id: u64) -> bool {
        self.prompt.contains_key(&client_id)
    }

    /// Append a character to a client's open prompt buffer.
    pub fn prompt_push(&mut self, client_id: u64, c: char) {
        if let Some(p) = self.prompt.get_mut(&client_id) {
            p.buffer.push(c);
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Delete the last character of a client's open prompt buffer.
    pub fn prompt_backspace(&mut self, client_id: u64) {
        if let Some(p) = self.prompt.get_mut(&client_id) {
            p.buffer.pop();
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Cancel a client's prompt without applying it.
    pub fn prompt_cancel(&mut self, client_id: u64) {
        if self.prompt.remove(&client_id).is_some() {
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Commit a client's prompt and return intent. The daemon owns editing and
    /// lookup only; the event loop applies every model mutation through its
    /// shared focus / global-projection lifecycle. An empty name is ignored,
    /// matching tmux.
    pub fn prompt_confirm(&mut self, client_id: u64, session: SessionId) -> Option<PromptOutcome> {
        let p = self.prompt.remove(&client_id)?;
        let mut outcome = None;
        let name = p.buffer.trim().to_string();
        if !name.is_empty() {
            match p.target {
                PromptTarget::Session => {
                    outcome = Some(PromptOutcome::RenameSession(name));
                }
                PromptTarget::Window => {
                    outcome = self
                        .server
                        .session(session)
                        .map(|s| PromptOutcome::RenameWindow {
                            window: s.active_window(),
                            name,
                        });
                }
                PromptTarget::FindWindow => {
                    // Switch to the first window whose name contains the query
                    // (case-insensitive). Flash if nothing matches.
                    let needle = name.to_lowercase();
                    let target = self.server.session(session).and_then(|s| {
                        s.window_ids().into_iter().find(|&wid| {
                            s.window(wid)
                                .map(|w| w.name.to_lowercase().contains(&needle))
                                .unwrap_or(false)
                        })
                    });
                    match target {
                        Some(wid) => outcome = Some(PromptOutcome::FocusWindow(wid)),
                        None => {
                            self.flash_message(client_id, format!("no window matching \"{name}\""))
                        }
                    }
                }
                // The event loop owns command dispatch (it can spawn PTYs etc.).
                PromptTarget::Command => outcome = Some(PromptOutcome::Command(name)),
            }
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        outcome
    }

    /// Open a copy-mode search input for `client_id` in `dir` (tmux `/` forward,
    /// `?` backward). Seeds an empty query; subsequent keys edit it. No-op if the
    /// client isn't in copy-mode.
    pub fn search_open(&mut self, client_id: u64, dir: lumux_core::copymode::SearchDir) {
        if !self.in_copy_mode(client_id) {
            return;
        }
        self.search.insert(
            client_id,
            SearchInput {
                buffer: String::new(),
                dir,
            },
        );
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    pub fn in_search(&self, client_id: u64) -> bool {
        self.search.contains_key(&client_id)
    }

    /// The open search query and its direction prefix char (`/` or `?`) for the
    /// status line, if this client is typing a search.
    pub fn search_prompt(&self, client_id: u64) -> Option<(char, &str)> {
        self.search.get(&client_id).map(|s| {
            let prefix = match s.dir {
                lumux_core::copymode::SearchDir::Forward => '/',
                lumux_core::copymode::SearchDir::Backward => '?',
            };
            (prefix, s.buffer.as_str())
        })
    }

    pub fn search_push(&mut self, client_id: u64, c: char) {
        if let Some(s) = self.search.get_mut(&client_id) {
            s.buffer.push(c);
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    pub fn search_backspace(&mut self, client_id: u64) {
        if let Some(s) = self.search.get_mut(&client_id) {
            s.buffer.pop();
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Cancel the search input, returning to copy-mode navigation. The cursor
    /// stays where it was (no jump).
    pub fn search_cancel(&mut self, client_id: u64) {
        if self.search.remove(&client_id).is_some() {
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Run the typed search against the active pane's copy-mode buffer, moving
    /// the copy cursor to the first match. Returns false if there was no match
    /// (the caller flashes a message). An empty query just closes the input.
    pub fn search_confirm(&mut self, client_id: u64, session: SessionId) -> bool {
        let Some(input) = self.search.remove(&client_id) else {
            return true;
        };
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        let query = input.buffer;
        if query.trim().is_empty() {
            return true;
        }
        if !self.copy_target_is_active(client_id, session) {
            self.exit_copy_mode(client_id);
            return true;
        }
        let pid = self
            .copy
            .get(&client_id)
            .expect("validated copy target")
            .pane;
        let Some(grid) = self.panes.get(&pid).map(|p| &p.grid) else {
            return true;
        };
        match self.copy.get_mut(&client_id) {
            Some(cm) => cm.search(&query, input.dir, grid),
            None => true,
        }
    }

    /// Repeat the last copy-mode search (tmux `n`/`N`). `same_dir` true keeps the
    /// original direction (`n`), false reverses it (`N`). Returns false when
    /// there was no further match.
    pub fn search_repeat(&mut self, client_id: u64, session: SessionId, same_dir: bool) -> bool {
        if !self.copy_target_is_active(client_id, session) {
            self.exit_copy_mode(client_id);
            return true;
        }
        let pid = self
            .copy
            .get(&client_id)
            .expect("validated copy target")
            .pane;
        let Some(grid) = self.panes.get(&pid).map(|p| &p.grid) else {
            return true;
        };
        match self.copy.get_mut(&client_id) {
            Some(cm) => cm.search_repeat(same_dir, grid),
            None => true,
        }
    }

    /// Paste the most-recent buffer's text into the active pane (tmux prefix `]`
    /// / `paste-buffer`). Returns false if there are no buffers (caller flashes a
    /// message). The bytes go straight to the pane's PTY, like typed input.
    pub fn paste_buffer(&mut self, session: SessionId) -> bool {
        let Some(text) = self.buffers.top().map(str::to_string) else {
            return false;
        };
        if let Some(pid) = self.active_pane(session) {
            let _ = self.write_pane(pid, text.as_bytes());
        }
        true
    }

    /// Paste a specific named buffer (tmux `paste-buffer -b name`); falls back to
    /// the most-recent buffer when `name` is None. Returns false if the target
    /// buffer doesn't exist.
    pub fn paste_named_buffer(&mut self, session: SessionId, name: Option<&str>) -> bool {
        let text = match name {
            Some(n) => self.buffers.text_of(n).map(str::to_string),
            None => self.buffers.top().map(str::to_string),
        };
        let Some(text) = text else {
            return false;
        };
        if let Some(pid) = self.active_pane(session) {
            let _ = self.write_pane(pid, text.as_bytes());
        }
        true
    }

    /// Store text in a paste buffer (tmux `set-buffer`), named or auto-named.
    pub fn set_buffer(&mut self, name: Option<&str>, text: &str) {
        match name {
            Some(n) => {
                self.buffers.push_named(n, text);
            }
            None => {
                self.buffers.push(text);
            }
        }
    }

    /// Delete a named buffer (tmux `delete-buffer -b`). Returns true if removed.
    pub fn delete_named_buffer(&mut self, name: &str) -> bool {
        self.buffers.delete_named(name)
    }

    /// Write a buffer's text to a file (tmux `save-buffer`). `name` None saves the
    /// most-recent buffer. Returns Ok on success, Err(message) otherwise.
    pub fn save_buffer(&self, name: Option<&str>, path: &str) -> Result<(), String> {
        let text = match name {
            Some(n) => self.buffers.text_of(n),
            None => self.buffers.top(),
        };
        let Some(text) = text else {
            return Err("no such buffer".to_string());
        };
        std::fs::write(path, text).map_err(|e| format!("save-buffer: {e}"))
    }

    /// Read a file into a new paste buffer (tmux `load-buffer`). Returns the new
    /// buffer's name, or Err(message) on an I/O error.
    pub fn load_buffer(&mut self, path: &str) -> Result<String, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("load-buffer: {e}"))?;
        self.buffers
            .push(text)
            .ok_or_else(|| "load-buffer: file was empty".to_string())
    }

    /// Capture the active pane's visible text into a new paste buffer (tmux
    /// capture-pane). Trailing blank lines are dropped. Returns the buffer name,
    /// or None if there's no active pane / nothing to capture.
    pub fn capture_pane(&mut self, session: SessionId) -> Option<String> {
        let pid = self.active_pane(session)?;
        let lines = self.panes.get(&pid)?.grid.screen_text();
        // Trim trailing blank lines so the buffer ends at real content.
        let last = lines.iter().rposition(|l| !l.trim().is_empty())?;
        let text = lines[..=last].join("\n");
        self.buffers.push(text)
    }

    /// Run a shell command (tmux run-shell) and push its combined stdout/stderr
    /// into a paste buffer. Returns a short status string for the flash line. The
    /// command runs via `sh -c` with a captured, non-interactive stdout — output
    /// is bounded so a runaway command can't blow up memory.
    pub fn run_shell(&mut self, cmd: &str) -> String {
        use std::process::Command;
        let output = Command::new("sh").arg("-c").arg(cmd).output();
        match output {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                if !out.stderr.is_empty() {
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                }
                // Bound the captured output (256 KiB) so a huge dump is safe.
                const CAP: usize = 256 * 1024;
                if text.len() > CAP {
                    text.truncate(CAP);
                }
                let trimmed = text.trim_end().to_string();
                if trimmed.is_empty() {
                    format!("run-shell: exit {}", out.status.code().unwrap_or(-1))
                } else {
                    match self.buffers.push(trimmed) {
                        Some(name) => format!("run-shell output → {name}"),
                        None => "run-shell: no output".to_string(),
                    }
                }
            }
            Err(e) => format!("run-shell failed: {e}"),
        }
    }

    /// Open the paste-buffer chooser for a client (tmux prefix `=`). Returns
    /// false (and opens nothing) when there are no buffers.
    pub fn open_buffer_chooser(&mut self, client_id: u64) -> bool {
        if self.buffers.is_empty() {
            return false;
        }
        self.choosing_buffer.insert(client_id, 0);
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        true
    }

    pub fn in_buffer_chooser(&self, client_id: u64) -> bool {
        self.choosing_buffer.contains_key(&client_id)
    }

    /// Move the buffer-chooser selection (delta -1/+1, or absolute `to`), clamped
    /// to the buffer list.
    pub fn buffer_chooser_move(&mut self, client_id: u64, delta: i32, to: Option<usize>) {
        let n = self.buffers.len();
        if n == 0 {
            return;
        }
        if let Some(sel) = self.choosing_buffer.get_mut(&client_id) {
            *sel = match to {
                Some(i) => i.min(n - 1),
                None => (*sel as i32 + delta).clamp(0, n as i32 - 1) as usize,
            };
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Confirm the chooser: paste the highlighted buffer into the active pane and
    /// close the chooser.
    pub fn buffer_chooser_confirm(&mut self, client_id: u64, session: SessionId) {
        let Some(sel) = self.choosing_buffer.remove(&client_id) else {
            return;
        };
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        let text = self.buffers.get(sel).map(|b| b.text.clone());
        if let (Some(text), Some(pid)) = (text, self.active_pane(session)) {
            let _ = self.write_pane(pid, text.as_bytes());
        }
    }

    /// Delete the highlighted buffer, keeping the chooser open (the selection
    /// clamps to the new length). Closes the chooser if it empties the stack.
    pub fn buffer_chooser_delete(&mut self, client_id: u64) {
        let Some(&sel) = self.choosing_buffer.get(&client_id) else {
            return;
        };
        self.buffers.delete(sel);
        if self.buffers.is_empty() {
            self.choosing_buffer.remove(&client_id);
        } else if let Some(s) = self.choosing_buffer.get_mut(&client_id) {
            *s = (*s).min(self.buffers.len() - 1);
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Close the buffer chooser without pasting.
    pub fn buffer_chooser_cancel(&mut self, client_id: u64) {
        if self.choosing_buffer.remove(&client_id).is_some() {
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Open the display-panes overlay for a client (tmux prefix q): pane numbers
    /// are drawn over each pane until the next key picks one (or dismisses it).
    pub fn show_pane_numbers(&mut self, client_id: u64) {
        self.showing_panes.insert(client_id);
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    pub fn in_display_panes(&self, client_id: u64) -> bool {
        self.showing_panes.contains(&client_id)
    }

    /// Close the display-panes overlay without changing focus.
    pub fn hide_pane_numbers(&mut self, client_id: u64) {
        if self.showing_panes.remove(&client_id) {
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Resolve and focus the pane shown as `number` in the display-panes overlay
    /// (1-based, offset by base-index). Always closes the overlay and returns the
    /// focused pane so the event loop can apply its user-focus lifecycle.
    pub fn pick_pane_number(
        &mut self,
        client_id: u64,
        session: SessionId,
        number: u32,
    ) -> Option<PaneId> {
        self.showing_panes.remove(&client_id);
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        let base = self.base_index();
        let idx = number.saturating_sub(base) as usize;
        if let Some(s) = self.server.session_mut(session) {
            let wid = s.active_window();
            if let Some(w) = s.window_mut(wid) {
                let ids = w.pane_ids();
                if let Some(&pid) = ids.get(idx) {
                    if w.focus_pane(pid) {
                        return Some(pid);
                    }
                }
            }
        }
        None
    }

    pub fn toggle_help(&mut self, client_id: u64) {
        if !self.help.remove(&client_id) {
            self.help.insert(client_id);
            self.help_offset.insert(client_id, 0); // fresh open starts at the top
        } else {
            self.help_offset.remove(&client_id);
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Toggle the big-digit clock overlay (tmux `clock-mode`, prefix `t`).
    pub fn toggle_clock(&mut self, client_id: u64) {
        if !self.clock.remove(&client_id) {
            self.clock.insert(client_id);
            // Keep the keymap in lockstep so the next key closes the overlay
            // (Clock mode's "any key"), matching how prefix-`t` enters via feed().
            // Opening via the `:clock-mode` command bypasses feed(), so set it
            // here too; a keyboard open just re-sets the same mode harmlessly.
            if let Some(k) = self.keymaps.get_mut(&client_id) {
                k.enter_clock_mode();
            }
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Scroll the help overlay for a client (tmux-style up/down/paging). The
    /// offset is clamped against the list length at render time, so this only
    /// needs to move it; over-scrolling is harmless.
    pub fn help_scroll(&mut self, client_id: u64, key: lumux_core::keymap::HelpKey) {
        use lumux_core::keymap::HelpKey;
        let off = self.help_offset.entry(client_id).or_insert(0);
        // A page is most of the visible binding rows; the exact clamp happens in
        // render_help, which knows the screen height and entry count.
        const PAGE: usize = 10;
        *off = match key {
            HelpKey::Up => off.saturating_sub(1),
            HelpKey::Down => off.saturating_add(1),
            HelpKey::PageUp => off.saturating_sub(PAGE),
            HelpKey::PageDown => off.saturating_add(PAGE),
            HelpKey::Top => 0,
            HelpKey::Bottom => usize::MAX, // clamped to the last page in render
            HelpKey::Close => *off,        // handled by toggle_help; no-op here
        };
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
        // Start the cursor on the client's current session row (all collapsed).
        let tree = ChooseTree::default();
        let rows = self.tree_rows(&tree);
        let cursor = current
            .and_then(|sid| {
                rows.iter()
                    .position(|r| matches!(r, TreeRow::Session(s) if *s == sid))
            })
            .unwrap_or(0);
        let _ = sessions; // (kept for clarity; rows already reflects the session list)
        self.choosing
            .insert(client_id, ChooseTree { cursor, ..tree });
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    pub fn in_chooser(&self, client_id: u64) -> bool {
        self.choosing.contains_key(&client_id)
    }

    /// Flatten the sessions (and, for expanded ones, their windows) into the
    /// ordered list of visible choose-tree rows.
    fn tree_rows(&self, tree: &ChooseTree) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        for sid in self.server.session_ids() {
            rows.push(TreeRow::Session(sid));
            if tree.expanded.contains(&sid) {
                if let Some(s) = self.server.session(sid) {
                    for wid in s.window_ids() {
                        rows.push(TreeRow::Window(sid, wid));
                    }
                }
            }
        }
        rows
    }

    /// The row the cursor is currently on, if any.
    fn tree_cursor_row(&self, client_id: u64) -> Option<TreeRow> {
        let tree = self.choosing.get(&client_id)?;
        let rows = self.tree_rows(tree);
        rows.get(tree.cursor.min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// Force a client's next render to be a full repaint (e.g. after switching
    /// session, where the whole screen changes).
    pub fn invalidate_client(&mut self, client_id: u64) {
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Move the choose-tree cursor. `delta` of -1/+1 = up/down over the flattened
    /// rows; `to` jumps to the Nth top-level SESSION (digit key), regardless of
    /// expansion. Clamped to the visible rows.
    pub fn chooser_move(&mut self, client_id: u64, delta: i32, to: Option<usize>) {
        let Some(tree) = self.choosing.get(&client_id) else {
            return;
        };
        let rows = self.tree_rows(tree);
        if rows.is_empty() {
            return;
        }
        let next = match to {
            // A digit selects the Nth session row (skip window rows).
            Some(n) => rows
                .iter()
                .enumerate()
                .filter(|(_, r)| matches!(r, TreeRow::Session(_)))
                .nth(n)
                .map(|(idx, _)| idx)
                .unwrap_or_else(|| tree.cursor.min(rows.len() - 1)),
            None => {
                let cur = tree.cursor as i32 + delta;
                cur.clamp(0, rows.len() as i32 - 1) as usize
            }
        };
        if let Some(tree) = self.choosing.get_mut(&client_id) {
            tree.cursor = next;
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Expand the session on the cursor row to reveal its windows (tmux
    /// choose-tree Right/`l`). No-op on a window row or an already-expanded
    /// session.
    pub fn chooser_expand(&mut self, client_id: u64) {
        let Some(row) = self.tree_cursor_row(client_id) else {
            return;
        };
        if let TreeRow::Session(sid) = row {
            if let Some(tree) = self.choosing.get_mut(&client_id) {
                if tree.expanded.insert(sid) {
                    if let Some(r) = self.renderers.get_mut(&client_id) {
                        r.invalidate();
                    }
                }
            }
        }
    }

    /// Collapse (tmux choose-tree Left/`h`): on an expanded session row, hide its
    /// windows; on a window row, collapse the parent session and move the cursor
    /// to it.
    pub fn chooser_collapse(&mut self, client_id: u64) {
        let Some(row) = self.tree_cursor_row(client_id) else {
            return;
        };
        let target_sid = match row {
            TreeRow::Session(sid) => sid,
            TreeRow::Window(sid, _) => sid,
        };
        if let Some(tree) = self.choosing.get_mut(&client_id) {
            tree.expanded.remove(&target_sid);
        }
        // Re-point the cursor at the (now collapsed) session row.
        if let Some(tree) = self.choosing.get(&client_id) {
            let rows = self.tree_rows(tree);
            if let Some(idx) = rows
                .iter()
                .position(|r| matches!(r, TreeRow::Session(s) if *s == target_sid))
            {
                if let Some(tree) = self.choosing.get_mut(&client_id) {
                    tree.cursor = idx;
                }
            }
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Confirm the choose-tree and return its navigation intent. This module
    /// owns overlay state only; the event loop owns the atomic client switch,
    /// focus acknowledgement, geometry reconciliation, and repaint lifecycle.
    pub fn chooser_confirm(&mut self, client_id: u64) -> Option<ChooserPick> {
        let row = self.tree_cursor_row(client_id)?;
        self.choosing.remove(&client_id);
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        Some(match row {
            TreeRow::Session(sid) => ChooserPick::Session(sid),
            TreeRow::Window(sid, wid) => ChooserPick::Window(sid, wid),
        })
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

    /// Exit copy mode before navigation when the validated destination no
    /// longer denotes the buffer that owns this client's copy state.
    pub(crate) fn exit_copy_mode_if_focus_changes(
        &mut self,
        client_id: u64,
        session: SessionId,
        window: WindowId,
        pane: Option<PaneId>,
    ) {
        let changes_target = self.copy.get(&client_id).is_some_and(|copy| {
            copy.session != session
                || copy.window != window
                || pane.is_some_and(|pane| copy.pane != pane)
        });
        if changes_target {
            self.exit_copy_mode(client_id);
        }
    }

    pub(crate) fn copy_target_matches(&self, client_id: u64, target: InteractionPane) -> bool {
        self.copy.get(&client_id).is_some_and(|copy| {
            (copy.session, copy.window, copy.pane) == (target.session, target.window, target.pane)
        }) && self.interaction_pane_is_active(target)
            && self.interaction_pane_modes_match(target)
    }

    fn copy_target_is_active(&self, client_id: u64, session: SessionId) -> bool {
        self.copy.get(&client_id).is_some_and(|copy| {
            copy.session == session
                && self.server.session(copy.session).is_some_and(|current| {
                    current.active_window() == copy.window
                        && current
                            .window(copy.window)
                            .is_some_and(|window| window.active_pane() == copy.pane)
                })
                && self
                    .panes
                    .get(&copy.pane)
                    .is_some_and(|pane| !pane.grid.alt_screen())
        })
    }

    /// Drop every client-owned part of copy interaction state. Copy cursors and
    /// selections are meaningful only for the pane buffer where they started;
    /// navigation to another pane/session must call this before changing focus.
    pub fn exit_copy_mode(&mut self, client_id: u64) {
        let changed = self.copy.remove(&client_id).is_some()
            | self.search.remove(&client_id).is_some()
            | self.mouse_sel.remove(&client_id).is_some();
        if changed {
            if let Some(keymap) = self.keymaps.get_mut(&client_id) {
                keymap.reset();
            }
            if let Some(renderer) = self.renderers.get_mut(&client_id) {
                renderer.invalidate();
            }
        }
    }

    /// Enter copy-mode for `client_id`, anchored at the active pane's live tail.
    /// No-op when the active pane is on the alternate screen: a full-screen app
    /// (vim/less) owns the viewport and has no scrollback to browse, so copy-mode
    /// there would only show stale primary-screen history (tmux blocks it too).
    pub fn enter_copy_mode(&mut self, client_id: u64, session: SessionId) {
        let window = self
            .server
            .session(session)
            .map(|session| session.active_window());
        if let (Some(window), Some(pid)) = (window, self.active_pane(session)) {
            if let Some(p) = self.panes.get(&pid) {
                if p.grid.alt_screen() {
                    return;
                }
                self.copy.insert(
                    client_id,
                    CopySession {
                        session,
                        window,
                        pane: pid,
                        mode: CopyMode::enter(&p.grid),
                    },
                );
                // Keep the keymap in lockstep: copy-mode can be entered by the
                // keyboard (where `feed` already set Copy mode) OR by a mouse
                // wheel scroll (which bypasses `feed`). Forcing it here means keys
                // like `q`/arrows are interpreted as copy-mode keys either way,
                // instead of leaking through to the shell.
                if let Some(k) = self.keymaps.get_mut(&client_id) {
                    k.enter_copy_mode();
                }
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
        if !self.copy_target_is_active(client_id, session) {
            self.exit_copy_mode(client_id);
            return false;
        }
        let pid = self
            .copy
            .get(&client_id)
            .expect("validated copy target")
            .pane;
        let Some(grid) = self.panes.get(&pid).map(|p| &p.grid) else {
            return false;
        };
        let still = match self.copy.get_mut(&client_id) {
            Some(cm) => cm.navigate(key, grid),
            None => false,
        };
        if !still {
            self.copy.remove(&client_id);
            // A search input can't outlive copy-mode; drop it too.
            self.search.remove(&client_id);
            // Copy-mode ended: bring the keymap back to Normal so subsequent keys
            // go to the shell (mirrors the keyboard `q`/Escape reset path).
            if let Some(k) = self.keymaps.get_mut(&client_id) {
                k.reset();
            }
            // The view reverts from the scrolled copy view to the live shell —
            // force a clean repaint for that transition.
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
        // While still scrolling, do NOT invalidate: render_copy_mode produces a
        // full Screen and the differ emits a minimal incremental update. Forcing
        // a full repaint here would clear the screen (ESC[2J) every wheel notch,
        // which shows as flicker.
        still
    }

    /// Toggle/begin a selection at the copy cursor.
    pub fn copy_start_selection(&mut self, client_id: u64) {
        if let Some(cm) = self.copy.get_mut(&client_id) {
            cm.start_selection();
        }
    }

    /// Toggle rectangle (block) selection for the client's copy-mode (tmux
    /// rectangle-toggle). Begins a selection if none is active.
    pub fn copy_toggle_rectangle(&mut self, client_id: u64) {
        if let Some(cm) = self.copy.get_mut(&client_id) {
            cm.toggle_rectangle();
            if let Some(r) = self.renderers.get_mut(&client_id) {
                r.invalidate();
            }
        }
    }

    /// Yank the current selection to the clipboard and exit copy-mode. Returns
    /// the OSC-52-or-similar bytes to forward to the client, if any.
    pub fn copy_yank(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        if !self.copy_target_is_active(client_id, session) {
            self.exit_copy_mode(client_id);
            return None;
        }
        let pid = self.copy.get(&client_id)?.pane;
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
        // Push onto the paste-buffer stack (tmux: a copy-mode yank becomes the
        // newest buffer) as well as the system/OSC-52 clipboard.
        self.buffers.push(text.clone());
        let _ = self.clipboard.set_text(&text);
        // If a copy-command is configured (tmux copy-pipe integration), also pipe
        // the selection to it (e.g. `set -s copy-command 'xclip -i'`).
        let copy_command = self.config.copy_command.clone();
        if !copy_command.is_empty() {
            self.pipe_to_command(&copy_command, &text);
        }
        Some(text)
    }

    /// Spawn `sh -c cmd` and write `input` to its stdin (best-effort; errors are
    /// swallowed — a failed pipe shouldn't crash the daemon). Used by the
    /// copy-command (copy-pipe) integration on yank.
    fn pipe_to_command(&self, cmd: &str, input: &str) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(input.as_bytes());
            }
            // Reap so the child doesn't linger as a zombie.
            let _ = child.wait();
        }
    }

    fn active_pane(&self, session: SessionId) -> Option<PaneId> {
        let s = self.server.session(session)?;
        Some(s.window(s.active_window())?.active_pane())
    }

    /// The currently marked pane (tmux `select-pane -m`), if any.
    pub fn marked_pane(&self) -> Option<(SessionId, PaneId)> {
        self.marked_pane
    }

    /// Toggle the mark on the active pane of `session` (tmux prefix `m`). Marking
    /// the already-marked pane clears the mark; marking a different pane moves
    /// it. Returns true if a pane is now marked, false if the mark was cleared.
    pub fn toggle_marked_pane(&mut self, session: SessionId) -> bool {
        let Some(pid) = self.active_pane(session) else {
            return false;
        };
        if self.marked_pane == Some((session, pid)) {
            self.marked_pane = None;
            false
        } else {
            self.marked_pane = Some((session, pid));
            true
        }
    }

    /// Clear the mark if it points at `pid` (called when a pane closes so a stale
    /// mark can't dangle).
    pub fn clear_mark_if(&mut self, pid: PaneId) {
        if matches!(self.marked_pane, Some((_, m)) if m == pid) {
            self.marked_pane = None;
        }
    }

    /// Whether `pid`'s pane is currently on the alternate screen (a full-screen
    /// app like vim/less or a TUI agent owns the viewport). Such panes have no
    /// scrollback to browse, so a mouse wheel must be translated into arrow-key
    /// input for the app rather than entering copy-mode.
    pub fn pane_on_alt_screen(&self, pid: PaneId) -> bool {
        self.panes.get(&pid).is_some_and(|p| p.grid.alt_screen())
    }

    /// Whether the app in `pid`'s pane has enabled mouse reporting. When true,
    /// the daemon forwards raw mouse events to it (re-encoded pane-relative)
    /// instead of using them for scroll/copy-mode/selection — like tmux.
    pub fn pane_wants_mouse(&self, pid: PaneId) -> bool {
        self.panes.get(&pid).is_some_and(|p| p.grid.wants_mouse())
    }

    /// Poll every live pane's child and return those that have exited. ConPTY
    /// does not reliably deliver read-EOF when a shell exits (the output pipe can
    /// stay open after the child is gone), so relying on the reader thread's EOF
    /// alone would leak sessions on Windows. The control loop calls this on a
    /// timer and cascade-closes each returned pane. Panes already marked dead
    /// (their EOF path fired first) are skipped, so this never double-reports.
    pub fn reap_exited_panes(&mut self) -> Vec<PaneId> {
        let mut exited = Vec::new();
        for (id, pane) in self.panes.iter_mut() {
            if pane.dead {
                continue;
            }
            if let Ok(Some(_)) = pane.writer.try_wait() {
                pane.dead = true;
                exited.push(*id);
            }
        }
        exited
    }

    /// Mark a pane dead (its child exited) and cascade-close it in the model.
    /// Returns the cascade result so the loop can notify/close clients.
    pub fn close_pane(&mut self, session: SessionId, pane: PaneId) -> CascadeResult {
        if let Some(p) = self.panes.get_mut(&pane) {
            p.dead = true;
        }
        self.panes.remove(&pane);
        // A pane's agent status is live: once the process is gone the last
        // "working"/"blocked" report is stale, so clear it here rather than let
        // it linger on the sidebar (the dominant staleness source).
        self.agent_lifecycles.remove(&pane);
        self.detected_agents.remove(&pane);
        let result = self.server.kill_pane(session, pane);
        if result == CascadeResult::SessionClosed {
            self.clear_session_transients(session);
        }
        result
    }

    /// Apply a pane's self-reported lifecycle state. This is the single seam
    /// that derives an unseen completion from `working/blocked -> idle` and
    /// suppresses duplicate reports. Returns whether visible status changed.
    pub fn report_agent_state(&mut self, pane: PaneId, report: AgentReport) -> bool {
        use std::collections::btree_map::Entry;

        // Reports for unknown/dead panes are telemetry about a lifecycle that no
        // longer exists. Do not leave sequence tombstones for arbitrary ids.
        if self.panes.get(&pane).is_none_or(|pane| pane.dead) {
            return false;
        }

        match self.agent_lifecycles.entry(pane) {
            Entry::Vacant(entry) => {
                entry.insert(AgentLifecycle::reported(report));
                true
            }
            Entry::Occupied(mut entry) => entry.get_mut().report(report),
        }
    }

    /// Clear a pane's agent status (the agent process exited but the pane lives).
    /// Only the exact agent/owner lifecycle may clear its row. A fresh clear
    /// advances the sequence even when no visible status existed, so an older
    /// in-flight report cannot recreate the row afterward.
    pub fn clear_agent_status(&mut self, pane: PaneId, clear: AgentClear) -> bool {
        use std::collections::btree_map::Entry;

        if self.panes.get(&pane).is_none_or(|pane| pane.dead) {
            return false;
        }

        match self.agent_lifecycles.entry(pane) {
            // Preserve the old clear-before-report behavior: the invisible
            // tombstone prevents a delayed earlier report from creating a row.
            Entry::Vacant(entry) => {
                entry.insert(AgentLifecycle::cleared(clear));
                false
            }
            Entry::Occupied(mut entry) => entry.get_mut().clear(clear),
        }
    }

    /// Mark a completed turn in `pane` as seen. Active or blocked states are
    /// intentionally unchanged. Returns whether Done became Idle.
    pub fn acknowledge_agent(&mut self, pane: PaneId) -> bool {
        self.agent_lifecycles
            .get_mut(&pane)
            .and_then(|lifecycle| lifecycle.status.as_mut())
            .is_some_and(lumux_core::agent::AgentStatus::acknowledge)
    }

    /// The agent status shown for `pane`: the hook-reported lifecycle when there
    /// is one, else the process-detected presence.
    ///
    /// Hooks win because they carry real state (working/blocked) and the unseen
    /// completion badge; detection only knows "an agent is running here".
    pub fn agent_status(&self, pane: PaneId) -> Option<&lumux_core::agent::AgentStatus> {
        self.agent_lifecycles
            .get(&pane)
            .and_then(|lifecycle| lifecycle.status.as_ref())
            .or_else(|| self.detected_agents.get(&pane))
    }

    /// Re-scan every live pane for a running agent process. Returns true when
    /// the visible set changed, so the caller can repaint.
    ///
    /// Detection is authoritative for *disappearance* as well: when a pane that
    /// previously had a detected agent no longer does, any stale hook status for
    /// it is dropped too — the process is provably gone, which is more reliable
    /// than waiting for an exit hook that may never fire. Panes that were never
    /// detected are left alone, so a platform without process inspection (the
    /// default empty implementation) can never wipe hook-reported state.
    pub fn refresh_detected_agents(&mut self) -> bool {
        let mut changed = false;
        let panes: Vec<PaneId> = self.panes.keys().copied().collect();
        for pane in panes {
            let Some(pid) = self.panes.get(&pane).and_then(|p| p.writer.child_pid()) else {
                continue;
            };
            let names = self.pty_system.descendant_process_names(pid);
            let detected = lumux_core::detect::identify_agent_among(&names);
            match detected {
                Some(agent) => {
                    let is_new = self
                        .detected_agents
                        .get(&pane)
                        .map(|s| s.agent != agent)
                        .unwrap_or(true);
                    if is_new {
                        self.detected_agents.insert(
                            pane,
                            lumux_core::agent::AgentStatus::new(
                                agent,
                                lumux_core::agent::AgentState::Idle,
                            ),
                        );
                        // Only a visible change if no hook status was covering it.
                        changed |= self.hook_status(pane).is_none();
                    }
                }
                None => {
                    if self.detected_agents.remove(&pane).is_some() {
                        // The agent process is gone. Drop any lingering hook
                        // status for this pane as well, so a missed exit hook
                        // can't leave a permanent row.
                        let had_hook = self.hook_status(pane).is_some();
                        if had_hook {
                            if let Some(lifecycle) = self.agent_lifecycles.get_mut(&pane) {
                                lifecycle.status = None;
                            }
                        }
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Advance the working-agent spinner one frame. Returns true when at least
    /// one pane is currently working, i.e. when the animation is actually
    /// visible and the caller should repaint. Idle sidebars cost nothing.
    pub fn advance_spinner(&mut self) -> bool {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        let panes: Vec<PaneId> = self
            .agent_lifecycles
            .keys()
            .chain(self.detected_agents.keys())
            .copied()
            .collect();
        panes.into_iter().any(|pane| {
            self.agent_status(pane)
                .map(|status| status.display_state() == lumux_core::agent::AgentState::Working)
                .unwrap_or(false)
        })
    }

    /// Open a context menu for `client_id` anchored at the click, clamped so the
    /// popup always fits on screen.
    pub(crate) fn open_context_menu(
        &mut self,
        client_id: u64,
        session: SessionId,
        target: MenuTarget,
        col: u16,
        row: u16,
    ) {
        let items = ContextMenu::items_for(target);
        if items.is_empty() {
            return;
        }
        let mut menu = ContextMenu {
            target,
            items,
            origin: (col, row),
            hover: None,
        };
        if let Some(size) = self.server.effective_size(session) {
            // Keep the whole popup on screen: shift left/up as needed rather
            // than clipping, so no item becomes unreachable.
            let max_x = size.cols.saturating_sub(menu.width());
            let max_y = size.rows.saturating_sub(1).saturating_sub(menu.height());
            menu.origin = (col.min(max_x), row.min(max_y));
        }
        self.menus.insert(client_id, menu);
        self.invalidate_client(client_id);
    }

    /// Close any open context menu. Returns true if one was open.
    pub(crate) fn close_context_menu(&mut self, client_id: u64) -> bool {
        let had = self.menus.remove(&client_id).is_some();
        if had {
            self.invalidate_client(client_id);
        }
        had
    }

    /// Capture the open menu's geometry for the interaction map.
    fn menu_frame(&self, client_id: u64) -> Option<MenuFrame> {
        let menu = self.menus.get(&client_id)?;
        Some(MenuFrame {
            origin: menu.origin,
            width: menu.width(),
            items: menu.items.clone(),
        })
    }

    /// Point the menu's highlight at whatever is under the pointer. Returns
    /// true when the highlight moved, so the caller repaints only on change.
    pub(crate) fn set_menu_hover(&mut self, client_id: u64, col: u16, row: u16) -> bool {
        let Some(menu) = self.menus.get_mut(&client_id) else {
            return false;
        };
        let index = menu
            .item_at(col, row)
            .and_then(|action| menu.items.iter().position(|item| *item == action));
        if menu.hover == index {
            return false;
        }
        menu.hover = index;
        self.invalidate_client(client_id);
        true
    }

    pub(crate) fn context_menu(&self, client_id: u64) -> Option<&ContextMenu> {
        self.menus.get(&client_id)
    }

    /// Paint the context menu popup over the composed frame.
    fn render_context_menu(&self, screen: &mut lumux_core::render::Screen, client_id: u64) {
        let Some(menu) = self.menus.get(&client_id) else {
            return;
        };
        let (x, y) = menu.origin;
        let (w, h) = (menu.width(), menu.height());
        let frame = Self::styled("fg=colour250,bg=colour238");
        let item = Self::styled("fg=colour252,bg=colour236");
        // Border box.
        for row in y..y + h {
            for col in x..x + w {
                screen.set_cell(
                    col as usize,
                    row as usize,
                    lumux_core::render::Cell::new(' ', frame.clone()),
                );
            }
        }
        let hovered = Self::styled("fg=colour231,bg=colour24,bold");
        for (index, action) in menu.items.iter().enumerate() {
            let row = y as usize + 1 + index;
            let style = if menu.hover == Some(index) {
                &hovered
            } else {
                &item
            };
            for col in x..x + w {
                screen.set_cell(
                    col as usize,
                    row,
                    lumux_core::render::Cell::new(' ', style.clone()),
                );
            }
            screen.write_str_clipped(
                x as usize + 2,
                row,
                action.label(),
                style,
                w.saturating_sub(3) as usize,
            );
        }
    }

    /// The hook-reported status for a pane, ignoring process detection.
    fn hook_status(&self, pane: PaneId) -> Option<&lumux_core::agent::AgentStatus> {
        self.agent_lifecycles
            .get(&pane)
            .and_then(|lifecycle| lifecycle.status.as_ref())
    }

    /// Whether dead panes are kept on screen (tmux remain-on-exit).
    pub fn remain_on_exit(&self) -> bool {
        self.config.remain_on_exit
    }

    /// Whether session persistence (save/restore to disk) is enabled.
    pub fn persist_enabled(&self) -> bool {
        self.config.persist
    }

    /// The command line registered for `event` via set-hook, if any (tmux hooks).
    pub fn hook_command(&self, event: &str) -> Option<String> {
        self.config.hooks.get(event).cloned()
    }

    /// Handle a pane's child exiting. With remain-on-exit OFF this is the normal
    /// cascade-close ([`close_pane`]). With it ON, the pane is marked dead but
    /// KEPT (its last screen stays visible) so it can be inspected or respawned;
    /// returns `None` to signal "no close happened" so the caller leaves the
    /// pane->session mapping and window/session intact.
    pub fn pane_exited(&mut self, session: SessionId, pane: PaneId) -> Option<CascadeResult> {
        // Process death ends any agent lifecycle even when remain-on-exit keeps
        // the dead pane's screen/model entry for inspection.
        self.agent_lifecycles.remove(&pane);
        if self.config.remain_on_exit {
            if let Some(p) = self.panes.get_mut(&pane) {
                p.dead = true;
                // Rendering stays owned by the event loop, which knows the
                // affected session. A normal damage diff is enough to show the
                // dead marker and cannot disturb unrelated terminals.
                return None;
            }
        }
        Some(self.close_pane(session, pane))
    }

    /// Whether `pane` is a kept-dead pane (remain-on-exit). Used to gate respawn.
    pub fn is_pane_dead(&self, pane: PaneId) -> bool {
        self.panes.get(&pane).is_some_and(|p| p.dead)
    }

    /// Respawn a dead pane's shell in place (tmux respawn-pane), reusing the same
    /// pane id (and thus its slot in the layout) so the grid is replaced by a
    /// fresh one. Returns the new PTY reader for the event loop to pump, or None
    /// if the pane isn't dead / doesn't exist. The shell argv is the pane's
    /// original one (from the model).
    pub fn respawn_pane(
        &mut self,
        session: SessionId,
        pane: PaneId,
        size: PtySize,
    ) -> std::io::Result<Option<<S::Pty as Pty>::Reader>> {
        if !self.is_pane_dead(pane) {
            return Ok(None);
        }
        // Recover the pane's shell argv from the model.
        let shell = self
            .server
            .session(session)
            .and_then(|s| {
                s.window_ids().into_iter().find_map(|wid| {
                    s.window(wid)
                        .and_then(|w| w.pane(pane).map(|p| p.shell.clone()))
                })
            })
            .unwrap_or_else(|| self.config.shell_argv(None).unwrap_or_default());
        let reader = self.spawn_pane(pane, &shell, size, None)?;
        // The caller invalidates only clients attached to this session.
        Ok(Some(reader))
    }

    /// Kill the active window of `session` (tmux `kill-window`): drop all its
    /// panes' live PTYs and remove the window from the model. Returns the closed
    /// pane ids (so the event loop can drop their pane->session mappings) and the
    /// cascade result (emptying the session closes it).
    pub fn close_active_window(&mut self, session: SessionId) -> (Vec<PaneId>, CascadeResult) {
        let Some(wid) = self.server.session(session).map(|s| s.active_window()) else {
            return (Vec::new(), CascadeResult::NotFound);
        };
        let (panes, result) = self.server.kill_window(session, wid);
        for pid in &panes {
            if let Some(p) = self.panes.get_mut(pid) {
                p.dead = true;
            }
            self.panes.remove(pid);
            self.agent_lifecycles.remove(pid);
        }
        if result == CascadeResult::SessionClosed {
            self.clear_session_transients(session);
        }
        (panes, result)
    }

    /// Forget daemon-owned state whose lifetime is exactly one session. Every
    /// closure path (explicit kill, last-pane cascade, last-window cascade)
    /// funnels through this helper so a model session cannot disappear while
    /// its sidebar overrides or marked pane remain retained.
    fn clear_session_transients(&mut self, session: SessionId) {
        self.sidebar_on.remove(&session);
        self.sidebar_collapsed.remove(&session);
        if self
            .marked_pane
            .is_some_and(|(marked_session, _)| marked_session == session)
        {
            self.marked_pane = None;
        }
    }

    /// Close a whole session and all of its live/transient pane state. Keeping
    /// this cleanup behind one interface prevents kill-session from bypassing
    /// agent lifecycle cleanup.
    pub fn close_session(&mut self, session: SessionId) -> Vec<PaneId> {
        let panes = self
            .server
            .session(session)
            .map(|session| {
                session
                    .window_ids()
                    .into_iter()
                    .flat_map(|window| {
                        session
                            .window(window)
                            .map(|window| window.pane_ids())
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for pane in &panes {
            self.panes.remove(pane);
            self.agent_lifecycles.remove(pane);
        }
        self.clear_session_transients(session);
        self.server.kill_session(session);
        panes
    }

    /// Content viewport for `session`: the pane area to the right of any sidebar
    /// and above the status row. This is the single authority for the content
    /// plane's origin and extent — every pane hit-test, divider drag, and the
    /// compositor derive from it, so the sidebar offset (and the reserved status
    /// row) live in exactly one place.
    pub fn content_viewport(&self, session: SessionId) -> Option<lumux_core::layout::Rect> {
        let size = self.server.effective_size(session)?;
        Some(self.content_viewport_for_size(session, size))
    }

    /// The same content-plane authority for lifecycle points that already have
    /// the effective outer size (attach/resize/config reflow).
    fn content_viewport_for_size(
        &self,
        session: SessionId,
        size: PtySize,
    ) -> lumux_core::layout::Rect {
        let sidebar = self.sidebar_width(session).min(size.cols);
        lumux_core::layout::Rect::new(sidebar, 0, size.cols - sidebar, size.rows.saturating_sub(1))
    }

    /// Width of the thin collapsed rail (just wide enough for the toggle glyph
    /// plus its border).
    pub const SIDEBAR_RAIL_WIDTH: u16 = 3;

    /// Columns reserved on the left for the sessions/agents sidebar for this
    /// session: 0 when off, a thin rail when collapsed, else the configured width
    /// (clamped so it can never swallow more than half the screen).
    pub fn sidebar_width(&self, session: SessionId) -> u16 {
        if !self.sidebar_visible(session) {
            return 0;
        }
        let cols = self
            .server
            .effective_size(session)
            .map(|s| s.cols)
            .unwrap_or(0);
        let max = cols / 2;
        let desired = if self.sidebar_collapsed(session) {
            Self::SIDEBAR_RAIL_WIDTH
        } else {
            self.config.sidebar_width.max(Self::SIDEBAR_RAIL_WIDTH)
        };
        // At extremely narrow sizes the half-screen invariant wins over the
        // normal three-column rail minimum. This keeps at least half of the
        // terminal available to content and never returns more columns than
        // physically exist.
        desired.min(max)
    }

    /// Whether the sidebar is shown for `session`: the per-session override if
    /// set, else the config default. Session-global (all clients of the session
    /// share it), which is the only coherent scope under the shared PTY.
    pub fn sidebar_visible(&self, session: SessionId) -> bool {
        self.sidebar_on
            .get(&session)
            .copied()
            .unwrap_or(self.config.sidebar)
    }

    /// Whether the (shown) sidebar is collapsed to its thin rail. Expanded by
    /// default.
    pub fn sidebar_collapsed(&self, session: SessionId) -> bool {
        self.sidebar_collapsed
            .get(&session)
            .copied()
            .unwrap_or(false)
    }

    /// Toggle (or set) the sidebar for `session`. Returns the new visibility.
    /// The caller must resize the session afterward so the PTYs reflow to the
    /// new content width, and invalidate renderers for a clean repaint.
    pub fn set_sidebar_visible(&mut self, session: SessionId, on: bool) {
        self.sidebar_on.insert(session, on);
    }

    /// Collapse/expand the (shown) sidebar for `session`. Caller reflows after.
    pub fn set_sidebar_collapsed(&mut self, session: SessionId, collapsed: bool) {
        self.sidebar_collapsed.insert(session, collapsed);
    }

    /// Scroll the sidebar section under `row`. Returns true when the visible
    /// slice changed. Wheel events in sidebar columns are consumed even when
    /// this returns false, so they never open pane copy mode behind the panel.
    pub fn scroll_sidebar(
        &mut self,
        client_id: u64,
        session: SessionId,
        row: u16,
        height: usize,
        up: bool,
    ) -> bool {
        if self.sidebar_collapsed(session) {
            return false;
        }
        let content_h = height.saturating_sub(1);
        let projection = self.sidebar_projection(client_id, session, content_h);
        let Some(section) = projection.section_at(row as usize) else {
            return false;
        };
        self.scroll_sidebar_section(client_id, session, section, height, up)
    }

    /// Scroll a section identified by an already-rendered sidebar frame. This is
    /// the epoch-safe counterpart to [`Daemon::scroll_sidebar`]: row geometry is
    /// resolved from the applied frame, while clamping uses the latest content so
    /// removed entries can never leave an invalid offset behind.
    pub(crate) fn scroll_sidebar_section(
        &mut self,
        client_id: u64,
        session: SessionId,
        section: SidebarSectionKind,
        height: usize,
        up: bool,
    ) -> bool {
        let content_h = height.saturating_sub(1);
        let step = 3usize;
        let projection = self.sidebar_projection(client_id, session, content_h);
        let current = projection.current_offset(section);
        let next = projection.scrolled_offset(section, up, step);
        if next == current {
            return false;
        }
        *self
            .sidebar_scroll
            .entry(client_id)
            .or_default()
            .offset_mut(section) = next;
        true
    }

    /// Bring `session` into the session section for `client_id` without
    /// disturbing that client's independently scrolled agent section. Session
    /// switches call this once; ordinary renders do not, so users can still
    /// scroll away from the current row while browsing.
    pub fn ensure_sidebar_session_visible(&mut self, client_id: u64, session: SessionId) -> bool {
        let Some(size) = self.server.effective_size(session) else {
            return false;
        };
        let content_h = (size.rows as usize).saturating_sub(1);
        let projection = self.sidebar_projection(client_id, session, content_h);
        let current = projection.current_offset(SidebarSectionKind::Sessions);
        let Some(next) = projection.session_offset_revealing(session) else {
            return false;
        };
        if next == current {
            return false;
        }
        self.sidebar_scroll.entry(client_id).or_default().sessions = next;
        true
    }

    /// Mouse-press: if (col,row) is on a split divider, remember it as the
    /// grabbed divider for `client_id` so a following drag resizes it. A press in
    /// open pane area records nothing, so plain click-drags don't resize.
    pub fn begin_drag(&mut self, client_id: u64, session: SessionId, col: u16, row: u16) {
        self.dragging.remove(&client_id);
        let Some(vp) = self.content_viewport(session) else {
            return;
        };
        let Some((window, layout, path)) = self.server.session(session).and_then(|s| {
            let window = s.active_window();
            let layout = visible_layout(s.window(window)?);
            let path = lumux_core::layout::divider_at(&layout, col, row, vp)?;
            Some((window, layout, path))
        }) else {
            return;
        };
        self.dragging.insert(
            client_id,
            DividerDrag {
                session,
                window,
                viewport: vp,
                layout,
                path,
            },
        );
    }

    /// Epoch-safe divider press. The rendered split path is accepted only while
    /// the same window, viewport, and complete visible topology still exist.
    pub(crate) fn begin_drag_in_frame(
        &mut self,
        client_id: u64,
        divider: Option<InteractionDivider>,
    ) {
        self.dragging.remove(&client_id);
        let Some(divider) = divider else {
            return;
        };
        let current_matches = self.server.session(divider.session).is_some_and(|session| {
            session.active_window() == divider.window
                && session
                    .window(divider.window)
                    .is_some_and(|window| visible_layout(window) == divider.layout)
        }) && self.content_viewport(divider.session)
            == Some(divider.viewport);
        if current_matches {
            self.dragging.insert(
                client_id,
                DividerDrag {
                    session: divider.session,
                    window: divider.window,
                    viewport: divider.viewport,
                    layout: divider.layout,
                    path: divider.path,
                },
            );
        }
    }

    /// Mouse-drag: move the divider grabbed on press to follow the cursor, and
    /// re-fit the PTYs. No-op (returns false) if this client didn't grab one.
    pub fn drag_divider(&mut self, client_id: u64, session: SessionId, col: u16, row: u16) -> bool {
        let Some(grabbed) = self.dragging.get(&client_id).cloned() else {
            return false;
        };
        if grabbed.session != session || self.content_viewport(session) != Some(grabbed.viewport) {
            self.dragging.remove(&client_id);
            return false;
        }
        let topology_matches = self.server.session(session).is_some_and(|current| {
            current.active_window() == grabbed.window
                && current
                    .window(grabbed.window)
                    .is_some_and(|window| visible_layout(window) == grabbed.layout)
        });
        if !topology_matches {
            self.dragging.remove(&client_id);
            return false;
        }
        let moved = self
            .server
            .session_mut(session)
            .and_then(|s| {
                s.window_mut(grabbed.window)
                    .map(|w| w.drag_divider(&grabbed.path, col, row, grabbed.viewport))
            })
            .unwrap_or(false);
        if moved {
            if let Some(size) = self.server.effective_size(session) {
                self.resize_session(session, size);
            }
            if let Some(layout) = self
                .server
                .session(session)
                .and_then(|session| session.window(grabbed.window))
                .map(visible_layout)
            {
                if let Some(current) = self.dragging.get_mut(&client_id) {
                    current.layout = layout;
                }
            }
        }
        moved
    }

    /// Cancel client-owned mouse gestures without applying their pending
    /// divider or selection action. Used when an Up belongs to an unknown frame.
    pub(crate) fn cancel_mouse_gestures(&mut self, client_id: u64) {
        self.dragging.remove(&client_id);
        if matches!(self.mouse_sel.get(&client_id), Some(MouseSel::Dragging { .. })) {
            // A promoted mouse selection created (or took ownership of) the
            // client's CopySession and copy keymap. Cancelling only MouseSel
            // would strand the client in copy mode after the physical release.
            self.exit_copy_mode(client_id);
        } else {
            self.mouse_sel.remove(&client_id);
        }
    }

    /// Whether this client currently has a divider grabbed (press landed on a
    /// split border). Used so a drag that started on a divider resizes rather
    /// than starting a text selection.
    pub fn is_dragging_divider(&self, client_id: u64) -> bool {
        self.dragging.contains_key(&client_id)
    }

    /// Mouse-press over a selectable pane: arm a possible text selection at
    /// (col,row). Promoted to a live selection on the first drag motion (tmux
    /// starts selecting on move, so a plain click never flickers copy-mode).
    /// No-op on the status row or the alternate screen (nothing to select).
    pub fn mouse_sel_arm(&mut self, client_id: u64, session: SessionId, col: u16, row: u16) {
        self.mouse_sel.remove(&client_id);
        let Some((pane, rect)) = self.pane_and_rect_at_screen(session, col, row) else {
            return;
        };
        let Some(window) = self
            .server
            .session(session)
            .map(|session| session.active_window())
        else {
            return;
        };
        let target = InteractionPane {
            session,
            window,
            pane,
            rect,
            wants_mouse: self.pane_wants_mouse(pane),
            alt_screen: self.pane_on_alt_screen(pane),
        };
        let copy_top = self.copy.get(&client_id).and_then(|copy| {
            ((copy.session, copy.window, copy.pane) == (session, window, pane)).then(|| copy.top())
        });
        self.mouse_sel_arm_target(client_id, target, copy_top, col, row);
    }

    pub(crate) fn mouse_sel_arm_in_frame(
        &mut self,
        client_id: u64,
        interactions: &InteractionMap,
        col: u16,
        row: u16,
    ) {
        self.mouse_sel.remove(&client_id);
        if let Some(target) = interactions.pane_at(col, row) {
            let copy_top = interactions.copy_top_for(target);
            self.mouse_sel_arm_target(client_id, target, copy_top, col, row);
        }
    }

    fn mouse_sel_arm_target(
        &mut self,
        client_id: u64,
        target: InteractionPane,
        copy_top: Option<usize>,
        col: u16,
        row: u16,
    ) {
        if target.alt_screen || target.wants_mouse || !self.interaction_pane_modes_match(target) {
            return;
        }
        let origin_col = col
            .saturating_sub(target.rect.x)
            .min(target.rect.cols.saturating_sub(1));
        let origin_row = row
            .saturating_sub(target.rect.y)
            .min(target.rect.rows.saturating_sub(1));
        self.mouse_sel.insert(
            client_id,
            MouseSel::Armed {
                session: target.session,
                window: target.window,
                pane: target.pane,
                origin_col,
                origin_row,
                origin_top: copy_top,
            },
        );
    }

    /// Mouse-drag while a selection is armed/active. The first motion enters
    /// copy-mode, anchors the selection at the press cell, and extends it to the
    /// current cell; later motions just extend. Returns true when a text
    /// selection is live (so the caller skips divider-drag and repaints).
    pub fn mouse_sel_drag(
        &mut self,
        client_id: u64,
        session: SessionId,
        col: u16,
        row: u16,
    ) -> bool {
        let Some((state_session, window, pane)) = self.mouse_sel_identity(client_id) else {
            return false;
        };
        if state_session != session {
            self.mouse_sel.remove(&client_id);
            return true;
        }
        let target = self.current_interaction_pane(state_session, window, pane);
        let copy_top = target.and_then(|target| {
            self.copy.get(&client_id).and_then(|copy| {
                ((copy.session, copy.window, copy.pane)
                    == (target.session, target.window, target.pane))
                    .then(|| copy.top())
            })
        });
        self.mouse_sel_drag_target(client_id, target, copy_top, col, row)
    }

    pub(crate) fn mouse_sel_drag_in_frame(
        &mut self,
        client_id: u64,
        interactions: &InteractionMap,
        col: u16,
        row: u16,
    ) -> bool {
        let Some((session, window, pane)) = self.mouse_sel_identity(client_id) else {
            return false;
        };
        let target = (session == interactions.session)
            .then(|| interactions.pane(pane))
            .flatten()
            .filter(|target| target.window == window);
        let copy_top = target.and_then(|target| interactions.copy_top_for(target));
        self.mouse_sel_drag_target(client_id, target, copy_top, col, row)
    }

    fn mouse_sel_identity(&self, client_id: u64) -> Option<(SessionId, WindowId, PaneId)> {
        match self.mouse_sel.get(&client_id).copied()? {
            MouseSel::Armed {
                session,
                window,
                pane,
                ..
            }
            | MouseSel::Dragging {
                session,
                window,
                pane,
            } => Some((session, window, pane)),
        }
    }

    fn current_interaction_pane(
        &self,
        session: SessionId,
        window: WindowId,
        pane: PaneId,
    ) -> Option<InteractionPane> {
        let viewport = self.content_viewport(session)?;
        let current = self.server.session(session)?;
        if current.active_window() != window {
            return None;
        }
        let current_window = current.window(window)?;
        if current_window.active_pane() != pane {
            return None;
        }
        let rect =
            *lumux_core::layout::compute(&visible_layout(current_window), viewport).get(&pane)?;
        Some(InteractionPane {
            session,
            window,
            pane,
            rect,
            wants_mouse: self.pane_wants_mouse(pane),
            alt_screen: self.pane_on_alt_screen(pane),
        })
    }

    fn mouse_sel_drag_target(
        &mut self,
        client_id: u64,
        target: Option<InteractionPane>,
        frame_copy_top: Option<usize>,
        col: u16,
        row: u16,
    ) -> bool {
        let Some(state) = self.mouse_sel.get(&client_id).copied() else {
            return false;
        };
        let Some(target) = target
            .filter(|target| !target.alt_screen && self.interaction_pane_modes_match(*target))
        else {
            self.mouse_sel.remove(&client_id);
            return true;
        };
        match state {
            MouseSel::Armed {
                session,
                window,
                pane,
                origin_col,
                origin_row,
                origin_top,
            } => {
                if (session, window, pane) != (target.session, target.window, target.pane) {
                    self.mouse_sel.remove(&client_id);
                    return true;
                }
                let active_matches = self.server.session(session).is_some_and(|current| {
                    current.active_window() == window
                        && current
                            .window(window)
                            .is_some_and(|current| current.active_pane() == pane)
                });
                if !active_matches {
                    self.mouse_sel.remove(&client_id);
                    return true;
                }
                if !self.in_copy_mode(client_id) {
                    self.enter_copy_mode(client_id, session);
                }
                let Some(cm) = self.copy.get_mut(&client_id) else {
                    self.mouse_sel.remove(&client_id);
                    return true;
                };
                let live_top = cm.top();
                let origin = lumux_core::copymode::Pos {
                    row: origin_top.unwrap_or(live_top) + origin_row as usize,
                    col: origin_col as usize,
                };
                let cur = Self::point_to_buffer(
                    frame_copy_top.unwrap_or(live_top),
                    target.rect,
                    col,
                    row,
                );
                if let Some(grid) = self.panes.get(&pane).map(|pane| &pane.grid) {
                    cm.set_cursor(origin, grid);
                    cm.start_selection();
                    cm.set_cursor(cur, grid);
                }
                self.mouse_sel.insert(
                    client_id,
                    MouseSel::Dragging {
                        session,
                        window,
                        pane,
                    },
                );
                if let Some(renderer) = self.renderers.get_mut(&client_id) {
                    renderer.invalidate();
                }
                true
            }
            MouseSel::Dragging {
                session,
                window,
                pane,
            } => {
                if (session, window, pane) != (target.session, target.window, target.pane) {
                    self.mouse_sel.remove(&client_id);
                    return true;
                }
                if let Some(cm) = self.copy.get_mut(&client_id) {
                    let cur = Self::point_to_buffer(
                        frame_copy_top.unwrap_or_else(|| cm.top()),
                        target.rect,
                        col,
                        row,
                    );
                    if let Some(grid) = self.panes.get(&pane).map(|pane| &pane.grid) {
                        cm.set_cursor(cur, grid);
                    }
                }
                true
            }
        }
    }

    fn point_to_buffer(
        top: usize,
        rect: lumux_core::layout::Rect,
        col: u16,
        row: u16,
    ) -> lumux_core::copymode::Pos {
        let rel_col = col.saturating_sub(rect.x).min(rect.cols.saturating_sub(1)) as usize;
        let rel_row = row.saturating_sub(rect.y).min(rect.rows.saturating_sub(1)) as usize;
        lumux_core::copymode::Pos {
            row: top + rel_row,
            col: rel_col,
        }
    }

    /// Whether a mouse text-selection drag is currently in progress.
    pub fn mouse_sel_active(&self, client_id: u64) -> bool {
        matches!(
            self.mouse_sel.get(&client_id),
            Some(MouseSel::Dragging { .. })
        )
    }

    /// Whether a mouse selection is armed or in progress (press seen, not yet
    /// released). The event loop uses this to hold a trailing partial mouse
    /// introducer across a read split, so a release SGR sequence chopped by an
    /// SSH/mosh read boundary still reassembles instead of being dropped.
    pub fn mouse_sel_pending(&self, client_id: u64) -> bool {
        self.mouse_sel.contains_key(&client_id)
    }

    /// Mouse-release: finish a text-selection drag by yanking the selection
    /// (which also exits copy-mode) and returning the copied text, if any. A drag
    /// that never moved (still `Armed`) copies nothing.
    pub fn mouse_sel_finish(&mut self, client_id: u64) -> Option<String> {
        match self.mouse_sel.remove(&client_id) {
            Some(MouseSel::Dragging {
                session,
                window,
                pane,
            }) => {
                let exact_target_is_active = self.server.session(session).is_some_and(|current| {
                    current.active_window() == window
                        && current
                            .window(window)
                            .is_some_and(|current| current.active_pane() == pane)
                        && self.panes.contains_key(&pane)
                });
                if exact_target_is_active {
                    self.copy_yank(client_id, session)
                } else {
                    self.exit_copy_mode(client_id);
                    None
                }
            }
            _ => None,
        }
    }

    /// The visible pane and rectangle at a screen point, or None on session
    /// chrome/outside the content plane. This is the single pane hit-test seam:
    /// callers cannot accidentally target a leaf hidden by zoom or translate
    /// app-mouse coordinates against the retained split tree.
    pub fn pane_and_rect_at_screen(
        &self,
        session: SessionId,
        col: u16,
        row: u16,
    ) -> Option<(PaneId, lumux_core::layout::Rect)> {
        let viewport = self.content_viewport(session)?;
        if !viewport.contains_point(col, row) {
            return None;
        }
        let session = self.server.session(session)?;
        let window = session.window(session.active_window())?;
        let rects = lumux_core::layout::compute(&visible_layout(window), viewport);
        let pane = lumux_core::layout::pane_at(&rects, col, row)?;
        Some((pane, *rects.get(&pane)?))
    }

    /// The pane id at a screen point within the content area.
    pub fn pane_at_screen(&self, session: SessionId, col: u16, row: u16) -> Option<PaneId> {
        self.pane_and_rect_at_screen(session, col, row)
            .map(|(pane, _)| pane)
    }

    /// Revalidate the stable identity portion of a rendered pane target. Its
    /// rectangle and mode flags intentionally remain historical; only current
    /// model membership controls whether it is still safe to mutate the pane.
    pub(crate) fn interaction_pane_is_current(&self, target: InteractionPane) -> bool {
        self.server
            .session(target.session)
            .and_then(|session| session.window(target.window))
            .is_some_and(|window| window.pane_ids().contains(&target.pane))
            && self.panes.contains_key(&target.pane)
    }

    pub(crate) fn interaction_pane_is_active(&self, target: InteractionPane) -> bool {
        self.interaction_pane_is_current(target)
            && self.server.session(target.session).is_some_and(|session| {
                session.active_window() == target.window
                    && session
                        .window(target.window)
                        .is_some_and(|window| window.active_pane() == target.pane)
            })
    }

    pub(crate) fn interaction_pane_modes_match(&self, target: InteractionPane) -> bool {
        self.interaction_pane_is_current(target)
            && self.pane_wants_mouse(target.pane) == target.wants_mouse
            && self.pane_on_alt_screen(target.pane) == target.alt_screen
    }

    /// Whether a modal surface owns the whole screen. Mouse input must not fall
    /// through to invisible pane/sidebar targets while one is open. Copy mode
    /// is intentionally excluded because it keeps the persistent sidebar.
    pub fn full_screen_overlay_active(&self, client_id: u64) -> bool {
        self.clock.contains(&client_id)
            || self.help.contains(&client_id)
            || self.choosing.contains_key(&client_id)
            || self.choosing_buffer.contains_key(&client_id)
    }

    /// Compatibility wrapper for daemon-level tests that only inspect VT. The
    /// event loop uses [`Self::render_client_frame`] so it cannot publish pixels
    /// without the interaction map produced by the same render operation.
    pub fn render_for_client(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        self.render_client_frame(client_id, session)
            .map(|frame| frame.bytes)
    }

    /// Render one client and capture the immutable interaction projection that
    /// belongs to those exact pixels.
    pub(crate) fn render_client_frame(
        &mut self,
        client_id: u64,
        session: SessionId,
    ) -> Option<RenderedClientFrame> {
        // A projection clamps offsets to the current item counts. Persist that
        // normalized viewport at the render seam so shrink-then-regrow cannot
        // resurrect an old offset that was only transiently clamped on screen.
        if let Some(size) = self.server.effective_size(session) {
            self.normalize_sidebar_scroll(client_id, session, size.rows as usize);
        }
        // The clock overlay takes over the whole screen when active.
        if self.clock.contains(&client_id) {
            let bytes = self.render_clock(client_id, session)?;
            let interactions = self.interaction_map_for_client(client_id, session, None)?;
            return Some(RenderedClientFrame {
                bytes,
                interactions,
            });
        }
        // The help overlay takes over the whole screen when active.
        if self.help.contains(&client_id) {
            let bytes = self.render_help(client_id, session)?;
            let interactions = self.interaction_map_for_client(client_id, session, None)?;
            return Some(RenderedClientFrame {
                bytes,
                interactions,
            });
        }
        // The session switcher likewise takes over the screen.
        if self.choosing.contains_key(&client_id) {
            let bytes = self.render_chooser(client_id, session)?;
            let interactions = self.interaction_map_for_client(client_id, session, None)?;
            return Some(RenderedClientFrame {
                bytes,
                interactions,
            });
        }
        // The paste-buffer chooser is a full-screen overlay too.
        if self.choosing_buffer.contains_key(&client_id) {
            let bytes = self.render_buffer_chooser(client_id, session)?;
            let interactions = self.interaction_map_for_client(client_id, session, None)?;
            return Some(RenderedClientFrame {
                bytes,
                interactions,
            });
        }
        // Copy-mode clients see the scrolled history view instead of live panes.
        if self.copy.contains_key(&client_id) {
            if self.copy_target_is_active(client_id, session) {
                let bytes = self.render_copy_mode(client_id, session)?;
                let interactions = self.interaction_map_for_client(client_id, session, None)?;
                return Some(RenderedClientFrame {
                    bytes,
                    interactions,
                });
            }
            self.exit_copy_mode(client_id);
        }
        let size = self.server.effective_size(session)?;
        let s = self.server.session(session)?;
        let window = s.window(s.active_window())?;
        // When a pane is zoomed (tmux prefix z), render only that pane fullscreen
        // by swapping in a single-leaf layout; otherwise use the real split tree.
        let layout = visible_layout(window);
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
            active_border: self.active_border_attrs(),
            inactive_border: self.inactive_border_attrs(),
        };
        // Compose panes without a built-in status row, but reserve the bottom
        // row so panes don't extend into it; we then paint our own styled status
        // (or a transient message) onto that reserved row. Panes start to the
        // right of the sidebar (0 when it's off).
        let sidebar_w = self.sidebar_width(session) as usize;
        let mut screen = compose(
            (size.cols as usize, size.rows as usize),
            &view,
            None,
            true,
            sidebar_w,
        );
        // Display-panes overlay (tmux prefix q): draw each pane's number centered
        // in its rect, on top of the composed panes.
        if self.showing_panes.contains(&client_id) {
            self.overlay_pane_numbers(&mut screen, session, &layout, size);
        }
        // Paint the sessions/agents sidebar into the reserved left columns.
        if sidebar_w > 0 {
            self.render_sidebar(
                &mut screen,
                client_id,
                session,
                sidebar_w as u16,
                size.rows as usize,
            );
        }
        let status = self.paint_status(&mut screen, client_id, session);
        // The context menu floats above everything else it overlaps.
        self.render_context_menu(&mut screen, client_id);
        let renderer = self.renderers.get_mut(&client_id)?;
        let bytes = renderer.render(screen);
        let interactions = self.interaction_map_for_client(client_id, session, Some(status))?;
        Some(RenderedClientFrame {
            bytes,
            interactions,
        })
    }

    fn interaction_map_for_client(
        &self,
        client_id: u64,
        session: SessionId,
        status: Option<StatusFrame>,
    ) -> Option<InteractionMap> {
        let size = self.server.effective_size(session)?;
        let modal = self.full_screen_overlay_active(client_id);
        let empty_status = StatusFrame {
            row: size.rows.saturating_sub(1),
            hits: Vec::new(),
        };
        if modal {
            return Some(InteractionMap {
                session,
                size,
                modal: true,
                copy_mode: false,
                copy_pane: None,
                copy_top: None,
                sidebar: None,
                window: None,
                viewport: None,
                layout: None,
                panes: Vec::new(),
                status: empty_status,
                menu: self.menu_frame(client_id),
            });
        }

        let current = self.server.session(session)?;
        let window_id = current.active_window();
        let window = current.window(window_id)?;
        let layout = visible_layout(window);
        let viewport = self.content_viewport_for_size(session, size);
        let panes = lumux_core::layout::compute(&layout, viewport)
            .into_iter()
            .map(|(pane, rect)| InteractionPane {
                session,
                window: window_id,
                pane,
                rect,
                wants_mouse: self.pane_wants_mouse(pane),
                alt_screen: self.pane_on_alt_screen(pane),
            })
            .collect();
        Some(InteractionMap {
            session,
            size,
            modal: false,
            copy_mode: self.copy.contains_key(&client_id),
            copy_pane: self.copy.get(&client_id).map(|copy| copy.pane),
            copy_top: self.copy.get(&client_id).map(|copy| copy.top()),
            sidebar: self.sidebar_frame_for_client(client_id, session),
            window: Some(window_id),
            viewport: Some(viewport),
            layout: Some(layout),
            panes,
            status: status.unwrap_or(empty_status),
            menu: self.menu_frame(client_id),
        })
    }

    /// Capture the exact sidebar identities and geometry represented by the most
    /// recently composed screen. The event loop stores this beside its frame
    /// epoch; callers never rebuild a hit map from newer model state.
    pub(crate) fn sidebar_frame_for_client(
        &self,
        client_id: u64,
        session: SessionId,
    ) -> Option<SidebarFrame> {
        if self.full_screen_overlay_active(client_id) {
            return None;
        }
        let size = self.server.effective_size(session)?;
        let width = self.sidebar_width(session);
        if width == 0 {
            return None;
        }
        let content_height = (size.rows as usize).saturating_sub(1);
        let collapsed = self.sidebar_collapsed(session) || width == 1;
        let projection = self.sidebar_projection(client_id, session, content_height);
        Some(SidebarFrame::new(
            session,
            width,
            content_height,
            collapsed,
            &projection,
        ))
    }

    fn normalize_sidebar_scroll(
        &mut self,
        client_id: u64,
        session: SessionId,
        total_height: usize,
    ) {
        let requested = self
            .sidebar_scroll
            .get(&client_id)
            .copied()
            .unwrap_or_default();
        let normalized = self
            .sidebar_projection(client_id, session, total_height.saturating_sub(1))
            .normalized_scroll();
        if requested != normalized {
            self.sidebar_scroll.insert(client_id, normalized);
        }
    }

    /// Paint big pane numbers (1-based, offset by base-index) centered in each
    /// pane's rect for the display-panes overlay. Numbers are reverse-video so
    /// they stay legible over pane content; the active pane is marked with a
    /// trailing `*`.
    fn overlay_pane_numbers(
        &self,
        screen: &mut lumux_core::render::Screen,
        session: SessionId,
        layout: &lumux_core::model::PaneNode,
        size: PtySize,
    ) {
        use lumux_core::render::CellAttributes;
        let Some(s) = self.server.session(session) else {
            return;
        };
        let active = s.window(s.active_window()).map(|w| w.active_pane());
        let viewport = self.content_viewport_for_size(session, size);
        let rects = lumux_core::layout::compute(layout, viewport);
        let base = self.base_index();
        // Reverse-video by default; a configured display-panes-colour tints the
        // number foreground instead (active vs inactive picked per pane below).
        let attrs_for = |is_active: bool| {
            let colour = if is_active {
                &self.config.display_panes_active_colour
            } else {
                &self.config.display_panes_colour
            };
            let mut a = CellAttributes::default();
            if colour.is_empty() {
                a.set_reverse(true);
            } else {
                a.set_foreground(lumux_core::status::parse_color(colour));
            }
            a
        };
        // Number panes by their traversal order so the digits match pick_pane_number.
        for (i, pid) in layout.pane_ids().iter().enumerate() {
            let Some(rect) = rects.get(pid) else { continue };
            let is_active = Some(*pid) == active;
            let label = if is_active {
                format!(" {}* ", base as usize + i)
            } else {
                format!(" {} ", base as usize + i)
            };
            let attrs = attrs_for(is_active);
            let cx = rect.x as usize + (rect.cols as usize / 2).saturating_sub(label.len() / 2);
            let cy = rect.y as usize + rect.rows as usize / 2;
            screen.write_str(cx, cy, &label, &attrs);
        }
    }

    /// Build the one typed projection consumed by sidebar rendering,
    /// hit-testing, scrolling, and ensure-visible behavior. Enumeration order,
    /// section geometry, capacities, and clamped per-client offsets therefore
    /// cannot drift between those interaction paths.
    fn sidebar_projection(
        &self,
        client_id: u64,
        current_session: SessionId,
        content_h: usize,
    ) -> SidebarProjection {
        let scroll = self
            .sidebar_scroll
            .get(&client_id)
            .copied()
            .unwrap_or_default();
        let sessions = self
            .server
            .session_ids()
            .into_iter()
            .filter_map(|sid| {
                let session = self.server.session(sid)?;
                Some(SidebarSessionEntry {
                    sid,
                    name: session.name.clone(),
                    windows: session.window_count(),
                    current: sid == current_session,
                })
            })
            .collect();

        let mut agents = Vec::new();
        for sid in self.server.session_ids() {
            let Some(session) = self.server.session(sid) else {
                continue;
            };
            for wid in session.window_ids() {
                let Some(window) = session.window(wid) else {
                    continue;
                };
                for pid in window.pane_ids() {
                    if let Some(status) = self.agent_status(pid) {
                        agents.push(SidebarAgentEntry {
                            sid,
                            wid,
                            pid,
                            agent: status.agent.clone(),
                            state: status.display_state(),
                            session_name: session.name.clone(),
                        });
                    }
                }
            }
        }

        SidebarProjection::new(content_h, scroll, sessions, agents)
    }

    /// Paint the sessions/agents sidebar into the reserved columns `[0, width)`.
    /// `total_rows` is the full screen height (the status row is left for
    /// `paint_status`). When collapsed the sidebar is a thin rail with just the
    /// expand button; otherwise it's the full two-section list. A themed vertical
    /// border closes the right edge.
    fn render_sidebar(
        &self,
        screen: &mut lumux_core::render::Screen,
        client_id: u64,
        session: SessionId,
        width: u16,
        total_rows: usize,
    ) {
        let w = width as usize;
        if w == 0 {
            return;
        }
        let content_h = total_rows.saturating_sub(1);
        // At a one-column allocation there is no room for both body text and
        // the separating border. Render it as a toggle-only rail instead of
        // letting a session/agent glyph overwrite the border cell.
        let collapsed = self.sidebar_collapsed(session) || w == 1;

        let panel = self.sidebar_panel_attrs();
        let border = self.sidebar_border_attrs();

        // Fill the sidebar background so it reads as a panel, not floating text.
        for y in 0..content_h {
            for x in 0..w {
                screen.set_cell(x, y, lumux_core::render::Cell::new(' ', panel.clone()));
            }
        }
        // Right border down the whole content height.
        if w >= 1 {
            screen.vline(w - 1, 0, content_h, &border);
        }

        if collapsed {
            // Thin rail: just the expand button (▶) at the top.
            let btn = self.sidebar_header_attrs();
            screen.write_str(0, 0, "▶", &btn);
            return;
        }

        let text_w = w.saturating_sub(1); // leave the last column for the border
        let header = self.sidebar_header_attrs();
        let current = self.sidebar_current_attrs();
        let projection = self.sidebar_projection(client_id, session, content_h);
        for (y, row) in projection.rows() {
            match row {
                SidebarProjectedRow::Header(label) => {
                    // Header bar: the label plus a collapse button (◀) right-aligned
                    // on the first header row only.
                    self.fill_row(screen, y, text_w, &header);
                    // Row 0 carries the chrome buttons: `+` (new session) then
                    // `◀` (collapse), right-aligned. The label yields the space.
                    let label_width = if y == 0 {
                        text_w.saturating_sub(if text_w >= NEW_SESSION_BUTTON_SPAN { 3 } else { 1 })
                    } else {
                        text_w
                    };
                    screen.write_str_clipped(0, y, label, &header, label_width);
                    if y == 0 && text_w >= 1 {
                        screen.write_str(text_w - 1, y, "◀", &header);
                        if text_w >= NEW_SESSION_BUTTON_SPAN {
                            screen.write_str(text_w - 3, y, "+", &header);
                        }
                    }
                }
                SidebarProjectedRow::Session(entry) => {
                    let line = format!("{} · {}w", entry.name, entry.windows);
                    if entry.current {
                        self.fill_row(screen, y, text_w, &current);
                        screen.write_str_clipped(0, y, &line, &current, text_w);
                    } else {
                        screen.write_str_clipped(0, y, &line, &panel, text_w);
                    }
                }
                SidebarProjectedRow::Agent(entry) => {
                    let glyph = Self::agent_glyph(entry.state, self.spinner_tick);
                    let gattr = self.agent_glyph_attrs(entry.state, &panel);
                    // "● agent @sess" — the glyph gets a state color, the rest the
                    // panel style.
                    screen.write_str_clipped(0, y, &glyph.to_string(), &gattr, 1);
                    let rest = format!(" {} @{}", entry.agent, entry.session_name);
                    screen.write_str_clipped(1, y, &rest, &panel, text_w.saturating_sub(1));
                }
            }
        }
    }

    /// Fill row `y`'s first `width` columns with a styled blank (for header /
    /// selection bars) without touching the border column.
    fn fill_row(
        &self,
        screen: &mut lumux_core::render::Screen,
        y: usize,
        width: usize,
        attrs: &lumux_core::render::CellAttributes,
    ) {
        for x in 0..width {
            screen.set_cell(x, y, lumux_core::render::Cell::new(' ', attrs.clone()));
        }
    }

    /// A single-char status glyph for the agents list. A filled dot for live
    /// states, distinct shapes for the rest; color carries the meaning (see
    /// `agent_glyph_attrs`) with the shape as a fallback.
    fn agent_glyph(state: lumux_core::agent::AgentState, tick: u64) -> char {
        use lumux_core::agent::AgentState;
        match state {
            // A working agent animates so "busy" reads at a glance without
            // relying on color (matching herdr's braille spinner).
            AgentState::Working => {
                AGENT_SPINNER[(tick as usize) % AGENT_SPINNER.len()]
            }
            AgentState::Blocked => '●',
            AgentState::Done => '✓',
            AgentState::Idle => '○',
            AgentState::Unknown => '·',
        }
    }

    /// Base panel style for the sidebar body — a slightly darker background than
    /// the terminal so the sidebar reads as its own surface, with a soft fg.
    fn sidebar_panel_attrs(&self) -> lumux_core::render::CellAttributes {
        Self::styled("fg=colour250,bg=colour235")
    }

    /// Section-header style: bold accent text on a raised background.
    fn sidebar_header_attrs(&self) -> lumux_core::render::CellAttributes {
        Self::styled("fg=colour81,bg=colour238,bold")
    }

    /// Current-session row style: an accent bar so the active session pops.
    fn sidebar_current_attrs(&self) -> lumux_core::render::CellAttributes {
        Self::styled("fg=colour231,bg=colour24,bold")
    }

    /// The sidebar's right border style (dim, so it separates without shouting).
    fn sidebar_border_attrs(&self) -> lumux_core::render::CellAttributes {
        Self::styled("fg=colour240,bg=colour235")
    }

    /// Per-state color for an agent's status glyph, over the given panel bg.
    fn agent_glyph_attrs(
        &self,
        state: lumux_core::agent::AgentState,
        _panel: &lumux_core::render::CellAttributes,
    ) -> lumux_core::render::CellAttributes {
        use lumux_core::agent::AgentState;
        let colour = match state {
            AgentState::Blocked => "colour203", // red — needs you
            AgentState::Working => "colour118", // green — busy
            AgentState::Done => "colour75",     // blue — finished
            AgentState::Idle => "colour245",    // gray — waiting
            AgentState::Unknown => "colour240",
        };
        Self::styled(&format!("fg={colour},bg=colour235,bold"))
    }

    /// Build a `CellAttributes` from a tmux-style `fg=..,bg=..,bold` spec, reusing
    /// the status-bar style parser so the sidebar needs no termwiz dependency.
    fn styled(spec: &str) -> lumux_core::render::CellAttributes {
        let mut a = lumux_core::render::CellAttributes::default();
        lumux_core::status::apply_style_spec(&mut a, spec);
        a
    }

    /// The most-urgent reported agent state among a window's panes, if any
    /// reported. "Most urgent" so a blocked pane surfaces even if a sibling is
    /// idle — that's the state you most need to see.
    fn window_agent_state(
        &self,
        session: SessionId,
        wid: WindowId,
    ) -> Option<lumux_core::agent::AgentState> {
        let w = self.server.session(session)?.window(wid)?;
        w.pane_ids()
            .into_iter()
            .filter_map(|pid| self.agent_status(pid).map(|s| s.display_state()))
            .max_by_key(|s| s.urgency())
    }

    /// Hit-test a click at sidebar row `y` (0-based screen row) for `session`,
    /// returning what to switch to — a session, or a session+window for an agent
    /// row. Header/blank/out-of-range rows return None. Uses the same projection
    /// the renderer draws, so clicks land on what's shown.
    pub fn sidebar_pick_at(
        &self,
        client_id: u64,
        session: SessionId,
        y: usize,
        height: usize,
    ) -> Option<SidebarPick> {
        let content_h = height.saturating_sub(1);
        let projection = self.sidebar_projection(client_id, session, content_h);
        match projection.row_at(y)? {
            SidebarProjectedRow::Session(entry) => Some(SidebarPick::Session(entry.sid)),
            SidebarProjectedRow::Agent(entry) => Some(SidebarPick::Agent {
                session: entry.sid,
                window: entry.wid,
                pane: entry.pid,
            }),
            SidebarProjectedRow::Header(_) => None,
        }
    }

    /// Whether a click at (col,row) inside the sidebar hit the collapse/expand
    /// toggle button. When collapsed the whole rail toggles (the button is the
    /// rail); when expanded it's the ◀ glyph at the top-right of the header.
    pub fn sidebar_toggle_hit(&self, session: SessionId, col: u16, row: u16) -> bool {
        let w = self.sidebar_width(session);
        if w == 0 || col >= w {
            return false;
        }
        if self.sidebar_collapsed(session) || w == 1 {
            // The whole rail is the expand button.
            true
        } else {
            // The ◀ button sits at text_w-1 on header row 0.
            let text_w = w.saturating_sub(1);
            row == 0 && text_w >= 1 && col == text_w - 1
        }
    }

    /// Whether a click hit the `+` new-session button on the SESSIONS header.
    /// Mirrors `SidebarFrame::click_at` so the live path agrees with the
    /// captured interaction map.
    pub fn sidebar_new_session_hit(&self, session: SessionId, col: u16, row: u16) -> bool {
        let w = self.sidebar_width(session);
        if w <= 1 || col >= w || self.sidebar_collapsed(session) {
            return false;
        }
        let text_w = w.saturating_sub(1) as usize;
        row == 0 && text_w >= NEW_SESSION_BUTTON_SPAN && col as usize == text_w - 3
    }

    /// Build the centre segment (window list) as styled spans plus the per-entry
    /// hit ranges, so `paint_status` and `status_window_at` stay in lockstep.
    /// Uses the tmux window-status format strings when configured, otherwise the
    /// built-in reverse-video list.
    fn window_list_segment(
        &self,
        s: &lumux_core::model::Session,
        base_idx: u32,
        base: &lumux_core::render::CellAttributes,
        ctx: &lumux_core::status::StatusContext,
    ) -> (Vec<lumux_core::status::Span>, Vec<(usize, usize, usize)>) {
        use lumux_core::status::{self, WindowEntry};
        let entries: Vec<WindowEntry> = s
            .window_ids()
            .iter()
            .enumerate()
            .filter_map(|(i, wid)| {
                s.window(*wid).map(|w| WindowEntry {
                    index: i as u32 + base_idx,
                    name: w.name.clone(),
                    active: *wid == s.active_window(),
                })
            })
            .collect();
        let cfg = &self.config;
        if cfg.window_status_format.is_empty() && cfg.window_status_current_format.is_empty() {
            let spans = status::window_list(&entries, base);
            let ranges = status::window_list_hit_ranges(&entries);
            (spans, ranges)
        } else {
            // If only one of the two formats is set, fall back to the other for
            // the missing one so a partial config still renders every entry.
            let inactive = if cfg.window_status_format.is_empty() {
                &cfg.window_status_current_format
            } else {
                &cfg.window_status_format
            };
            let current = if cfg.window_status_current_format.is_empty() {
                &cfg.window_status_format
            } else {
                &cfg.window_status_current_format
            };
            status::window_list_formatted(
                &entries,
                inactive,
                current,
                &cfg.window_status_separator,
                ctx,
                base,
            )
        }
    }

    /// Whether `client_id` has the prefix armed (pressed the prefix and is
    /// awaiting the next command key). Drives the `#{?client_prefix,…}` status
    /// token.
    fn client_prefix_armed(&self, client_id: u64) -> bool {
        self.keymaps
            .get(&client_id)
            .map(|k| k.mode() == lumux_core::keymap::Mode::AwaitingCommand)
            .unwrap_or(false)
    }

    /// Paint the bottom status row: a transient flash message if one is pending,
    /// otherwise the configured styled status bar.
    /// The status bar's right segment: the configured `status_right`, preceded by
    /// the prefix indicator while the prefix key is armed. Making the modal
    /// state visible is the whole point, so the indicator is styled to stand out
    /// and sits at the right edge where it does not shift the window list.
    fn status_right_spans(
        &self,
        ctx: &lumux_core::status::StatusContext,
    ) -> Vec<lumux_core::status::Span> {
        let right = lumux_core::status::format(&self.config.status_right, ctx);
        if !ctx.client_prefix || self.config.prefix_indicator.is_empty() {
            return right;
        }
        let mut spans = lumux_core::status::format(
            &format!(
                "#[bg=colour203,fg=colour231,bold] {} ",
                self.config.prefix_indicator
            ),
            ctx,
        );
        spans.extend(right);
        spans
    }

    fn paint_status(
        &self,
        screen: &mut lumux_core::render::Screen,
        client_id: u64,
        session: SessionId,
    ) -> StatusFrame {
        use lumux_core::render::{Justify, StyledStatus};
        use lumux_core::status::{self, StatusContext};

        let (status_width, status_y) = screen.dimensions();
        let empty = || StatusFrame {
            row: status_y.saturating_sub(1) as u16,
            hits: Vec::new(),
        };

        // An open rename prompt takes over the whole row (tmux's command prompt).
        if let Some(p) = self.prompt.get(&client_id) {
            // The command-prompt shows a bare ":" prefix; the others name the op.
            let line = match p.target {
                PromptTarget::Command => format!(":{}", p.buffer),
                PromptTarget::Window => format!("(rename-window) {}", p.buffer),
                PromptTarget::Session => format!("(rename-session) {}", p.buffer),
                PromptTarget::FindWindow => format!("(find-window) {}", p.buffer),
            };
            self.paint_message_row(screen, &line);
            // Park the caret at the end of the typed text: without this the
            // cursor stays wherever the pane left it, so the prompt looks
            // unfocused even though it has the keyboard.
            let caret = line.chars().count().min(status_width.saturating_sub(1));
            screen.set_cursor(Some((caret, status_y.saturating_sub(1))));
            return empty();
        }

        // A pending display-message takes over the whole row.
        if let Some(msg) = self.message.get(&client_id) {
            self.paint_message_row(screen, msg);
            return empty();
        }

        let Some(s) = self.server.session(session) else {
            return empty();
        };
        let window = match s.window(s.active_window()) {
            Some(w) => w,
            None => return empty(),
        };
        let base_idx = self.config.base_index;
        let ctx = StatusContext {
            session: s.name.clone(),
            window: window.name.clone(),
            window_index: window_index(s, window.id) + base_idx,
            pane_index: base_idx,
            host: hostname(),
            flags: String::new(),
            client_prefix: self.client_prefix_armed(client_id),
            time: now_parts(),
        };

        let base = StyledStatus::base_attrs(&self.config.status_bg, &self.config.status_fg);

        // Fall back to status_format if status_left is empty (simple setups).
        let left_fmt = if self.config.status_left.is_empty() {
            &self.config.status_format
        } else {
            &self.config.status_left
        };
        let wids = s.window_ids();
        let (centre, hit_ranges) = self.window_list_segment(s, base_idx, &base, &ctx);
        let styled = StyledStatus {
            left: status::format(left_fmt, &ctx),
            centre,
            right: self.status_right_spans(&ctx),
            base,
            justify: match self.config.status_justify.as_str() {
                "centre" | "center" => Justify::Centre,
                "right" => Justify::Right,
                _ => Justify::Left,
            },
        };
        let centre_start = styled.centre_start(status_width);
        styled.render(screen);
        let hits = hit_ranges
            .into_iter()
            .filter_map(|(position, start, end)| {
                let window = wids.get(position).copied()?;
                let start = centre_start.saturating_add(start).min(status_width);
                let end = centre_start.saturating_add(end).min(status_width);
                (start < end).then_some(StatusHit {
                    start: start as u16,
                    end: end as u16,
                    window,
                })
            })
            .collect();
        StatusFrame {
            row: status_y.saturating_sub(1) as u16,
            hits,
        }
    }

    /// Paint a full-width message/prompt row (display-message, command prompt),
    /// honoring tmux `message-style` when set and otherwise using reverse video.
    fn paint_message_row(&self, screen: &mut lumux_core::render::Screen, text: &str) {
        use lumux_core::render::{Cell, CellAttributes};
        let y = screen.dimensions().1.saturating_sub(1);
        if self.config.message_style.is_empty() {
            screen.status_line(y, text);
            return;
        }
        let w = screen.dimensions().0;
        let mut attrs = CellAttributes::default();
        lumux_core::status::apply_style_spec(&mut attrs, &self.config.message_style);
        for x in 0..w {
            screen.set_cell(x, y, Cell::new(' ', attrs.clone()));
        }
        screen.write_str(0, y, text, &attrs);
    }

    /// Hit-test a click on the status row against the window list: return the
    /// window id whose entry was clicked, if any. Rebuilds the same StyledStatus
    /// the renderer drew, so the column ranges line up exactly with what's shown.
    /// `col` is the clicked column; `width` is the status bar width (session cols).
    pub fn status_window_at(
        &self,
        session: SessionId,
        col: u16,
        width: usize,
    ) -> Option<lumux_core::model::WindowId> {
        use lumux_core::render::{Justify, StyledStatus};
        use lumux_core::status::{self, StatusContext};

        // A prompt or flash message owns the whole row — no window list to hit.
        let s = self.server.session(session)?;
        let window = s.window(s.active_window())?;
        let base_idx = self.config.base_index;
        let ctx = StatusContext {
            session: s.name.clone(),
            window: window.name.clone(),
            window_index: window_index(s, window.id) + base_idx,
            pane_index: base_idx,
            host: hostname(),
            flags: String::new(),
            // Hit-testing only needs geometry; prefix state can't shift the
            // window-list columns, so it's irrelevant here.
            client_prefix: false,
            time: now_parts(),
        };
        let base = StyledStatus::base_attrs(&self.config.status_bg, &self.config.status_fg);
        let left_fmt = if self.config.status_left.is_empty() {
            &self.config.status_format
        } else {
            &self.config.status_left
        };
        let wids = s.window_ids();
        let (centre, hit_ranges) = self.window_list_segment(s, base_idx, &base, &ctx);
        let styled = StyledStatus {
            left: status::format(left_fmt, &ctx),
            centre,
            right: self.status_right_spans(&ctx),
            base,
            justify: match self.config.status_justify.as_str() {
                "centre" | "center" => Justify::Centre,
                "right" => Justify::Right,
                _ => Justify::Left,
            },
        };
        // Map the click column into the centre segment, then to a window entry.
        let cx = styled.centre_start(width);
        let click = col as usize;
        if click < cx {
            return None;
        }
        let rel = click - cx;
        for (pos, start, end) in hit_ranges {
            if rel >= start && rel < end {
                return wids.get(pos).copied();
            }
        }
        None
    }

    /// Render the active pane's scrolled history for a client in copy-mode.
    fn render_copy_mode(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use lumux_core::render::WindowView;
        let size = self.server.effective_size(session)?;
        let s = self.server.session(session)?;
        let window = s.window(s.active_window())?;
        let active = window.active_pane();

        // Lay out exactly like the live view so the OTHER panes keep rendering in
        // their rectangles. A zoomed pane fills the screen; otherwise the split
        // tree. (Previously copy-mode painted the active pane full-screen, which
        // blanked every other pane — visible the moment you scrolled.)
        let layout = visible_layout(window);
        let mut grids = BTreeMap::new();
        for pid in window.pane_ids() {
            if let Some(p) = self.panes.get(&pid) {
                grids.insert(pid, &p.grid);
            }
        }
        let view = WindowView {
            layout: &layout,
            grids: &grids,
            active_pane: active,
            active_border: self.active_border_attrs(),
            inactive_border: self.inactive_border_attrs(),
        };
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let content_rows = rows.saturating_sub(1);
        let sidebar = self.sidebar_width(session).min(size.cols);
        let mut screen = compose((cols, rows), &view, None, true, sidebar as usize);
        // Copy mode replaces the pane plane, not persistent session chrome.
        // Paint the same sidebar projection as the live renderer so the
        // reserved columns never become a blank-but-still-clickable region.
        if sidebar > 0 {
            self.render_sidebar(&mut screen, client_id, session, sidebar, rows);
        }

        // The active pane's rectangle, into which we paint the scrolled view.
        let viewport = self.content_viewport_for_size(session, size);
        let rect = *lumux_core::layout::compute(&layout, viewport).get(&active)?;
        let grid = &self.panes.get(&active)?.grid;
        let cm = self.copy.get(&client_id)?;
        let top = cm.top();

        // Overpaint only the active pane's rect with its scrolled-back rows.
        // Copy real cells (not a re-derived string) so wide glyphs keep their
        // two-column layout and cell attributes/colors survive — exactly like
        // the live blit path. A plain char-by-char copy drifts every column
        // after a wide (CJK/emoji) char and drops colors, which looked garbled.
        let (ox, oy) = (rect.x as usize, rect.y as usize);
        screen.blit_grid_scrolled(ox, oy, rect.cols as usize, rect.rows as usize, grid, top);

        // Paint the selection highlight (reverse video) over the blitted rows so
        // a mouse-drag or keyboard selection is visible. selection_span gives the
        // exact column range a yank would copy on each combined-buffer row.
        let rect_cols = rect.cols as usize;
        for gy in 0..rect.rows as usize {
            if let Some((c0, c1)) = cm.selection_span(top + gy, rect_cols) {
                for gx in c0..c1.min(rect_cols) {
                    screen.reverse_cell(ox + gx, oy + gy);
                }
            }
        }

        // Place the copy cursor within the pane rect (if visible in this scroll).
        let cur = cm.cursor();
        if cur.row >= top && cur.row < top + rect.rows as usize {
            let cx = ox + cur.col.min(rect.cols.saturating_sub(1) as usize);
            let cy = oy + (cur.row - top);
            screen.set_cursor(Some((
                cx.min(cols.saturating_sub(1)),
                cy.min(content_rows.saturating_sub(1)),
            )));
        }

        // Copy-mode status line across the reserved bottom row. While a search
        // query is being typed, show it as a `/query` (or `?query`) prompt so
        // the user sees what they're searching for, like tmux.
        if let Some((prefix, query)) = self.search_prompt(client_id) {
            let line = format!("{prefix}{query}");
            screen.status_line(rows.saturating_sub(1), &line);
            // Park the cursor at the end of the query so typing feels live.
            let cx = lumux_core::render::display_width(&line).min(cols.saturating_sub(1));
            screen.set_cursor(Some((cx, rows.saturating_sub(1))));
        } else {
            let label = match (cm.has_selection(), cm.is_rectangle()) {
                (true, true) => {
                    "-- COPY (block) --  arrows move, Ctrl-v toggles block, Enter yanks, q quits"
                }
                (true, false) => {
                    "-- COPY (selecting) --  arrows/PgUp/PgDn move, / search, Enter yanks, q quits"
                }
                _ => "-- COPY --  arrows/PgUp/PgDn move, / search, Space selects, q quits",
            };
            screen.status_line(rows.saturating_sub(1), label);
        }

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Render the key-binding help overlay (tmux prefix ?). Lists the active
    /// bindings, generated from this client's keymap so it reflects config.
    fn render_help(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use lumux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let mut screen = Screen::new(cols, rows);

        let entries = self
            .keymaps
            .get(&client_id)
            .map(|k| k.bindings().help_entries())
            .unwrap_or_default();

        let key_width = entries
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(8)
            .min(20);

        // The list occupies rows [2, max_y); the bottom row is the status line.
        let max_y = rows.saturating_sub(1);
        let visible = max_y.saturating_sub(2);
        // Clamp the scroll offset so the last page can't scroll past the end
        // (this is also where HelpKey::Bottom's usize::MAX resolves to the end).
        let max_off = entries.len().saturating_sub(visible);
        let off = (*self.help_offset.get(&client_id).unwrap_or(&0)).min(max_off);

        screen.write_plain(0, 0, "lumux key bindings");
        for (row, (key, desc)) in entries.iter().skip(off).take(visible).enumerate() {
            let line = format!("  {key:<key_width$}   {desc}");
            screen.write_plain(0, 2 + row, &line);
        }

        // Status line: scroll hint, plus a position indicator when it scrolls.
        let status = if entries.len() > visible {
            let shown_end = (off + visible).min(entries.len());
            format!(
                "-- HELP --  ↑/↓ PgUp/PgDn scroll, q closes   [{}-{}/{}]",
                off + 1,
                shown_end,
                entries.len()
            )
        } else {
            "-- HELP --  q / Escape closes".to_string()
        };
        screen.status_line(rows.saturating_sub(1), &status);
        screen.set_cursor(None);

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Render the big-digit clock overlay (tmux `clock-mode`, prefix `t`): the
    /// current HH:MM in a large block font, centered on screen.
    fn render_clock(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use lumux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let mut screen = Screen::new(cols, rows);

        let t = now_parts();
        let text = format!("{:02}:{:02}", t.hour, t.minute);
        let glyphs: Vec<&[&str; 5]> = text.chars().map(clock_glyph).collect();
        let glyph_w = 3; // each digit/colon glyph is 3 columns wide
        let gap = 1; // one blank column between glyphs
        let art_w = glyphs.len() * glyph_w + glyphs.len().saturating_sub(1) * gap;
        let art_h = 5;
        let content_rows = rows.saturating_sub(1);
        let ox = (cols.saturating_sub(art_w)) / 2;
        let oy = (content_rows.saturating_sub(art_h)) / 2;

        for (gi, glyph) in glyphs.iter().enumerate() {
            let gx = ox + gi * (glyph_w + gap);
            for (row, line) in glyph.iter().enumerate() {
                for (col, ch) in line.chars().enumerate() {
                    if ch != ' ' {
                        screen.write_plain(gx + col, oy + row, "█");
                    }
                }
            }
        }
        screen.status_line(rows.saturating_sub(1), "-- CLOCK --  any key closes");
        screen.set_cursor(None);

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Render the session switcher / choose-tree overlay (tmux prefix s): a tree
    /// of sessions (expandable to their windows) on the left, and on the right a
    /// live preview that follows the cursor — every window of a session row, or a
    /// single window when the cursor is on a window row.
    fn render_chooser(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use lumux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let mut screen = Screen::new(cols, rows);
        let tree = self.choosing.get(&client_id)?.clone();
        let tree_rows = self.tree_rows(&tree);
        let cursor = tree.cursor.min(tree_rows.len().saturating_sub(1));

        // Left list column: about a third of the width, clamped to a sane range
        // (but never wider than the screen, leaving room for a divider+preview).
        let list_w = (cols / 3).clamp(20, 40).min(cols.saturating_sub(2));
        let max_y = rows.saturating_sub(1);

        screen.write_plain(0, 0, "choose a session");
        // Scroll the list so the cursor row stays visible.
        let list_rows = max_y.saturating_sub(2); // rows [2, max_y)
        let max_first = tree_rows.len().saturating_sub(list_rows.max(1));
        let first = cursor
            .saturating_sub(list_rows.saturating_sub(1))
            .min(max_first);
        let mut session_ordinal = 0usize;
        for (i, row) in tree_rows.iter().enumerate() {
            // Count session ordinals across the WHOLE list (for the digit label),
            // even rows scrolled out of view.
            let this_session_ord = session_ordinal;
            if matches!(row, TreeRow::Session(_)) {
                session_ordinal += 1;
            }
            if i < first {
                continue;
            }
            let y = 2 + (i - first);
            if y >= max_y {
                break;
            }
            let line = match row {
                TreeRow::Session(sid) => {
                    let Some(s) = self.server.session(*sid) else {
                        continue;
                    };
                    let marker = if tree.expanded.contains(sid) {
                        "▾"
                    } else {
                        "▸"
                    };
                    let count = format!("{}w", s.window_count());
                    let prefix = format!("{marker} {this_session_ord}: ");
                    let name_room = list_w
                        .saturating_sub(prefix.chars().count())
                        .saturating_sub(count.chars().count())
                        .saturating_sub(1);
                    let name: String = s.name.chars().take(name_room).collect();
                    let used =
                        prefix.chars().count() + name.chars().count() + count.chars().count();
                    let pad = list_w.saturating_sub(used);
                    format!("{prefix}{name}{}{count}", " ".repeat(pad))
                }
                TreeRow::Window(sid, wid) => {
                    let Some(s) = self.server.session(*sid) else {
                        continue;
                    };
                    let Some(w) = s.window(*wid) else { continue };
                    let active = w.id == s.active_window();
                    let idx = window_index(s, *wid) + self.config.base_index;
                    let mark = if active { "*" } else { "" };
                    // Prefix the most-urgent agent glyph among the window's panes,
                    // so the chooser shows the same status the sidebar does.
                    let glyph = self
                        .window_agent_state(*sid, *wid)
                        .map(|st| format!("{} ", Self::agent_glyph(st, self.spinner_tick)))
                        .unwrap_or_default();
                    let mut line = format!("    {glyph}{idx}:{}{mark}", w.name);
                    if line.chars().count() > list_w {
                        line = line.chars().take(list_w).collect();
                    }
                    line
                }
            };
            if i == cursor {
                screen.status_line_width(y, &line, list_w);
            } else {
                screen.write_plain_clipped(0, y, &line, list_w);
            }
        }

        // Divider + preview that follows the cursor row.
        let div_x = list_w;
        let preview_x = list_w + 1;
        if preview_x < cols {
            screen.vline(div_x, 0, max_y, &Default::default());
            let pw = cols - preview_x;
            let ph = max_y; // preview area height (bottom row is the mode line)
            match tree_rows.get(cursor) {
                Some(TreeRow::Session(sid)) => {
                    self.render_session_preview(&mut screen, Some(*sid), preview_x, pw, ph);
                }
                Some(TreeRow::Window(sid, wid)) => {
                    self.render_window_preview(&mut screen, *sid, *wid, preview_x, pw, ph);
                }
                None => {}
            }
        }

        screen.status_line(
            max_y,
            "-- TREE --  ↑/↓ move, →/← expand/collapse, Enter selects, Esc cancels",
        );
        screen.set_cursor(None);

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Preview a single window's full split layout, filling the whole preview
    /// region (used when the choose-tree cursor is on a window row).
    fn render_window_preview(
        &self,
        screen: &mut lumux_core::render::Screen,
        sid: SessionId,
        wid: WindowId,
        x: usize,
        w: usize,
        h: usize,
    ) {
        let Some(s) = self.server.session(sid) else {
            return;
        };
        let Some(win) = s.window(wid) else { return };
        if h == 0 {
            return;
        }
        let base_idx = self.config.base_index;
        let idx = window_index(s, wid) + base_idx;
        let marker = if win.id == s.active_window() { "*" } else { "" };
        let header = format!("{idx}:{}{marker}", win.name);
        screen.label_segment(x, 0, w, &header);
        let content_h = h.saturating_sub(1);
        if content_h > 0 {
            let mut grids = std::collections::BTreeMap::new();
            for pid in win.pane_ids() {
                if let Some(p) = self.panes.get(&pid) {
                    grids.insert(pid, &p.grid);
                }
            }
            lumux_core::render::blit_window_layout(screen, x, 1, w, content_h, &win.layout, &grids);
        }
    }

    /// Render the paste-buffer chooser (tmux prefix `=`). A left list of buffers
    /// (`index: <preview>`) with the highlighted one reversed, and a right pane
    /// previewing the highlighted buffer's full text (clipped to the region).
    fn render_buffer_chooser(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use lumux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let mut screen = Screen::new(cols, rows);
        let sel = *self.choosing_buffer.get(&client_id)?;

        let list_w = (cols / 3).clamp(20, 40).min(cols.saturating_sub(2));
        let max_y = rows.saturating_sub(1);

        screen.write_plain(0, 0, "choose a paste buffer");
        for (i, buf) in self.buffers.iter().enumerate() {
            let y = 2 + i;
            if y >= max_y {
                break;
            }
            // One-line preview: first line, control chars shown as spaces.
            let preview = first_line_preview(&buf.text);
            let mut line = format!("{i}: {preview}");
            if line.chars().count() > list_w {
                line = line.chars().take(list_w).collect();
            }
            if i == sel {
                screen.status_line_width(y, &line, list_w);
            } else {
                screen.write_plain_clipped(0, y, &line, list_w);
            }
        }

        // Divider + full preview of the highlighted buffer.
        let div_x = list_w;
        let preview_x = list_w + 1;
        if preview_x < cols {
            screen.vline(div_x, 0, max_y, &Default::default());
            if let Some(buf) = self.buffers.get(sel) {
                let pw = cols - preview_x;
                for (row, line) in buf.text.lines().enumerate() {
                    if row >= max_y {
                        break;
                    }
                    let printable: String = line
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect();
                    screen.write_plain_clipped(preview_x, row, &printable, pw);
                }
            }
        }

        screen.status_line(
            max_y,
            "-- BUFFERS --  Up/Down or digit, Enter/p pastes, d deletes, Esc cancels",
        );
        screen.set_cursor(None);

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }
    /// clipped live view of that window's active pane. The height is split as
    /// evenly as possible across the windows.
    fn render_session_preview(
        &self,
        screen: &mut lumux_core::render::Screen,
        preview_sid: Option<SessionId>,
        x: usize,
        w: usize,
        h: usize,
    ) {
        let Some(sid) = preview_sid else {
            return;
        };
        let Some(s) = self.server.session(sid) else {
            return;
        };
        let wids = s.window_ids();
        if wids.is_empty() || h == 0 {
            return;
        }
        let active_wid = s.active_window();
        // Share the height across windows; each cell needs at least a header
        // (1 row) plus ideally some content. Cap the number shown so each gets
        // a usable slice.
        let max_shown = (h / 2).max(1); // >=2 rows per window when possible
        let shown = wids.len().min(max_shown);
        let slot_h = h / shown;
        let base_idx = self.config.base_index;

        for (i, wid) in wids.iter().take(shown).enumerate() {
            let top = i * slot_h;
            // Last slot soaks up any remainder.
            let this_h = if i + 1 == shown { h - top } else { slot_h };
            let is_last = i + 1 == shown;
            let Some(win) = s.window(*wid) else {
                continue;
            };
            let marker = if *wid == active_wid { "*" } else { "" };
            let header = format!("{}:{}{marker}", i as u32 + base_idx, win.name);
            screen.label_segment(x, top, w, &header);
            // Reserve the slot's bottom row for a separator between windows (not
            // after the last one), so each preview is visually delimited.
            let sep_rows = if is_last { 0 } else { 1 };
            let content_h = this_h.saturating_sub(1 + sep_rows); // minus header (+sep)
            if content_h > 0 {
                // Preview the window's WHOLE split layout (all panes + dividers),
                // shrunk into the slot — not just the active pane.
                let mut grids = std::collections::BTreeMap::new();
                for pid in win.pane_ids() {
                    if let Some(p) = self.panes.get(&pid) {
                        grids.insert(pid, &p.grid);
                    }
                }
                lumux_core::render::blit_window_layout(
                    screen,
                    x,
                    top + 1,
                    w,
                    content_h,
                    &win.layout,
                    &grids,
                );
            }
            if sep_rows == 1 {
                // Thin horizontal rule across the preview column at the slot's end.
                let sep_y = top + this_h - 1;
                screen.hline(sep_y, x, x + w, &Default::default());
            }
        }
        // If there are more windows than slots, note the overflow on the last row.
        if wids.len() > shown {
            let more = format!("  (+{} more windows)", wids.len() - shown);
            screen.write_plain_clipped(x, h.saturating_sub(1), &more, w);
        }
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
        // and grid in step with the layout rectangles. Panes occupy the columns
        // to the right of the sidebar.
        let viewport = self.content_viewport_for_size(session, size);
        if let Some(w) = s.window(s.active_window()) {
            // A zoomed pane fills the whole content area; otherwise lay out the
            // real split tree. This must mirror render_for_client's choice so the
            // PTY dimensions match what the client actually sees.
            let layout = visible_layout(w);
            let rects = lumux_core::layout::compute(&layout, viewport);
            for pid in pane_ids {
                if let (Some(rect), Some(p)) = (rects.get(&pid), self.panes.get_mut(&pid)) {
                    let psz = PtySize::new(rect.cols.max(1), rect.rows.max(1));
                    let _ = p.writer.resize(psz);
                    p.grid.resize(psz.cols as usize, psz.rows as usize);
                }
            }
        }
        // Geometry mutation is renderer-free. ClientRenderer automatically
        // performs a full repaint when screen dimensions change; same-size
        // layout changes are safely damage-diffed. Invalidating here used to
        // dirty clients attached to unrelated sessions, so the next global
        // sidebar update cleared their whole terminals.
    }

    /// Re-fit the panes of EVERY window in `session` to `size` (not just the
    /// active one). Needed after break-pane, which changes two windows at once:
    /// the new window the pane moved to, and the source window whose remaining
    /// panes grew. A plain [`resize_session`] only refits the active window, so
    /// the inactive source window's PTYs would stay mis-sized until refocused.
    pub fn resize_all_windows(&mut self, session: SessionId, size: PtySize) {
        let Some(s) = self.server.session(session) else {
            return;
        };
        let viewport = self.content_viewport_for_size(session, size);
        // Collect (pane, rect) for every window first to avoid borrow conflicts.
        let mut fits: Vec<(PaneId, lumux_core::layout::Rect)> = Vec::new();
        for wid in s.window_ids() {
            if let Some(w) = s.window(wid) {
                let layout = visible_layout(w);
                for (pid, rect) in lumux_core::layout::compute(&layout, viewport) {
                    fits.push((pid, rect));
                }
            }
        }
        for (pid, rect) in fits {
            if let Some(p) = self.panes.get_mut(&pid) {
                let psz = PtySize::new(rect.cols.max(1), rect.rows.max(1));
                let _ = p.writer.resize(psz);
                p.grid.resize(psz.cols as usize, psz.rows as usize);
            }
        }
        // See resize_session: renderer invalidation belongs to the affected
        // client lifecycle, never to this session-scoped PTY operation.
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

/// Clone a [`PaneNode`] tree, replacing each leaf's real [`PaneId`] with the
/// snapshot-local id produced by `id_of`. Used when capturing a layout so the
/// saved tree references dense ids independent of the live id counter.
fn remap_layout(
    node: &lumux_core::model::PaneNode,
    id_of: &impl Fn(PaneId) -> u32,
) -> lumux_core::model::PaneNode {
    use lumux_core::model::PaneNode;
    match node {
        PaneNode::Leaf(pid) => PaneNode::Leaf(PaneId(id_of(*pid))),
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => PaneNode::Split {
            dir: *dir,
            ratio: *ratio,
            first: Box::new(remap_layout(first, id_of)),
            second: Box::new(remap_layout(second, id_of)),
        },
    }
}

/// 0-based position of `wid` within its session's window list.
fn window_index(session: &lumux_core::model::Session, wid: lumux_core::model::WindowId) -> u32 {
    session
        .window_ids()
        .iter()
        .position(|&w| w == wid)
        .unwrap_or(0) as u32
}

/// A one-line, control-char-free preview of a buffer's text for the chooser
/// list: the first non-empty line, with control characters shown as spaces.
fn first_line_preview(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Local hostname for the `#H` token.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "localhost".to_string())
}

/// A 5-row-tall, 3-column-wide block-digit glyph for `c` (used by clock-mode).
/// Each row is a 3-char string; a non-space character means "filled" (drawn as
/// a solid block). Falls back to a blank glyph for any character not in the
/// small set clock-mode actually needs (digits + colon).
fn clock_glyph(c: char) -> &'static [&'static str; 5] {
    const BLANK: [&str; 5] = ["   ", "   ", "   ", "   ", "   "];
    const ZERO: [&str; 5] = ["###", "# #", "# #", "# #", "###"];
    const ONE: [&str; 5] = ["  #", "  #", "  #", "  #", "  #"];
    const TWO: [&str; 5] = ["###", "  #", "###", "#  ", "###"];
    const THREE: [&str; 5] = ["###", "  #", "###", "  #", "###"];
    const FOUR: [&str; 5] = ["# #", "# #", "###", "  #", "  #"];
    const FIVE: [&str; 5] = ["###", "#  ", "###", "  #", "###"];
    const SIX: [&str; 5] = ["###", "#  ", "###", "# #", "###"];
    const SEVEN: [&str; 5] = ["###", "  #", "  #", "  #", "  #"];
    const EIGHT: [&str; 5] = ["###", "# #", "###", "# #", "###"];
    const NINE: [&str; 5] = ["###", "# #", "###", "  #", "###"];
    const COLON: [&str; 5] = ["   ", " # ", "   ", " # ", "   "];
    match c {
        '0' => &ZERO,
        '1' => &ONE,
        '2' => &TWO,
        '3' => &THREE,
        '4' => &FOUR,
        '5' => &FIVE,
        '6' => &SIX,
        '7' => &SEVEN,
        '8' => &EIGHT,
        '9' => &NINE,
        ':' => &COLON,
        _ => &BLANK,
    }
}

/// Current local time broken into the parts the status formatter needs.
#[cfg(unix)]
fn now_parts() -> lumux_core::status::TimeParts {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        lumux_core::status::TimeParts {
            hour: tm.tm_hour as u8,
            minute: tm.tm_min as u8,
            second: tm.tm_sec as u8,
            day: tm.tm_mday as u8,
            month: (tm.tm_mon + 1) as u8,
            year: (tm.tm_year + 1900) as u16,
        }
    }
}

/// Windows: query the local wall clock via GetLocalTime so status-bar time
/// tokens (%H:%M, %d-%b-%y) render the real time. GetLocalTime is infallible and
/// already returns broken-out local components, so no timezone math is needed.
#[cfg(windows)]
fn now_parts() -> lumux_core::status::TimeParts {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    lumux_core::status::TimeParts {
        hour: st.wHour as u8,
        minute: st.wMinute as u8,
        second: st.wSecond as u8,
        day: st.wDay as u8,
        month: st.wMonth as u8,
        year: st.wYear,
    }
}

/// Other non-unix targets keep the zero default (no clock wired).
#[cfg(not(any(unix, windows)))]
fn now_parts() -> lumux_core::status::TimeParts {
    lumux_core::status::TimeParts::default()
}

#[cfg(all(test, unix))]
mod agent_status_tests {
    use super::*;
    use lumux_backend_unix::UnixPtySystem;
    use lumux_core::agent::{AgentClear, AgentIdentity, AgentReport, AgentState};
    use lumux_core::traits::PtySize;

    #[test]
    fn pane_environment_carries_endpoint_identity_and_exact_reporter() {
        let pane: PaneId = "%7".parse().unwrap();
        let runtime = PaneRuntime {
            endpoint: Some(OsString::from("/run/lumux-test.sock")),
            reporter: Some(std::path::PathBuf::from("/opt/lumux/bin/lumux")),
        };
        let env = runtime
            .environment(pane)
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            env.get("LUMUX").map(String::as_str),
            Some("/run/lumux-test.sock")
        );
        assert_eq!(env.get("LUMUX_PANE").map(String::as_str), Some("%7"));
        assert_eq!(
            env.get("LUMUX_BIN").map(String::as_str),
            Some("/opt/lumux/bin/lumux")
        );

        let fallback = PaneRuntime {
            endpoint: None,
            reporter: None,
        }
        .environment(pane)
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        assert_eq!(fallback.get("LUMUX").map(String::as_str), Some("1"));
        assert!(!fallback.contains_key("LUMUX_BIN"));
    }

    fn spawn() -> (Daemon<UnixPtySystem>, SessionId, PaneId) {
        let mut d = Daemon::new(UnixPtySystem);
        let (sid, pid, _reader) = d
            .new_session("t", Some(vec!["/bin/sh".to_string()]), PtySize::new(80, 24))
            .expect("spawn session");
        (d, sid, pid)
    }

    fn report(
        agent: impl Into<String>,
        owner: Option<String>,
        claim: bool,
        state: AgentState,
        sequence: u64,
    ) -> AgentReport {
        AgentReport::new(AgentIdentity::new(agent, owner), claim, state, sequence)
    }

    fn clear(agent: impl Into<String>, owner: Option<String>, sequence: u64) -> AgentClear {
        AgentClear::new(AgentIdentity::new(agent, owner), sequence)
    }

    #[test]
    fn copy_mode_does_not_retarget_after_shared_focus_changes() {
        let (mut daemon, session, left) = spawn();
        let (right, _reader) = daemon
            .split_active(session, SplitDir::Horizontal, PtySize::new(80, 24))
            .expect("split succeeds")
            .expect("new pane");
        let client = daemon
            .server
            .attach_client(session, PtySize::new(80, 24))
            .expect("attach client");
        daemon.register_client(client);

        daemon.enter_copy_mode(client, session);
        assert!(daemon.in_copy_mode(client));
        assert_eq!(daemon.copy.get(&client).map(|copy| copy.pane), Some(right));

        let current = daemon.server.session_mut(session).expect("session exists");
        let window = current.active_window();
        assert!(current
            .window_mut(window)
            .is_some_and(|window| window.focus_pane(left)));

        assert!(
            !daemon.copy_navigate(client, session, CopyKey::Up),
            "copy navigation must fail closed instead of applying right-pane state to left"
        );
        assert!(!daemon.in_copy_mode(client));
    }

    #[test]
    fn rendered_copy_target_rejects_exit_and_reentry_on_another_pane() {
        let (mut daemon, session, left) = spawn();
        let (_right, _reader) = daemon
            .split_active(session, SplitDir::Horizontal, PtySize::new(80, 24))
            .expect("split succeeds")
            .expect("new pane");
        let client = daemon
            .server
            .attach_client(session, PtySize::new(80, 24))
            .expect("attach client");
        daemon.register_client(client);
        daemon.enter_copy_mode(client, session);
        let applied = daemon
            .interaction_map_for_client(client, session, None)
            .expect("copy interaction map");
        let old_target = applied.copy_pane().expect("rendered copy target");

        daemon.exit_copy_mode(client);
        let current = daemon.server.session_mut(session).expect("session exists");
        let window = current.active_window();
        assert!(current
            .window_mut(window)
            .is_some_and(|window| window.focus_pane(left)));
        daemon.enter_copy_mode(client, session);
        assert!(daemon.in_copy_mode(client));
        assert!(
            !daemon.copy_target_matches(client, old_target),
            "an old copy frame must not match a new copy generation on another pane"
        );
    }

    #[test]
    fn mouse_aware_pane_press_does_not_arm_server_selection() {
        let (mut daemon, session, pane) = spawn();
        let client = daemon
            .server
            .attach_client(session, PtySize::new(80, 24))
            .expect("attach client");
        daemon.register_client(client);
        daemon.feed_pane(pane, b"\x1b[?1000h");
        assert!(daemon.pane_wants_mouse(pane));

        daemon.mouse_sel_arm(client, session, 1, 1);
        assert!(
            !daemon.mouse_sel_pending(client),
            "an app-owned drag must not leave a latent lumux selection gesture"
        );
    }

    #[test]
    fn report_and_get_agent_status() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.agent_status(pid).is_none(), "no status before any report");
        assert!(d.report_agent_state(pid, report("claude", None, false, AgentState::Working, 1)));
        let got = d.agent_status(pid).expect("status after report");
        assert_eq!(got.agent, "claude");
        assert_eq!(got.semantic_state(), AgentState::Working);
        // A later report overwrites (sticky last-write).
        assert!(d.report_agent_state(pid, report("claude", None, false, AgentState::Blocked, 2)));
        assert_eq!(
            d.agent_status(pid).unwrap().semantic_state(),
            AgentState::Blocked
        );
    }

    #[test]
    fn stale_report_cannot_overwrite_newer_state() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(pid, report("claude", None, false, AgentState::Working, 20)));
        assert!(d.report_agent_state(pid, report("claude", None, false, AgentState::Idle, 30)));
        assert_eq!(
            d.agent_status(pid).unwrap().display_state(),
            AgentState::Done
        );

        assert!(!d.report_agent_state(pid, report("claude", None, false, AgentState::Blocked, 25)));
        assert_eq!(
            d.agent_status(pid).unwrap().display_state(),
            AgentState::Done
        );
    }

    #[test]
    fn clear_sequence_prevents_delayed_report_from_resurrecting_agent() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(pid, report("claude", None, false, AgentState::Working, 40)));
        assert!(d.clear_agent_status(pid, clear("claude", None, 50)));
        assert!(d.agent_status(pid).is_none());

        assert!(!d.report_agent_state(pid, report("claude", None, false, AgentState::Idle, 45)));
        assert!(d.agent_status(pid).is_none());

        assert!(d.report_agent_state(pid, report("claude", None, false, AgentState::Idle, 60)));
        assert_eq!(
            d.agent_status(pid).unwrap().display_state(),
            AgentState::Idle
        );
    }

    #[test]
    fn delayed_clear_from_replaced_agent_does_not_remove_current_agent() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session".to_string()),
                true,
                AgentState::Working,
                10,
            ),
        ));
        assert!(d.report_agent_state(
            pid,
            report(
                "codex",
                Some("codex-session".to_string()),
                true,
                AgentState::Working,
                20,
            ),
        ));

        assert!(!d.clear_agent_status(pid, clear("claude", Some("claude-session".to_string()), 30)));
        let status = d.agent_status(pid).expect("Codex lifecycle must remain");
        assert_eq!(status.agent, "codex");
        assert_eq!(status.semantic_state(), AgentState::Working);
    }

    #[test]
    fn delayed_clear_from_previous_owner_does_not_remove_new_same_agent_session() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session-a".to_string()),
                true,
                AgentState::Working,
                10,
            ),
        ));
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session-b".to_string()),
                true,
                AgentState::Blocked,
                20,
            ),
        ));

        assert!(!d.clear_agent_status(
            pid,
            clear("claude", Some("claude-session-a".to_string()), 30)
        ));
        let status = d
            .agent_status(pid)
            .expect("new Claude lifecycle must remain");
        assert_eq!(status.agent, "claude");
        assert_eq!(status.semantic_state(), AgentState::Blocked);
    }

    #[test]
    fn nonclaim_old_owner_cannot_reclaim_or_clear_newer_agent() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session".to_string()),
                true,
                AgentState::Working,
                10,
            ),
        ));
        assert!(d.report_agent_state(
            pid,
            report(
                "codex",
                Some("codex-session".to_string()),
                true,
                AgentState::Working,
                20,
            ),
        ));

        assert!(!d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session".to_string()),
                false,
                AgentState::Blocked,
                40,
            ),
        ));
        assert!(
            !d.clear_agent_status(pid, clear("claude", Some("claude-session".to_string()), 50),)
        );
        // Rejected foreign events do not advance the current owner's sequence.
        assert!(d.report_agent_state(
            pid,
            report(
                "codex",
                Some("codex-session".to_string()),
                false,
                AgentState::Idle,
                30,
            ),
        ));
        let status = d.agent_status(pid).expect("Codex lifecycle must remain");
        assert_eq!(status.agent, "codex");
        assert_eq!(status.display_state(), AgentState::Done);
    }

    #[test]
    fn explicit_newer_claim_replaces_current_lifecycle() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session".to_string()),
                true,
                AgentState::Working,
                10,
            ),
        ));

        assert!(d.report_agent_state(
            pid,
            report(
                "codex",
                Some("codex-session".to_string()),
                true,
                AgentState::Idle,
                20,
            ),
        ));
        let status = d.agent_status(pid).expect("claimed Codex lifecycle");
        assert_eq!(status.agent, "codex");
        assert_eq!(status.display_state(), AgentState::Idle);
    }

    #[test]
    fn same_owner_normal_reports_continue_current_lifecycle() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session".to_string()),
                true,
                AgentState::Working,
                10,
            ),
        ));

        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session".to_string()),
                false,
                AgentState::Idle,
                20,
            ),
        ));
        assert_eq!(
            d.agent_status(pid).unwrap().display_state(),
            AgentState::Done
        );
    }

    #[test]
    fn replacement_owner_starts_idle_without_inheriting_a_completion() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session-a".to_string()),
                true,
                AgentState::Working,
                10,
            ),
        ));

        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session-b".to_string()),
                true,
                AgentState::Idle,
                20,
            ),
        ));
        let status = d.agent_status(pid).expect("replacement lifecycle");
        assert_eq!(status.display_state(), AgentState::Idle);
        assert!(status.is_acknowledged());
    }

    #[test]
    fn matching_agent_owner_clear_removes_status() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.report_agent_state(
            pid,
            report(
                "claude",
                Some("claude-session".to_string()),
                true,
                AgentState::Working,
                10,
            ),
        ));

        assert!(d.clear_agent_status(pid, clear("claude", Some("claude-session".to_string()), 20)));
        assert!(d.agent_status(pid).is_none());
    }

    #[test]
    fn owned_tombstone_requires_a_claim_to_resume() {
        let (mut d, _sid, pid) = spawn();
        let owner = Some("claude-session".to_string());
        assert!(d.report_agent_state(
            pid,
            report("claude", owner.clone(), true, AgentState::Working, 10),
        ));
        assert!(d.clear_agent_status(pid, clear("claude", owner.clone(), 20)));

        assert!(!d.report_agent_state(
            pid,
            report("claude", owner.clone(), false, AgentState::Blocked, 30),
        ));
        assert!(d.agent_status(pid).is_none());

        assert!(d.report_agent_state(pid, report("claude", owner, true, AgentState::Idle, 40),));
        assert_eq!(
            d.agent_status(pid).unwrap().display_state(),
            AgentState::Idle
        );
    }

    #[test]
    fn closing_a_pane_clears_its_agent_status() {
        let (mut d, sid, pid) = spawn();
        d.report_agent_state(pid, report("claude", None, false, AgentState::Working, 1));
        assert!(d.agent_status(pid).is_some());
        // Pane death (its process exited) must drop the now-stale status.
        let _ = d.close_pane(sid, pid);
        assert!(
            d.agent_status(pid).is_none(),
            "close_pane must clear the pane's agent status"
        );
    }

    #[test]
    fn last_pane_cascade_clears_only_session_scoped_sidebar_state() {
        let (mut d, sid, pid) = spawn();
        d.set_sidebar_visible(sid, false);
        d.set_sidebar_collapsed(sid, true);
        d.sidebar_scroll.insert(
            99,
            SidebarScroll {
                sessions: 3,
                agents: 4,
            },
        );
        d.marked_pane = Some((sid, pid));

        assert_eq!(d.close_pane(sid, pid), CascadeResult::SessionClosed);
        assert!(!d.sidebar_on.contains_key(&sid));
        assert!(!d.sidebar_collapsed.contains_key(&sid));
        assert_eq!(
            d.sidebar_scroll.get(&99),
            Some(&SidebarScroll {
                sessions: 3,
                agents: 4,
            }),
            "closing a session must not erase presentation state owned by a client"
        );
        assert!(d.marked_pane.is_none());
    }

    #[test]
    fn unregister_client_clears_its_sidebar_viewport() {
        let (mut d, _sid, _pid) = spawn();
        d.sidebar_scroll.insert(
            99,
            SidebarScroll {
                sessions: 3,
                agents: 4,
            },
        );

        d.unregister_client(99);

        assert!(!d.sidebar_scroll.contains_key(&99));
    }

    #[test]
    fn sidebar_scrolling_is_owned_by_each_client() {
        let (mut d, sid, _pid) = spawn();
        let mut sessions = vec![sid];
        for index in 1..6 {
            let (created, _pane, _reader) = d
                .new_session(
                    format!("s{index}"),
                    Some(vec!["/bin/sh".to_string()]),
                    PtySize::new(80, 10),
                )
                .expect("spawn overflow session");
            sessions.push(created);
        }
        let first = d
            .server
            .attach_client(sid, PtySize::new(80, 10))
            .expect("attach first client");
        let second = d
            .server
            .attach_client(sid, PtySize::new(80, 10))
            .expect("attach second client");
        d.register_client(first);
        d.register_client(second);

        assert!(d.scroll_sidebar(first, sid, 2, 10, false));

        assert!(matches!(
            d.sidebar_pick_at(first, sid, 1, 10),
            Some(SidebarPick::Session(target)) if target == sessions[2]
        ));
        assert!(matches!(
            d.sidebar_pick_at(second, sid, 1, 10),
            Some(SidebarPick::Session(target)) if target == sessions[0]
        ));
    }

    #[test]
    fn rendering_persists_clamped_session_offset_across_shrink_then_regrow() {
        let (mut d, sid, _pid) = spawn();
        let mut overflow = Vec::new();
        for index in 1..6 {
            let (created, _pane, _reader) = d
                .new_session(
                    format!("session-{index}"),
                    Some(vec!["/bin/sh".to_string()]),
                    PtySize::new(80, 10),
                )
                .expect("spawn overflow session");
            overflow.push(created);
        }
        let client = d
            .server
            .attach_client(sid, PtySize::new(80, 10))
            .expect("attach client");
        d.register_client(client);
        assert!(d.scroll_sidebar(client, sid, 2, 10, false));
        assert_eq!(d.sidebar_scroll.get(&client).unwrap().sessions, 2);

        for session in overflow {
            assert!(d.server.kill_session(session));
        }
        let _ = d
            .render_for_client(client, sid)
            .expect("render after session-list shrink");
        assert_eq!(
            d.sidebar_scroll.get(&client).unwrap().sessions,
            0,
            "rendering the shrunken list must persist its clamped viewport"
        );

        for index in 1..6 {
            d.new_session(
                format!("replacement-{index}"),
                Some(vec!["/bin/sh".to_string()]),
                PtySize::new(80, 10),
            )
            .expect("regrow session list");
        }
        assert!(matches!(
            d.sidebar_pick_at(client, sid, 1, 10),
            Some(SidebarPick::Session(picked)) if picked == sid
        ));
    }

    #[test]
    fn rendering_persists_clamped_agent_offset_across_shrink_then_regrow() {
        let (mut d, sid, first_pane) = spawn();
        let mut agents = vec![(first_pane, "agent-0".to_string())];
        assert!(d.report_agent_state(
            first_pane,
            report("agent-0", None, false, AgentState::Working, 1),
        ));
        for index in 1..6 {
            let (_session, pane, _reader) = d
                .new_session(
                    format!("agent-session-{index}"),
                    Some(vec!["/bin/sh".to_string()]),
                    PtySize::new(80, 10),
                )
                .expect("spawn overflow agent");
            let name = format!("agent-{index}");
            assert!(d.report_agent_state(pane, report(&name, None, false, AgentState::Working, 1),));
            agents.push((pane, name));
        }
        let client = d
            .server
            .attach_client(sid, PtySize::new(80, 10))
            .expect("attach client");
        d.register_client(client);
        assert!(d.scroll_sidebar(client, sid, 5, 10, false));
        assert_eq!(d.sidebar_scroll.get(&client).unwrap().agents, 3);

        for (pane, name) in agents.iter().skip(1) {
            assert!(d.clear_agent_status(*pane, clear(name, None, 2)));
        }
        let _ = d
            .render_for_client(client, sid)
            .expect("render after agent-list shrink");
        assert_eq!(
            d.sidebar_scroll.get(&client).unwrap().agents,
            0,
            "rendering the shrunken list must persist its clamped viewport"
        );

        for (pane, name) in agents.iter().skip(1) {
            assert!(d.report_agent_state(*pane, report(name, None, false, AgentState::Working, 3),));
        }
        assert!(matches!(
            d.sidebar_pick_at(client, sid, 6, 10),
            Some(SidebarPick::Agent { pane, .. }) if pane == first_pane
        ));
    }

    #[test]
    fn sidebar_projection_routes_agent_scroll_at_the_rendered_section_boundary() {
        let (mut d, sid, first_pane) = spawn();
        let mut sessions = vec![sid];
        let mut agent_panes = vec![first_pane];
        assert!(d.report_agent_state(
            first_pane,
            report("claude", None, false, AgentState::Working, 1),
        ));
        for index in 1..6 {
            let (created, pane, _reader) = d
                .new_session(
                    format!("s{index}"),
                    Some(vec!["/bin/sh".to_string()]),
                    PtySize::new(80, 10),
                )
                .expect("spawn overflow agent");
            sessions.push(created);
            agent_panes.push(pane);
            assert!(d.report_agent_state(
                pane,
                report(
                    format!("agent-{index}"),
                    None,
                    false,
                    AgentState::Working,
                    1,
                ),
            ));
        }
        let client = 99;

        // Height 10 leaves nine content rows: sessions occupy 0..5 and agents
        // 5..9. Scrolling the AGENTS header advances only that section by the
        // established three-row wheel step.
        assert!(d.scroll_sidebar(client, sid, 5, 10, false));
        assert_eq!(
            d.sidebar_scroll.get(&client),
            Some(&SidebarScroll {
                sessions: 0,
                agents: 3,
            })
        );
        assert!(matches!(
            d.sidebar_pick_at(client, sid, 6, 10),
            Some(SidebarPick::Agent { pane, .. }) if pane == agent_panes[3]
        ));
        assert!(matches!(
            d.sidebar_pick_at(client, sid, 1, 10),
            Some(SidebarPick::Session(target)) if target == sessions[0]
        ));
        assert!(!d.scroll_sidebar(client, sid, 9, 10, false));
    }

    #[test]
    fn ensure_sidebar_session_visible_uses_the_projected_session_capacity() {
        let (mut d, sid, _pid) = spawn();
        let mut sessions = vec![sid];
        for index in 1..6 {
            let (created, _pane, _reader) = d
                .new_session(
                    format!("s{index}"),
                    Some(vec!["/bin/sh".to_string()]),
                    PtySize::new(80, 10),
                )
                .expect("spawn overflow session");
            sessions.push(created);
        }
        let target = sessions[5];
        let client = d
            .server
            .attach_client(sid, PtySize::new(80, 10))
            .expect("attach client");
        d.register_client(client);
        assert!(d.server.set_client_session(client, target));

        // The projected session section is five rows high: one header plus four
        // entries. Revealing index five therefore moves the offset to two.
        assert!(d.ensure_sidebar_session_visible(client, target));
        assert_eq!(
            d.sidebar_scroll.get(&client),
            Some(&SidebarScroll {
                sessions: 2,
                agents: 0,
            })
        );
        assert!(matches!(
            d.sidebar_pick_at(client, target, 4, 10),
            Some(SidebarPick::Session(picked)) if picked == target
        ));
        assert!(!d.ensure_sidebar_session_visible(client, target));
    }

    #[test]
    fn sidebar_width_keeps_half_the_terminal_for_tiny_content() {
        let (mut d, sid, _pid) = spawn();
        d.set_config(Config {
            sidebar: true,
            sidebar_width: 20,
            ..Default::default()
        });
        let client = d
            .server
            .attach_client(sid, PtySize::new(5, 10))
            .expect("attach client");
        assert_eq!(d.sidebar_width(sid), 2);
        d.set_sidebar_collapsed(sid, true);
        assert_eq!(d.sidebar_width(sid), 2);
        d.server.set_client_size(client, PtySize::new(1, 10));
        assert_eq!(d.sidebar_width(sid), 0);
    }

    #[test]
    fn narrow_sidebar_headers_never_paint_the_content_plane() {
        let (d, sid, _pid) = spawn();
        let mut screen = lumux_core::render::Screen::new(10, 5);
        let attrs = lumux_core::render::CellAttributes::default();
        for y in 0..4 {
            for x in 5..10 {
                screen.set_cell(x, y, lumux_core::render::Cell::new('P', attrs.clone()));
            }
        }

        d.render_sidebar(&mut screen, 1, sid, 5, 5);

        for y in 0..4 {
            for x in 5..10 {
                assert_eq!(
                    screen.cell(x, y).map(|cell| cell.str()),
                    Some("P"),
                    "sidebar paint escaped its five-column allocation at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn sidebar_labels_respect_display_width_and_preserve_the_border() {
        let (mut d, sid, pid) = spawn();
        d.server.session_mut(sid).unwrap().name = "界A".to_string();
        assert!(d.report_agent_state(pid, report("🤖e\u{301}", None, false, AgentState::Idle, 1)));

        let mut screen = lumux_core::render::Screen::new(5, 9);
        d.render_sidebar(&mut screen, 1, sid, 5, 9);

        // The two-cell CJK session name gets a styled spacer, then the ASCII
        // suffix; no label or ZWJ/combining agent name may consume the border.
        assert_eq!(screen.cell(0, 1).map(|cell| cell.str()), Some("界"));
        assert_eq!(screen.cell(1, 1).map(|cell| cell.str()), Some(" "));
        assert_eq!(screen.cell(2, 1).map(|cell| cell.str()), Some("A"));
        for y in 0..8 {
            assert_eq!(
                screen.cell(4, y).map(|cell| cell.str()),
                Some("│"),
                "sidebar text overwrote its border on row {y}"
            );
        }
    }

    #[test]
    fn one_column_sidebar_is_a_toggle_only_rail() {
        let (mut d, sid, pid) = spawn();
        d.set_config(Config {
            sidebar: true,
            ..Default::default()
        });
        let client = d
            .server
            .attach_client(sid, PtySize::new(3, 6))
            .expect("attach narrow client");
        d.register_client(client);
        assert_eq!(d.sidebar_width(sid), 1);
        assert!(d.report_agent_state(pid, report("claude", None, false, AgentState::Blocked, 1)));

        let mut screen = lumux_core::render::Screen::new(1, 6);
        d.render_sidebar(&mut screen, client, sid, 1, 6);
        assert_eq!(screen.cell(0, 0).map(|cell| cell.str()), Some("▶"));
        assert!(
            (1..5).all(|y| screen.cell(0, y).map(|cell| cell.str()) == Some("│")),
            "a one-column rail must not render session or agent glyphs"
        );
        assert!(d.sidebar_toggle_hit(sid, 0, 4));
    }

    #[test]
    fn remain_on_exit_clears_agent_status_from_kept_dead_pane() {
        let (mut d, sid, pid) = spawn();
        d.set_config(Config {
            remain_on_exit: true,
            ..Default::default()
        });
        d.report_agent_state(pid, report("claude", None, false, AgentState::Blocked, 1));

        assert!(
            d.pane_exited(sid, pid).is_none(),
            "dead pane stays in model"
        );
        assert!(d.is_pane_dead(pid));
        assert!(
            d.agent_status(pid).is_none(),
            "kept-dead panes must not retain live agent state"
        );
    }

    #[test]
    fn closing_a_window_clears_all_of_its_agent_statuses() {
        let (mut d, sid, first) = spawn();
        let size = PtySize::new(80, 24);
        let (second, _reader) = d
            .split_active(sid, lumux_core::model::SplitDir::Horizontal, size)
            .expect("spawn split pane")
            .expect("split pane");
        d.report_agent_state(first, report("claude", None, false, AgentState::Working, 1));
        d.report_agent_state(second, report("codex", None, false, AgentState::Blocked, 1));

        let (closed, _result) = d.close_active_window(sid);
        assert!(closed.contains(&first) && closed.contains(&second));
        assert!(d.agent_status(first).is_none());
        assert!(d.agent_status(second).is_none());
    }

    #[test]
    fn closing_a_session_clears_all_agent_statuses() {
        let (mut d, sid, pid) = spawn();
        d.report_agent_state(pid, report("claude", None, false, AgentState::Working, 1));

        assert_eq!(d.close_session(sid), vec![pid]);
        assert!(d.agent_status(pid).is_none());
    }
}
