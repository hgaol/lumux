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
use lumux_core::model::{CascadeResult, PaneId, Server, SessionId, SplitDir};
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
    /// Clients in the session switcher, with their current highlighted index.
    choosing: BTreeMap<u64, usize>,
    /// Clients with an open text prompt (rename-window/-session): the target and
    /// the buffer typed so far.
    prompt: BTreeMap<u64, Prompt>,
    /// Clients mid-divider-drag: the path to the divider grabbed on mouse-press,
    /// so subsequent drag motion moves that same divider (even off its line).
    dragging: BTreeMap<u64, Vec<bool>>,
    clipboard: Box<dyn Clipboard>,
    config: Config,
}

/// An open rename prompt: what it renames, plus the in-progress text.
#[derive(Clone)]
pub struct Prompt {
    pub target: PromptTarget,
    pub buffer: String,
}

/// What an open prompt will rename when confirmed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptTarget {
    Window,
    Session,
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
            dragging: BTreeMap::new(),
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

    /// Feed PTY output bytes into a pane's emulator. Returns true if the output
    /// rang the terminal bell (BEL), so the caller can notify clients.
    ///
    /// Also drains any terminal-query replies the emulator produced (cursor
    /// position / device status / device attributes) and writes them straight
    /// back to this pane's PTY — i.e. to the shell's stdin — so line editors like
    /// PSReadLine that query the cursor get their answer.
    pub fn feed_pane(&mut self, id: PaneId, bytes: &[u8]) -> bool {
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
            p.grid.take_bell()
        } else {
            false
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
        self.help_offset.remove(&client_id);
        self.choosing.remove(&client_id);
        self.prompt.remove(&client_id);
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

    /// Commit a client's prompt: apply the typed name to the window or session.
    /// An empty name is ignored (keeps the existing one, matching tmux).
    pub fn prompt_confirm(&mut self, client_id: u64, session: SessionId) {
        let Some(p) = self.prompt.remove(&client_id) else {
            return;
        };
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
                            w.name = name;
                        }
                    }
                }
            }
        }
        if let Some(r) = self.renderers.get_mut(&client_id) {
            r.invalidate();
        }
    }

    /// Toggle the help overlay for a client (tmux prefix ?). Showing it the
    /// first time opens it (scrolled to the top); pressing it again (or q /
    /// Escape) closes it.
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
            // Copy-mode ended: bring the keymap back to Normal so subsequent keys
            // go to the shell (mirrors the keyboard `q`/Escape reset path).
            if let Some(k) = self.keymaps.get_mut(&client_id) {
                k.reset();
            }
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

    /// Whether `pid`'s pane is currently on the alternate screen (a full-screen
    /// app like vim/less or a TUI agent owns the viewport). Such panes have no
    /// scrollback to browse, so a mouse wheel must be translated into arrow-key
    /// input for the app rather than entering copy-mode.
    pub fn pane_on_alt_screen(&self, pid: PaneId) -> bool {
        self.panes.get(&pid).is_some_and(|p| p.grid.alt_screen())
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
        self.server.kill_pane(session, pane)
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

    /// Content viewport for `session` (full effective size minus the status row).
    fn content_viewport(&self, session: SessionId) -> Option<lumux_core::layout::Rect> {
        let size = self.server.effective_size(session)?;
        Some(lumux_core::layout::Rect::new(
            0,
            0,
            size.cols,
            size.rows.saturating_sub(1),
        ))
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
        };
        // Compose panes without a built-in status row, but reserve the bottom
        // row so panes don't extend into it; we then paint our own styled status
        // (or a transient message) onto that reserved row.
        let mut screen = compose(
            (size.cols as usize, size.rows as usize),
            &view,
            None,
            true,
        );
        self.paint_status(&mut screen, client_id, session);
        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
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
            let label = match p.target {
                PromptTarget::Window => "rename-window",
                PromptTarget::Session => "rename-session",
            };
            let line = format!("({label}) {}", p.buffer);
            screen.status_line(screen.dimensions().1.saturating_sub(1), &line);
            return;
        }

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
            time: now_parts(),
        };
        let base = StyledStatus::base_attrs(&self.config.status_bg, &self.config.status_fg);
        let left_fmt = if self.config.status_left.is_empty() {
            &self.config.status_format
        } else {
            &self.config.status_left
        };
        let wids = s.window_ids();
        let entries: Vec<status::WindowEntry> = wids
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
        let styled = StyledStatus {
            left: status::format(left_fmt, &ctx),
            centre: status::window_list(&entries, &base),
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
        for (pos, start, end) in status::window_list_hit_ranges(&entries) {
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
        };
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let content_rows = rows.saturating_sub(1);
        let mut screen = compose((cols, rows), &view, None, true);

        // The active pane's rectangle, into which we paint the scrolled view.
        let viewport = lumux_core::layout::Rect::new(0, 0, size.cols, content_rows as u16);
        let rect = *lumux_core::layout::compute(&layout, viewport).get(&active)?;
        let grid = &self.panes.get(&active)?.grid;
        let cm = self.copy.get(&client_id)?;
        let top = cm.top();

        // Overpaint only the active pane's rect with its scrolled-back rows,
        // clipped to the rect so it never bleeds into a neighbor.
        let (ox, oy) = (rect.x as usize, rect.y as usize);
        for vy in 0..rect.rows as usize {
            // Clear the rect row first (scrolled history may be shorter).
            for vx in 0..rect.cols as usize {
                screen.set_char(ox + vx, oy + vy, ' ');
            }
            if let Some(row) = grid.combined_row(top + vy) {
                let text = row.to_string_full();
                for (vx, ch) in text.chars().take(rect.cols as usize).enumerate() {
                    screen.set_char(ox + vx, oy + vy, ch);
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

        // Copy-mode status line across the reserved bottom row.
        let label = if cm.has_selection() {
            "-- COPY (selecting) --  arrows/PgUp/PgDn move, Enter yanks, q quits"
        } else {
            "-- COPY --  arrows/PgUp/PgDn move, Space selects, q quits"
        };
        screen.status_line(rows.saturating_sub(1), label);

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

    /// Render the session switcher overlay (tmux prefix s): a list of sessions
    /// on the left, and on the right a live preview of *every window* in the
    /// highlighted session (tmux's choose-tree). Each window gets a header
    /// (`index:name`, the active one marked `*`) and a clipped live view of its
    /// active pane, stacked top to bottom and sharing the preview height.
    fn render_chooser(&mut self, client_id: u64, session: SessionId) -> Option<String> {
        use lumux_core::render::Screen;
        let size = self.server.effective_size(session)?;
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let mut screen = Screen::new(cols, rows);
        let sel = *self.choosing.get(&client_id)?;
        let sids = self.server.session_ids();

        // Left list column: about a third of the width, clamped to a sane range
        // (but never wider than the screen, leaving room for a divider+preview).
        let list_w = (cols / 3).clamp(20, 40).min(cols.saturating_sub(2));
        let max_y = rows.saturating_sub(1);

        screen.write_plain(0, 0, "choose a session");
        for (i, sid) in sids.iter().enumerate() {
            let y = 2 + i;
            if y >= max_y {
                break;
            }
            let Some(s) = self.server.session(*sid) else {
                continue;
            };
            // Clip the label to the list column so it doesn't bleed into the
            // preview region.
            let mut line = format!("{}: {} ({}w)", i, s.name, s.window_count());
            if line.chars().count() > list_w {
                line = line.chars().take(list_w).collect();
            }
            if i == sel {
                screen.status_line_width(y, &line, list_w);
            } else {
                screen.write_plain(0, y, &line);
            }
        }

        // Divider + live preview of every window in the highlighted session.
        let div_x = list_w;
        let preview_x = list_w + 1;
        if preview_x < cols {
            screen.vline(div_x, 0, max_y, &Default::default());
            let pw = cols - preview_x;
            let ph = max_y; // preview area height (bottom row is the mode line)
            self.render_session_preview(&mut screen, sids.get(sel).copied(), preview_x, pw, ph);
        }

        screen.status_line(max_y, "-- SESSIONS --  Up/Down or digit, Enter selects, Esc cancels");
        screen.set_cursor(None);

        let renderer = self.renderers.get_mut(&client_id)?;
        Some(renderer.render(screen))
    }

    /// Paint a stacked, labeled preview of every window in `preview_sid` into the
    /// region at column `x`, width `w`, height `h`. Each window gets a one-row
    /// header (`index:name`, active marked `*`) and the remaining rows show a
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
                if let Some(p) = self.panes.get(&win.active_pane()) {
                    screen.blit_grid(x, top + 1, w, content_h, &p.grid);
                }
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
        // and grid in step with the layout rectangles.
        let viewport = lumux_core::layout::Rect::new(
            0,
            0,
            size.cols,
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
fn window_index(session: &lumux_core::model::Session, wid: lumux_core::model::WindowId) -> u32 {
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
