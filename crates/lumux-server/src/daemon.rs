//! Daemon-side runtime state: the object tree plus the live per-pane emulators
//! and PTY handles. This is the bridge between `lumux_core`'s pure model and the
//! backend's real PTYs.
//!
//! Generic over a [`PtySystem`] so the identical logic runs on the unix backend
//! (dev/CI) and the Windows ConPTY backend (Phase 10).

use std::collections::BTreeMap;

use lumux_core::config::Config;
use lumux_core::copymode::CopyMode;
use lumux_core::grid::Grid;
use lumux_core::keymap::{CopyKey, Keymap};
use lumux_core::model::{CascadeResult, PaneId, Server, SessionId, SplitDir, WindowId};
use lumux_core::render::{compose, ClientRenderer, WindowView};
use lumux_core::traits::{Clipboard, Pty, PtySize, PtySystem, PtyWriter, ShellCommand};

/// Per-pane live state: the emulator grid and the PTY input/control handle.
pub struct LivePane<W: PtyWriter> {
    pub grid: Grid,
    pub writer: W,
    pub dead: bool,
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

/// Environment to inject into every spawned pane shell so it knows it is running
/// inside lumux (mirrors tmux's `$TMUX`). `LUMUX` carries the daemon's
/// listener path (pipe/socket) when known — like tmux's `socket,pid,session` —
/// falling back to `1`. `LUMUX_PANE` carries this pane's id. The client checks
/// `LUMUX` before creating a new session to avoid accidental nesting.
fn lumux_pane_env(pane: PaneId) -> Vec<(String, String)> {
    let listener = std::env::var("LUMUX_PIPE")
        .or_else(|_| std::env::var("LUMUX_SOCK"))
        .unwrap_or_else(|_| "1".to_string());
    vec![
        ("LUMUX".to_string(), listener),
        ("LUMUX_PANE".to_string(), pane.to_string()),
    ]
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
    dragging: BTreeMap<u64, Vec<bool>>,
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
    /// Latest self-reported agent status per pane (`lumux report-state`, wired
    /// into each agent's hooks). Live and transient: never persisted, and
    /// cleared when the pane's process exits (see `close_pane`). Surfaced by the
    /// sidebar and the session chooser.
    agent_status: BTreeMap<PaneId, lumux_core::agent::AgentStatus>,
    /// Per-session sidebar visibility override (tmux-style `:set sidebar on`).
    /// Absent = fall back to the config default. Session-global by design: under
    /// the shared PTY, one client's toggle reflows every client of the session.
    sidebar_on: BTreeMap<SessionId, bool>,
    /// Per-session sidebar collapse state. When shown, the sidebar can be either
    /// expanded (full width) or collapsed to a thin clickable rail. Absent =
    /// expanded. Independent of `sidebar_on` (fully off).
    sidebar_collapsed: BTreeMap<SessionId, bool>,
    clipboard: Box<dyn Clipboard>,
    config: Config,
}

/// Mouse text-selection drag state (tmux drag-to-copy). A left-press over a
/// selectable pane records `Armed` with the press cell; the first drag motion
/// promotes it to `Dragging`, entering copy-mode and starting the selection.
#[derive(Clone, Copy)]
enum MouseSel {
    /// Pressed but not yet moved; remembers where the drag would start.
    Armed { ox: u16, oy: u16 },
    /// Drag in progress: copy-mode is open and the selection is being extended.
    Dragging,
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

/// One rendered row of the sessions/agents sidebar, and (for interactive rows)
/// the target a click switches to. Built by `sidebar_rows` and consumed by both
/// the renderer and the click hit-test so they never drift.
#[derive(Clone, PartialEq, Eq)]
pub enum SidebarRow {
    /// A non-clickable section header ("SESSIONS" / "AGENTS").
    Header(&'static str),
    /// A blank spacer row.
    Blank,
    /// A session entry; clicking switches to it.
    Session {
        sid: SessionId,
        name: String,
        windows: usize,
        current: bool,
    },
    /// A pane running an agent; clicking switches to its session + window.
    Agent {
        sid: SessionId,
        wid: WindowId,
        agent: String,
        state: lumux_core::agent::AgentState,
        session_name: String,
    },
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
            agent_status: BTreeMap::new(),
            sidebar_on: BTreeMap::new(),
            sidebar_collapsed: BTreeMap::new(),
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
            env: lumux_pane_env(id),
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
            let Some(s) = self.server.session(sid) else { continue };
            let win_ids = s.window_ids();
            let active_window = win_ids.iter().position(|&w| w == s.active_window()).unwrap_or(0);
            let mut windows = Vec::new();
            for &wid in &win_ids {
                let Some(w) = s.window(wid) else { continue };
                let pane_ids = w.pane_ids();
                // Map each real PaneId -> dense snapshot id (its position).
                let id_of = |pid: PaneId| pane_ids.iter().position(|&p| p == pid).unwrap_or(0) as u32;
                let active_pane = pane_ids.iter().position(|&p| p == w.active_pane()).unwrap_or(0);
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
            let Some(s) = self.server.session_mut(sid) else { continue };
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
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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
        self.server.detach_client(client_id);
    }

    /// Open a rename prompt for a client, seeded with the current name so the
    /// user can edit rather than retype it (tmux prefix , / $).
    pub fn open_prompt(&mut self, client_id: u64, session: SessionId, target: PromptTarget) {
        let seed = match target {
            PromptTarget::Session => self.server.session(session).map(|s| s.name.clone()),
            PromptTarget::Window => self.server.session(session).and_then(|s| {
                s.window(s.active_window()).map(|w| w.name.clone())
            }),
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

    /// Commit a client's prompt: apply the typed name to the window or session,
    /// run a find, or — for the command-prompt — return the typed line for the
    /// event loop to parse and dispatch. An empty name is ignored (keeps the
    /// existing one, matching tmux). Returns `Some(line)` only for the
    /// command-prompt target.
    pub fn prompt_confirm(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        let p = self.prompt.remove(&client_id)?;
        let mut command_line = None;
        let name = p.buffer.trim().to_string();
        if !name.is_empty() {
            match p.target {
                PromptTarget::Session => {
                    if let Some(s) = self.server.session_mut(session) {
                        s.name = name;
                    }
                }
                PromptTarget::Window => {
                    if let Some(s) = self.server.session_mut(session) {
                        let wid = s.active_window();
                        if let Some(w) = s.window_mut(wid) {
                            w.set_name_manual(name);
                        }
                    }
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
                        Some(wid) => {
                            if let Some(s) = self.server.session_mut(session) {
                                s.focus_window(wid);
                            }
                        }
                        None => self.flash_message(client_id, format!("no window matching \"{name}\"")),
                    }
                }
                // The event loop owns command dispatch (it can spawn PTYs etc.).
                PromptTarget::Command => command_line = Some(p.buffer.trim().to_string()),
            }
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        command_line
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
        let Some(pid) = self.active_pane(session) else {
            return true;
        };
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
        let Some(pid) = self.active_pane(session) else {
            return true;
        };
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

    /// Pick the pane shown as `number` in the display-panes overlay (1-based as
    /// drawn, offset by base-index), focusing it. Always closes the overlay.
    pub fn pick_pane_number(&mut self, client_id: u64, session: SessionId, number: u32) {
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
                    w.focus_pane(pid);
                }
            }
        }
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
            .and_then(|sid| rows.iter().position(|r| matches!(r, TreeRow::Session(s) if *s == sid)))
            .unwrap_or(0);
        let _ = sessions; // (kept for clarity; rows already reflects the session list)
        self.choosing.insert(client_id, ChooseTree { cursor, ..tree });
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
        rows.get(tree.cursor.min(rows.len().saturating_sub(1))).copied()
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

    /// Confirm the choose-tree: pick the cursor row. A session row switches to
    /// that session; a window row switches AND targets that window (the event
    /// loop focuses it). Returns the pick so the caller can update its state.
    pub fn chooser_confirm(&mut self, client_id: u64) -> Option<ChooserPick> {
        let row = self.tree_cursor_row(client_id)?;
        self.choosing.remove(&client_id);
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
        let pick = match row {
            TreeRow::Session(sid) => ChooserPick::Session(sid),
            TreeRow::Window(sid, wid) => ChooserPick::Window(sid, wid),
        };
        let sid = match pick {
            ChooserPick::Session(s) | ChooserPick::Window(s, _) => s,
        };
        if self.server.set_client_session(client_id, sid) {
            // For a window pick, focus that window in the session.
            if let ChooserPick::Window(sid, wid) = pick {
                if let Some(s) = self.server.session_mut(sid) {
                    s.focus_window(wid);
                }
            }
            Some(pick)
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
    /// No-op when the active pane is on the alternate screen: a full-screen app
    /// (vim/less) owns the viewport and has no scrollback to browse, so copy-mode
    /// there would only show stale primary-screen history (tmux blocks it too).
    pub fn enter_copy_mode(&mut self, client_id: u64, session: SessionId) {
        if let Some(pid) = self.active_pane(session) {
            if let Some(p) = self.panes.get(&pid) {
                if p.grid.alt_screen() {
                    return;
                }
                self.copy.insert(client_id, CopyMode::enter(&p.grid));
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
        self.agent_status.remove(&pane);
        self.server.kill_pane(session, pane)
    }

    /// Record a pane's self-reported agent status (`lumux report-state`).
    pub fn set_agent_status(&mut self, pane: PaneId, status: lumux_core::agent::AgentStatus) {
        self.agent_status.insert(pane, status);
    }

    /// Clear a pane's agent status (the agent process exited but the pane lives).
    pub fn clear_agent_status(&mut self, pane: PaneId) {
        self.agent_status.remove(&pane);
    }

    /// The latest agent status reported for `pane`, if any.
    pub fn agent_status(&self, pane: PaneId) -> Option<&lumux_core::agent::AgentStatus> {
        self.agent_status.get(&pane)
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
        if self.config.remain_on_exit {
            if let Some(p) = self.panes.get_mut(&pane) {
                p.dead = true;
                // Repaint so the dead marker shows.
                for r in self.renderers.values_mut() {
                    r.invalidate();
                }
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
                s.window_ids()
                    .into_iter()
                    .find_map(|wid| s.window(wid).and_then(|w| w.pane(pane).map(|p| p.shell.clone())))
            })
            .unwrap_or_else(|| self.config.shell_argv(None).unwrap_or_default());
        let reader = self.spawn_pane(pane, &shell, size, None)?;
        for r in self.renderers.values_mut() {
            r.invalidate();
        }
        Ok(Some(reader))
    }

    /// Kill the active window of `session` (tmux `kill-window`): drop all its
    /// panes' live PTYs and remove the window from the model. Returns the closed
    /// pane ids (so the event loop can drop their pane->session mappings) and the
    /// cascade result (emptying the session closes it).
    pub fn close_active_window(&mut self, session: SessionId) -> (Vec<PaneId>, CascadeResult) {
        let Some(wid) = self
            .server
            .session(session)
            .map(|s| s.active_window())
        else {
            return (Vec::new(), CascadeResult::NotFound);
        };
        let (panes, result) = self.server.kill_window(session, wid);
        for pid in &panes {
            if let Some(p) = self.panes.get_mut(pid) {
                p.dead = true;
            }
            self.panes.remove(pid);
        }
        (panes, result)
    }

    /// Content viewport for `session`: the pane area to the right of any sidebar
    /// and above the status row. This is the single authority for the content
    /// plane's origin and extent — every pane hit-test, divider drag, and the
    /// compositor derive from it, so the sidebar offset (and the reserved status
    /// row) live in exactly one place.
    pub fn content_viewport(&self, session: SessionId) -> Option<lumux_core::layout::Rect> {
        let size = self.server.effective_size(session)?;
        let sidebar = self.sidebar_width(session).min(size.cols);
        Some(lumux_core::layout::Rect::new(
            sidebar,
            0,
            size.cols - sidebar,
            size.rows.saturating_sub(1),
        ))
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
        if self.sidebar_collapsed(session) {
            return Self::SIDEBAR_RAIL_WIDTH;
        }
        let cols = self
            .server
            .effective_size(session)
            .map(|s| s.cols)
            .unwrap_or(0);
        let max = cols / 2;
        self.config.sidebar_width.min(max).max(Self::SIDEBAR_RAIL_WIDTH)
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
        self.sidebar_collapsed.get(&session).copied().unwrap_or(false)
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

    /// Mouse-press: if (col,row) is on a split divider, remember it as the
    /// grabbed divider for `client_id` so a following drag resizes it. A press in
    /// open pane area records nothing, so plain click-drags don't resize.
    pub fn begin_drag(&mut self, client_id: u64, session: SessionId, col: u16, row: u16) {
        self.dragging.remove(&client_id);
        let Some(vp) = self.content_viewport(session) else {
            return;
        };
        let path = self
            .server
            .session(session)
            .and_then(|s| s.window(s.active_window()).and_then(|w| w.divider_at(col, row, vp)));
        if let Some(path) = path {
            self.dragging.insert(client_id, path);
        }
    }

    /// Mouse-drag: move the divider grabbed on press to follow the cursor, and
    /// re-fit the PTYs. No-op (returns false) if this client didn't grab one.
    pub fn drag_divider(&mut self, client_id: u64, session: SessionId, col: u16, row: u16) -> bool {
        let Some(path) = self.dragging.get(&client_id).cloned() else {
            return false;
        };
        let Some(vp) = self.content_viewport(session) else {
            return false;
        };
        let moved = self
            .server
            .session_mut(session)
            .and_then(|s| {
                let wid = s.active_window();
                s.window_mut(wid).map(|w| w.drag_divider(&path, col, row, vp))
            })
            .unwrap_or(false);
        if moved {
            if let Some(size) = self.server.effective_size(session) {
                self.resize_session(session, size);
            }
        }
        moved
    }

    /// Mouse-release: end any divider drag for this client.
    pub fn end_drag(&mut self, client_id: u64) {
        self.dragging.remove(&client_id);
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
        let Some(pid) = self.pane_at_screen(session, col, row) else {
            return;
        };
        if self.pane_on_alt_screen(pid) {
            return;
        }
        self.mouse_sel
            .insert(client_id, MouseSel::Armed { ox: col, oy: row });
    }

    /// Mouse-drag while a selection is armed/active. The first motion enters
    /// copy-mode, anchors the selection at the press cell, and extends it to the
    /// current cell; later motions just extend. Returns true when a text
    /// selection is live (so the caller skips divider-drag and repaints).
    pub fn mouse_sel_drag(&mut self, client_id: u64, session: SessionId, col: u16, row: u16) -> bool {
        match self.mouse_sel.get(&client_id).copied() {
            Some(MouseSel::Armed { ox, oy }) => {
                // Begin copy-mode on the active pane; bail if it refuses (e.g. an
                // app flipped to the alt screen between press and drag).
                if !self.in_copy_mode(client_id) {
                    self.enter_copy_mode(client_id, session);
                }
                if !self.in_copy_mode(client_id) {
                    self.mouse_sel.remove(&client_id);
                    return false;
                }
                let Some(origin) = self.screen_to_buffer(client_id, session, ox, oy) else {
                    self.mouse_sel.remove(&client_id);
                    return false;
                };
                let cur = self
                    .screen_to_buffer(client_id, session, col, row)
                    .unwrap_or(origin);
                if let (Some(pid), Some(cm)) =
                    (self.active_pane(session), self.copy.get_mut(&client_id))
                {
                    if let Some(grid) = self.panes.get(&pid).map(|p| &p.grid) {
                        cm.set_cursor(origin, grid);
                        cm.start_selection();
                        cm.set_cursor(cur, grid);
                    }
                }
                self.mouse_sel.insert(client_id, MouseSel::Dragging);
                if let Some(r) = self.renderers.get_mut(&client_id) {
                    r.invalidate();
                }
                true
            }
            Some(MouseSel::Dragging) => {
                if let Some(cur) = self.screen_to_buffer(client_id, session, col, row) {
                    if let (Some(pid), Some(cm)) =
                        (self.active_pane(session), self.copy.get_mut(&client_id))
                    {
                        if let Some(grid) = self.panes.get(&pid).map(|p| &p.grid) {
                            cm.set_cursor(cur, grid);
                        }
                    }
                }
                true
            }
            None => false,
        }
    }

    /// Whether a mouse text-selection drag is currently in progress.
    pub fn mouse_sel_active(&self, client_id: u64) -> bool {
        matches!(self.mouse_sel.get(&client_id), Some(MouseSel::Dragging))
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
    pub fn mouse_sel_finish(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        match self.mouse_sel.remove(&client_id) {
            Some(MouseSel::Dragging) => self.copy_yank(client_id, session),
            _ => None,
        }
    }

    /// Map a screen cell (col,row) to a position in the active pane's combined
    /// buffer, using the pane's rect and the client's copy-mode scroll offset.
    /// Clamps into the pane so a drag past an edge selects to the boundary.
    fn screen_to_buffer(
        &self,
        client_id: u64,
        session: SessionId,
        col: u16,
        row: u16,
    ) -> Option<lumux_core::copymode::Pos> {
        let (_pid, rect) = self.active_pane_rect(session)?;
        let cm = self.copy.get(&client_id)?;
        let rel_col = col.saturating_sub(rect.x).min(rect.cols.saturating_sub(1)) as usize;
        let rel_row = row.saturating_sub(rect.y).min(rect.rows.saturating_sub(1)) as usize;
        Some(lumux_core::copymode::Pos {
            row: cm.top() + rel_row,
            col: rel_col,
        })
    }

    /// The active pane id and its on-screen rectangle in the content viewport.
    fn active_pane_rect(&self, session: SessionId) -> Option<(PaneId, lumux_core::layout::Rect)> {
        let vp = self.content_viewport(session)?;
        let s = self.server.session(session)?;
        let w = s.window(s.active_window())?;
        let active = w.active_pane();
        let layout = match w.zoomed_pane() {
            Some(pid) => lumux_core::model::PaneNode::leaf(pid),
            None => w.layout.clone(),
        };
        let rect = *lumux_core::layout::compute(&layout, vp).get(&active)?;
        Some((active, rect))
    }

    /// The pane id at a screen point within the content area, or None on the
    /// status row / outside any pane. Used to decide whether a press is over a
    /// selectable pane.
    fn pane_at_screen(&self, session: SessionId, col: u16, row: u16) -> Option<PaneId> {
        let vp = self.content_viewport(session)?;
        if row >= vp.rows {
            return None;
        }
        let s = self.server.session(session)?;
        let w = s.window(s.active_window())?;
        let rects = lumux_core::layout::compute(&w.layout, vp);
        lumux_core::layout::pane_at(&rects, col, row)
    }

    /// Render the active window of `session` for `client_id`, returning VT bytes
    /// to send (empty if nothing changed).
    pub fn render_for_client(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        // The clock overlay takes over the whole screen when active.
        if self.clock.contains(&client_id) {
            return self.render_clock(client_id, session);
        }
        // The help overlay takes over the whole screen when active.
        if self.help.contains(&client_id) {
            return self.render_help(client_id, session);
        }
        // The session switcher likewise takes over the screen.
        if self.choosing.contains_key(&client_id) {
            return self.render_chooser(client_id, session);
        }
        // The paste-buffer chooser is a full-screen overlay too.
        if self.choosing_buffer.contains_key(&client_id) {
            return self.render_buffer_chooser(client_id, session);
        }
        // Copy-mode clients see the scrolled history view instead of live panes.
        if self.copy.contains_key(&client_id) {
            return self.render_copy_mode(client_id, session);
        }
        let size = self.server.effective_size(session)?;
        let s = self.server.session(session)?;
        let window = s.window(s.active_window())?;
        // When a pane is zoomed (tmux prefix z), render only that pane fullscreen
        // by swapping in a single-leaf layout; otherwise use the real split tree.
        let layout = match window.zoomed_pane() {
            Some(pid) => lumux_core::model::PaneNode::leaf(pid),
            None => window.layout.clone(),
        };
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
            self.render_sidebar(&mut screen, session, sidebar_w as u16, size.rows as usize);
        }
        self.paint_status(&mut screen, client_id, session);
        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
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
        let sidebar = self.sidebar_width(session).min(size.cols);
        let viewport = lumux_core::layout::Rect::new(
            sidebar,
            0,
            size.cols - sidebar,
            size.rows.saturating_sub(1),
        );
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

    /// A row in the sidebar's flattened layout, with the action clicking it
    /// performs. Built once and shared by `render_sidebar` and the click
    /// hit-test so what you see and what you click stay in lockstep.
    ///
    /// Positioned sidebar rows: `(screen_y, row)` pairs across the content height
    /// `content_h`. The two sections split the height 1:1 — sessions fill the top
    /// half, agents the bottom half — each with its own header and independent
    /// truncation, so a long session list can't crowd the agents out. Both the
    /// renderer and the click hit-test consume this, so they never drift.
    fn sidebar_layout(&self, session: SessionId, content_h: usize) -> Vec<(usize, SidebarRow)> {
        let mut out: Vec<(usize, SidebarRow)> = Vec::new();
        if content_h == 0 {
            return out;
        }
        // Top half for sessions, bottom half for agents (1:1). With an odd
        // height the sessions section gets the extra row.
        let agents_start = content_h.div_ceil(2);

        // --- Sessions section: header at row 0, entries below. ---
        out.push((0, SidebarRow::Header("SESSIONS")));
        let mut y = 1;
        for sid in self.server.session_ids() {
            if y >= agents_start.saturating_sub(0) {
                break;
            }
            let Some(s) = self.server.session(sid) else { continue };
            out.push((
                y,
                SidebarRow::Session {
                    sid,
                    name: s.name.clone(),
                    windows: s.window_count(),
                    current: sid == session,
                },
            ));
            y += 1;
        }

        // --- Agents section: header at agents_start, entries below. ---
        if agents_start < content_h {
            out.push((agents_start, SidebarRow::Header("AGENTS")));
            let mut y = agents_start + 1;
            'outer: for sid in self.server.session_ids() {
                let Some(s) = self.server.session(sid) else { continue };
                for wid in s.window_ids() {
                    let Some(w) = s.window(wid) else { continue };
                    for pid in w.pane_ids() {
                        if let Some(status) = self.agent_status.get(&pid) {
                            if y >= content_h {
                                break 'outer;
                            }
                            out.push((
                                y,
                                SidebarRow::Agent {
                                    sid,
                                    wid,
                                    agent: status.agent.clone(),
                                    state: status.state,
                                    session_name: s.name.clone(),
                                },
                            ));
                            y += 1;
                        }
                    }
                }
            }
        }
        out
    }

    /// Paint the sessions/agents sidebar into the reserved columns `[0, width)`.
    /// `total_rows` is the full screen height (the status row is left for
    /// `paint_status`). When collapsed the sidebar is a thin rail with just the
    /// expand button; otherwise it's the full two-section list. A themed vertical
    /// border closes the right edge.
    fn render_sidebar(
        &self,
        screen: &mut lumux_core::render::Screen,
        session: SessionId,
        width: u16,
        total_rows: usize,
    ) {
        let w = width as usize;
        if w == 0 {
            return;
        }
        let content_h = total_rows.saturating_sub(1);
        let collapsed = self.sidebar_collapsed(session);

        let panel = self.sidebar_panel_attrs();
        let border = self.sidebar_border_attrs();

        // Fill the sidebar background so it reads as a panel, not floating text.
        for y in 0..content_h {
            for x in 0..w {
                screen.set_cell(
                    x,
                    y,
                    lumux_core::render::Cell::new(' ', panel.clone()),
                );
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
        for (y, row) in self.sidebar_layout(session, content_h) {
            match row {
                SidebarRow::Header(label) => {
                    // Header bar: the label plus a collapse button (◀) right-aligned
                    // on the first header row only.
                    self.fill_row(screen, y, text_w, &header);
                    screen.write_str(0, y, label, &header);
                    if y == 0 && text_w >= 1 {
                        screen.write_str(text_w - 1, y, "◀", &header);
                    }
                }
                SidebarRow::Blank => {}
                SidebarRow::Session {
                    name,
                    windows,
                    current: is_cur,
                    ..
                } => {
                    let line = format!("{name} · {windows}w");
                    let clipped: String = line.chars().take(text_w).collect();
                    if is_cur {
                        self.fill_row(screen, y, text_w, &current);
                        screen.write_str(0, y, &clipped, &current);
                    } else {
                        screen.write_str(0, y, &clipped, &panel);
                    }
                }
                SidebarRow::Agent {
                    agent,
                    state,
                    session_name,
                    ..
                } => {
                    let glyph = Self::agent_glyph(state);
                    let gattr = self.agent_glyph_attrs(state, &panel);
                    // "● agent @sess" — the glyph gets a state color, the rest the
                    // panel style.
                    screen.write_str(0, y, &glyph.to_string(), &gattr);
                    let rest = format!(" {agent} @{session_name}");
                    let clipped: String = rest.chars().take(text_w.saturating_sub(1)).collect();
                    screen.write_str(1, y, &clipped, &panel);
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
    fn agent_glyph(state: lumux_core::agent::AgentState) -> char {
        use lumux_core::agent::AgentState;
        match state {
            AgentState::Blocked => '●',
            AgentState::Working => '●',
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
            .filter_map(|pid| self.agent_status.get(&pid).map(|s| s.state))
            .max_by_key(|s| s.urgency())
    }

    /// Hit-test a click at sidebar row `y` (0-based screen row) for `session`,
    /// returning what to switch to — a session, or a session+window for an agent
    /// row. Header/blank/out-of-range rows return None. Uses the same positioned
    /// `sidebar_layout` the renderer draws, so clicks land on what's shown.
    pub fn sidebar_pick_at(&self, session: SessionId, y: usize, height: usize) -> Option<ChooserPick> {
        let content_h = height.saturating_sub(1);
        let row = self
            .sidebar_layout(session, content_h)
            .into_iter()
            .find(|(ry, _)| *ry == y)
            .map(|(_, r)| r)?;
        match row {
            SidebarRow::Session { sid, .. } => Some(ChooserPick::Session(sid)),
            SidebarRow::Agent { sid, wid, .. } => Some(ChooserPick::Window(sid, wid)),
            SidebarRow::Header(_) | SidebarRow::Blank => None,
        }
    }

    /// Whether a click at (col,row) inside the sidebar hit the collapse/expand
    /// toggle button. When collapsed the whole rail toggles (the button is the
    /// rail); when expanded it's the ◀ glyph at the top-right of the header.
    pub fn sidebar_toggle_hit(&self, session: SessionId, col: u16, row: u16, height: usize) -> bool {
        let w = self.sidebar_width(session);
        if w == 0 || col >= w {
            return false;
        }
        let _ = height;
        if self.sidebar_collapsed(session) {
            // The whole rail is the expand button.
            true
        } else {
            // The ◀ button sits at text_w-1 on header row 0.
            let text_w = w.saturating_sub(1);
            row == 0 && text_w >= 1 && col == text_w - 1
        }
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
    fn paint_status(
        &self,
        screen: &mut lumux_core::render::Screen,
        client_id: u64,
        session: SessionId,
    ) {
        use lumux_core::render::{Justify, StyledStatus};
        use lumux_core::status::{self, StatusContext};

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
            return;
        }

        // A pending display-message takes over the whole row.
        if let Some(msg) = self.message.get(&client_id) {
            self.paint_message_row(screen, msg);
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
        let (centre, _) = self.window_list_segment(s, base_idx, &base, &ctx);
        let styled = StyledStatus {
            left: status::format(left_fmt, &ctx),
            centre,
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
            right: status::format(&self.config.status_right, &ctx),
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
        let layout = match window.zoomed_pane() {
            Some(pid) => lumux_core::model::PaneNode::leaf(pid),
            None => window.layout.clone(),
        };
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

        // The active pane's rectangle, into which we paint the scrolled view.
        let viewport =
            lumux_core::layout::Rect::new(sidebar, 0, size.cols - sidebar, content_rows as u16);
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
            screen.set_cursor(Some((cx.min(cols.saturating_sub(1)), cy.min(content_rows.saturating_sub(1)))));
        }

        // Copy-mode status line across the reserved bottom row. While a search
        // query is being typed, show it as a `/query` (or `?query`) prompt so
        // the user sees what they're searching for, like tmux.
        if let Some((prefix, query)) = self.search_prompt(client_id) {
            let line = format!("{prefix}{query}");
            screen.status_line(rows.saturating_sub(1), &line);
            // Park the cursor at the end of the query so typing feels live.
            let cx = line.chars().count().min(cols.saturating_sub(1));
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
        let first = cursor.saturating_sub(list_rows.saturating_sub(1)).min(max_first);
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
                    let Some(s) = self.server.session(*sid) else { continue };
                    let marker = if tree.expanded.contains(sid) { "▾" } else { "▸" };
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
                    let Some(s) = self.server.session(*sid) else { continue };
                    let Some(w) = s.window(*wid) else { continue };
                    let active = w.id == s.active_window();
                    let idx = window_index(s, *wid) + self.config.base_index;
                    let mark = if active { "*" } else { "" };
                    // Prefix the most-urgent agent glyph among the window's panes,
                    // so the chooser shows the same status the sidebar does.
                    let glyph = self
                        .window_agent_state(*sid, *wid)
                        .map(|st| format!("{} ", Self::agent_glyph(st)))
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
                screen.write_plain(0, y, &line);
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
        let Some(s) = self.server.session(sid) else { return };
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
            lumux_core::render::blit_window_layout(
                screen,
                x,
                1,
                w,
                content_h,
                &win.layout,
                &grids,
            );
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
                screen.write_plain(0, y, &line);
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
                    let clipped: String = line
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .take(pw)
                        .collect();
                    screen.write_plain(preview_x, row, &clipped);
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
            screen.write_plain(x, h.saturating_sub(1), &more);
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
        let sidebar = self.sidebar_width(session).min(size.cols);
        let viewport = lumux_core::layout::Rect::new(
            sidebar,
            0,
            size.cols - sidebar,
            size.rows.saturating_sub(1), // status bar row
        );
        if let Some(w) = s.window(s.active_window()) {
            // A zoomed pane fills the whole content area; otherwise lay out the
            // real split tree. This must mirror render_for_client's choice so the
            // PTY dimensions match what the client actually sees.
            let layout = match w.zoomed_pane() {
                Some(pid) => lumux_core::model::PaneNode::leaf(pid),
                None => w.layout.clone(),
            };
            let rects = lumux_core::layout::compute(&layout, viewport);
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

    /// Re-fit the panes of EVERY window in `session` to `size` (not just the
    /// active one). Needed after break-pane, which changes two windows at once:
    /// the new window the pane moved to, and the source window whose remaining
    /// panes grew. A plain [`resize_session`] only refits the active window, so
    /// the inactive source window's PTYs would stay mis-sized until refocused.
    pub fn resize_all_windows(&mut self, session: SessionId, size: PtySize) {
        let Some(s) = self.server.session(session) else {
            return;
        };
        let sidebar = self.sidebar_width(session).min(size.cols);
        let viewport =
            lumux_core::layout::Rect::new(sidebar, 0, size.cols - sidebar, size.rows.saturating_sub(1));
        // Collect (pane, rect) for every window first to avoid borrow conflicts.
        let mut fits: Vec<(PaneId, lumux_core::layout::Rect)> = Vec::new();
        for wid in s.window_ids() {
            if let Some(w) = s.window(wid) {
                let layout = match w.zoomed_pane() {
                    Some(pid) => lumux_core::model::PaneNode::leaf(pid),
                    None => w.layout.clone(),
                };
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
        PaneNode::Split { dir, ratio, first, second } => PaneNode::Split {
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
    use lumux_core::agent::{AgentState, AgentStatus};
    use lumux_core::traits::PtySize;

    fn spawn() -> (Daemon<UnixPtySystem>, SessionId, PaneId) {
        let mut d = Daemon::new(UnixPtySystem);
        let (sid, pid, _reader) = d
            .new_session("t", Some(vec!["/bin/sh".to_string()]), PtySize::new(80, 24))
            .expect("spawn session");
        (d, sid, pid)
    }

    #[test]
    fn set_and_get_agent_status() {
        let (mut d, _sid, pid) = spawn();
        assert!(d.agent_status(pid).is_none(), "no status before any report");
        d.set_agent_status(pid, AgentStatus::new("claude", AgentState::Working));
        let got = d.agent_status(pid).expect("status after report");
        assert_eq!(got.agent, "claude");
        assert_eq!(got.state, AgentState::Working);
        // A later report overwrites (sticky last-write).
        d.set_agent_status(pid, AgentStatus::new("claude", AgentState::Blocked));
        assert_eq!(d.agent_status(pid).unwrap().state, AgentState::Blocked);
    }

    #[test]
    fn closing_a_pane_clears_its_agent_status() {
        let (mut d, sid, pid) = spawn();
        d.set_agent_status(pid, AgentStatus::new("claude", AgentState::Working));
        assert!(d.agent_status(pid).is_some());
        // Pane death (its process exited) must drop the now-stale status.
        let _ = d.close_pane(sid, pid);
        assert!(
            d.agent_status(pid).is_none(),
            "close_pane must clear the pane's agent status"
        );
    }
}
