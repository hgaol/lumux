//! The daemon control loop — threads + channels, no async runtime.
//!
//! Everything lumux touches (PTYs via portable-pty, sockets) is blocking I/O, so
//! the natural model is one thread per blocking source funneling [`Msg`]s into a
//! single control loop over an `mpsc` channel. The control loop owns all
//! [`Daemon`] state, so every model/grid mutation is serialized without locks.
//!
//! Threads:
//! - one accept thread per listener,
//! - one reader thread per client connection (socket -> ClientInput),
//! - one writer thread per client (ServerMsg channel -> socket),
//! - one reader thread per pane (PTY output -> PaneOutput).
//!
//! Generic over the backend, so Phase 10's ConPTY backend reuses it verbatim.

use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc::{channel, Sender};

use lumux_core::copymode::osc52;
use lumux_core::keymap::{Action, BufferKey, CopyKey, PromptKey, Reaction, SearchKey, SessionKey};
use lumux_core::layout::Direction;
use lumux_core::model::{CascadeResult, PaneId, SessionId, SplitDir};
use lumux_core::proto::{encode, ClientMsg, Command, Event, ServerMsg};
use lumux_core::traits::{FrameReader, FrameWriter, Listener, Pty, PtySize, PtySystem, Transport};

use crate::daemon::Daemon;

/// Ratio step per keyboard resize-pane keypress (~5% of the split each press).
const RESIZE_STEP: f32 = 0.05;

/// Messages funneled into the control loop from all source threads.
pub enum Msg {
    ClientConnected {
        first: ClientMsg,
        out: Sender<ServerMsg>,
        reply: Sender<u64>,
    },
    ClientInput {
        client_id: u64,
        msg: ClientMsg,
    },
    ClientGone {
        client_id: u64,
    },
    PaneOutput {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    PaneExited {
        pane: PaneId,
    },
    /// Periodic timer: poll live panes for exited children (ConPTY may not
    /// deliver read-EOF when a shell exits, so EOF alone can't be relied on).
    Tick,
}

struct ClientHandle {
    out: Sender<ServerMsg>,
    session: SessionId,
    /// Bytes of an SGR mouse report that arrived truncated at a frame boundary
    /// (SSH/TCP can split one report across reads). Held here and prepended to
    /// the next frame so the report is reassembled instead of leaking as text.
    pending_mouse: Vec<u8>,
}

/// State owned exclusively by the control loop.
struct Loop<S: PtySystem> {
    daemon: Daemon<S>,
    clients: HashMap<u64, ClientHandle>,
    pane_session: HashMap<PaneId, SessionId>,
    tx: Sender<Msg>,
    /// Last time auto-save wrote the state file, to throttle to ~every 15s.
    last_autosave: std::time::Instant,
    /// Where the session snapshot is saved/restored (tmux-resurrect).
    state_path: std::path::PathBuf,
}

/// Run the control loop until no sessions and no clients remain. Spawns one
/// accept thread for `listener` and blocks driving the loop.
pub fn run<S, L>(pty_system: S, listener: L) -> std::io::Result<()>
where
    S: PtySystem + 'static,
    <S::Pty as Pty>::Reader: Send + 'static,
    L: Listener + 'static,
{
    run_with_config(pty_system, listener, lumux_core::config::Config::default())
}

/// Like [`run`] but seeds the daemon with an initial config (prefix, bindings,
/// shell profiles, scrollback).
pub fn run_with_config<S, L>(
    pty_system: S,
    listener: L,
    config: lumux_core::config::Config,
) -> std::io::Result<()>
where
    S: PtySystem + 'static,
    <S::Pty as Pty>::Reader: Send + 'static,
    L: Listener + 'static,
{
    run_with_config_at(pty_system, listener, config, crate::state_path())
}

/// Like [`run_with_config`] but with an explicit session-state file path. Used by
/// tests (which need independent, controllable state files) and by callers that
/// override the default location.
pub fn run_with_config_at<S, L>(
    pty_system: S,
    listener: L,
    config: lumux_core::config::Config,
    state_path: std::path::PathBuf,
) -> std::io::Result<()>
where
    S: PtySystem + 'static,
    <S::Pty as Pty>::Reader: Send + 'static,
    L: Listener + 'static,
{
    let (tx, rx) = channel::<Msg>();
    spawn_accept(listener, tx.clone());
    spawn_ticker(tx.clone());

    let mut daemon = Daemon::new(pty_system);
    daemon.set_config(config);
    let mut lp = Loop {
        daemon,
        clients: HashMap::new(),
        pane_session: HashMap::new(),
        tx: tx.clone(),
        last_autosave: std::time::Instant::now(),
        state_path,
    };
    // Hold one tx so the loop never sees a disconnected channel while idle.
    drop(tx);

    // Restore saved sessions (tmux-resurrect) before serving clients, when
    // persistence is enabled and a state file exists. Restored sessions make the
    // daemon non-empty, so guard the empty-exit check with `served`.
    let mut served = false;
    if lp.daemon.persist_enabled() && lp.restore_from_disk() {
        served = true;
    }

    // The daemon auto-exits once it has served at least one client and then goes
    // idle (no sessions, no clients). `served` gates this so the periodic Tick —
    // which now drives the loop before any client connects — can't trip the
    // emptiness check at startup and kill a freshly-bound daemon.
    for msg in rx {
        if matches!(msg, Msg::ClientConnected { .. }) {
            served = true;
        }
        lp.handle(msg);
        if served && lp.daemon.server.is_empty() && lp.clients.is_empty() {
            break;
        }
    }
    Ok(())
}

impl<S: PtySystem + 'static> Loop<S>
where
    <S::Pty as Pty>::Reader: Send + 'static,
{
    /// Restore saved sessions from the on-disk state file (tmux-resurrect). Reads
    /// [`crate::state_path`], decodes it, and rebuilds each session — spawning a
    /// PTY + reader thread per pane and recording its pane→session mapping.
    /// Returns true if at least one session was restored. A missing, empty, or
    /// version-mismatched file is a no-op (false). Panes spawn at a default size;
    /// they refit when the first client attaches and triggers a resize.
    fn restore_from_disk(&mut self) -> bool {
        let path = self.state_path.clone();
        let Ok(bytes) = std::fs::read(&path) else {
            return false;
        };
        let Some(state) = lumux_core::persist::StateFile::decode(&bytes) else {
            return false;
        };
        let size = PtySize::new(80, 24);
        let mut restored = 0;
        for snap in &state.sessions {
            if let Some((sid, readers)) = self.daemon.restore_session(snap, size) {
                for (pid, reader) in readers {
                    self.pane_session.insert(pid, sid);
                    spawn_pane_reader(pid, reader, self.tx.clone());
                }
                restored += 1;
            }
        }
        if restored > 0 {
            tracing::info!("restored {restored} session(s) from {}", path.display());
        }
        restored > 0
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::ClientConnected { first, out, reply } => self.on_connect(first, out, reply),
            Msg::ClientInput { client_id, msg } => self.on_input(client_id, msg),
            Msg::ClientGone { client_id } => {
                // Detach != kill: panes keep running. That is persistence.
                self.daemon.unregister_client(client_id);
                self.clients.remove(&client_id);
            }
            Msg::PaneOutput { pane, bytes } => {
                let feed = self.daemon.feed_pane(pane, &bytes);
                if let Some(sid) = self.pane_session.get(&pane).copied() {
                    if feed.bell {
                        // Forward a bell to each attached client (tmux's default
                        // visual-bell-off behavior: the client emits a BEL so the
                        // user's own terminal flashes/beeps per its settings).
                        for id in self.session_clients(sid) {
                            if let Some(h) = self.clients.get(&id) {
                                let _ = h.out.send(ServerMsg::Event(Event::Bell));
                            }
                        }
                    }
                    // An app in the pane copied via OSC 52 (e.g. Claude Code's own
                    // selection). Re-emit it to each attached client's terminal so
                    // the user's local clipboard is set — tmux `set-clipboard`.
                    if let Some(text) = &feed.clipboard {
                        let seq = osc52(text);
                        for id in self.session_clients(sid) {
                            if let Some(h) = self.clients.get(&id) {
                                let _ = h.out.send(ServerMsg::Frame(seq.clone()));
                            }
                        }
                    }
                    self.render_session(sid);
                }
            }
            Msg::PaneExited { pane } => {
                if let Some(&sid) = self.pane_session.get(&pane) {
                    self.handle_pane_exited(sid, pane);
                }
            }
            Msg::Tick => {
                // Reap children that exited without the reader seeing EOF (ConPTY).
                for pane in self.daemon.reap_exited_panes() {
                    if let Some(&sid) = self.pane_session.get(&pane) {
                        self.handle_pane_exited(sid, pane);
                    }
                }
                // Auto-save the session snapshot when persistence is on, throttled
                // to ~every 15s so we don't hammer the disk on every 250ms tick.
                if self.daemon.persist_enabled()
                    && self.last_autosave.elapsed() >= std::time::Duration::from_secs(15)
                    && !self.daemon.server.is_empty()
                {
                    let _ = self.daemon.save_state(&self.state_path);
                    self.last_autosave = std::time::Instant::now();
                }
            }
        }
    }

    fn on_connect(&mut self, first: ClientMsg, out: Sender<ServerMsg>, reply: Sender<u64>) {
        let (session, size) = match self.resolve_attach(&first) {
            Ok(v) => v,
            Err(msg) => {
                // Reject the client with the specific reason (duplicate session
                // name, spawn failure, …) instead of attaching.
                let _ = out.send(ServerMsg::Error(msg));
                let _ = reply.send(0);
                return;
            }
        };
        let Some(client_id) = self.daemon.server.attach_client(session, size) else {
            let _ = reply.send(0);
            return;
        };
        self.daemon.register_client(client_id);
        let _ = out.send(ServerMsg::Attached {
            client_id,
            size: size.into(),
        });
        // Turn on mouse reporting in the client's terminal if configured.
        if self.daemon.mouse_enabled() {
            let _ = out.send(ServerMsg::Frame(
                lumux_core::mouse::ENABLE.as_bytes().to_vec(),
            ));
        }
        self.clients.insert(
            client_id,
            ClientHandle {
                out,
                session,
                pending_mouse: Vec::new(),
            },
        );
        let _ = reply.send(client_id);
        self.render_client(client_id);
    }

    /// Determine the session+size for an attach/new-session first message,
    /// spawning the session if needed. Returns Err(message) to reject the client
    /// with a specific reason: a spawn failure (bad shell argv), or a duplicate
    /// session name on new-session (tmux rejects these too).
    fn resolve_attach(&mut self, first: &ClientMsg) -> Result<(SessionId, PtySize), String> {
        match first {
            ClientMsg::Attach { session, size } => {
                let sz: PtySize = (*size).into();
                let existing = match session {
                    Some(name) => self.daemon.server.find_session_by_name(name),
                    None => self.daemon.server.session_ids().first().copied(),
                };
                if let Some(sid) = existing {
                    Ok((sid, sz))
                } else {
                    let name = session.clone().unwrap_or_else(|| "0".into());
                    let sid = self
                        .spawn_session(name, None, sz)
                        .ok_or_else(|| "failed to start session (check the shell command)".to_string())?;
                    Ok((sid, sz))
                }
            }
            ClientMsg::NewSession { name, shell, size } => {
                let sz: PtySize = (*size).into();
                let shell = shell.clone().map(|s| vec![s]);
                let name = name.clone().unwrap_or_else(|| "0".into());
                // tmux refuses `new-session -s <name>` when <name> already
                // exists ("duplicate session: <name>"); match that rather than
                // silently creating a second session with the same name.
                if self.daemon.server.find_session_by_name(&name).is_some() {
                    return Err(format!("duplicate session: {name}"));
                }
                let sid = self
                    .spawn_session(name, shell, sz)
                    .ok_or_else(|| "failed to start session (check the shell command)".to_string())?;
                Ok((sid, sz))
            }
            _ => Err("malformed first message".to_string()),
        }
    }

    /// Spawn a new session's first pane. Returns None (logging the error) if the
    /// PTY/shell could not be started, rather than panicking the daemon.
    fn spawn_session(
        &mut self,
        name: String,
        shell: Option<Vec<String>>,
        size: PtySize,
    ) -> Option<SessionId> {
        match self.daemon.new_session(name, shell, size) {
            Ok((sid, pid, reader)) => {
                self.pane_session.insert(pid, sid);
                spawn_pane_reader(pid, reader, self.tx.clone());
                Some(sid)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to spawn session shell");
                None
            }
        }
    }

    fn on_input(&mut self, client_id: u64, msg: ClientMsg) {
        let Some(session) = self.clients.get(&client_id).map(|h| h.session) else {
            return;
        };
        match msg {
            ClientMsg::Input(bytes) => {
                // Any input dismisses a pending display-message (tmux behavior).
                self.daemon.clear_message(client_id);
                // Mouse reporting sequences are intercepted here (when enabled)
                // and never reach the keymap or the shell; everything else is
                // forwarded to the keymap as before.
                let keyboard = if self.daemon.mouse_enabled() {
                    self.extract_and_handle_mouse(client_id, session, &bytes)
                } else {
                    bytes.clone()
                };
                let reactions = self
                    .daemon
                    .keymap_mut(client_id)
                    .map(|k| k.feed(&keyboard))
                    .unwrap_or_default();
                let mut session = session;
                for r in reactions {
                    match r {
                        Reaction::PassThrough(data) => {
                            // Broadcast to all panes when synchronize-panes is on
                            // for the active window; otherwise just the active pane.
                            self.daemon.write_input(session, &data);
                        }
                        Reaction::Do(Action::EnterCopyMode) => {
                            self.daemon.enter_copy_mode(client_id, session);
                        }
                        Reaction::Do(Action::ChooseSession) => {
                            self.daemon.open_chooser(client_id);
                        }
                        Reaction::Do(action) => self.apply_action(client_id, session, action),
                        Reaction::Copy(ck) => {
                            self.handle_copy_key(client_id, session, ck);
                        }
                        Reaction::Session(sk) => {
                            if let Some(new_session) = self.handle_session_key(client_id, sk) {
                                session = new_session;
                            }
                        }
                        Reaction::Prompt(pk) => {
                            self.handle_prompt_key(client_id, session, pk);
                        }
                        Reaction::Help(hk) => {
                            self.daemon.help_scroll(client_id, hk);
                        }
                        Reaction::Search(sk) => {
                            self.handle_search_key(client_id, session, sk);
                        }
                        Reaction::Buffer(bk) => {
                            self.handle_buffer_key(client_id, session, bk);
                        }
                        Reaction::PaneNumber(pick) => match pick {
                            Some(n) => self.daemon.pick_pane_number(client_id, session, n),
                            None => self.daemon.hide_pane_numbers(client_id),
                        },
                    }
                }
                self.render_session(session);
            }
            ClientMsg::Resize(size) => {
                // Update this client's stored size first so effective_size (the
                // min over clients) reflects the new dimensions — otherwise the
                // composed screen, and the right-aligned status segment, stay at
                // the attach-time width and overflow/wrap on the real terminal.
                self.daemon.server.set_client_size(client_id, size.into());
                self.daemon.resize_session(session, size.into());
                self.render_session(session);
            }
            ClientMsg::Detach => {
                if let Some(h) = self.clients.get(&client_id) {
                    let _ = h.out.send(ServerMsg::Detached);
                }
            }
            ClientMsg::Command(cmd) => self.on_command(client_id, session, cmd),
            _ => {}
        }
    }

    fn on_command(&mut self, client_id: u64, session: SessionId, cmd: Command) {
        match cmd {
            Command::SplitWindow { horizontal } => {
                let dir = if horizontal {
                    SplitDir::Horizontal
                } else {
                    SplitDir::Vertical
                };
                self.do_split(session, dir);
                self.render_session(session);
            }
            Command::NewWindow { .. } => {
                self.do_new_window(session);
                self.render_session(session);
            }
            Command::KillWindow => {
                self.do_kill_window(session);
            }
            Command::NextWindow => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    s.focus_next_window();
                }
                self.render_session(session);
            }
            Command::PrevWindow => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    s.focus_prev_window();
                }
                self.render_session(session);
            }
            Command::ListSessions => {
                let text = self.list_sessions();
                if let Some(h) = self.clients.get(&client_id) {
                    let _ = h.out.send(ServerMsg::Reply(text));
                }
            }
            Command::SelectWindow { index } => {
                self.select_window_by_number(session, index);
                self.render_session(session);
            }
            Command::SendKeys { keys } => {
                // Inject keys as if typed into the active pane (bypassing the
                // prefix keymap — scripting goes straight to the shell).
                if let Some(pid) = self.active_pane(session) {
                    let _ = self.daemon.write_pane(pid, &keys);
                }
            }
            Command::RenameWindow { name } => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    let wid = s.active_window();
                    if let Some(w) = s.window_mut(wid) {
                        w.set_name_manual(name);
                    }
                }
                self.render_session(session);
            }
            Command::RenameSession { name } => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    s.name = name;
                }
                self.render_session(session);
            }
            Command::SourceFile { path } => {
                let reply = match std::fs::read_to_string(&path) {
                    Ok(text) => match crate::parse_config(std::path::Path::new(&path), &text) {
                        Ok(cfg) => {
                            self.daemon.set_config(cfg);
                            self.render_session(session);
                            format!("sourced {path}\n")
                        }
                        Err(e) => format!("config error: {e}\n"),
                    },
                    Err(e) => format!("cannot read {path}: {e}\n"),
                };
                if let Some(h) = self.clients.get(&client_id) {
                    let _ = h.out.send(ServerMsg::Reply(reply));
                }
            }
            Command::KillSession { target } => {
                // Match by name; fall back to the current session.
                let sid = self
                    .daemon
                    .server
                    .find_session_by_name(&target)
                    .unwrap_or(session);
                self.kill_whole_session(sid);
            }
            Command::KillServer => {
                let ids = self.daemon.server.session_ids();
                for id in ids {
                    self.kill_whole_session(id);
                }
            }
        }
    }

    /// Kill a session and notify/disconnect its clients.
    fn kill_whole_session(&mut self, session: SessionId) {
        // Drop the pane->session map entries for this session's panes.
        let panes: Vec<PaneId> = self
            .pane_session
            .iter()
            .filter(|(_, s)| **s == session)
            .map(|(p, _)| *p)
            .collect();
        for p in panes {
            self.pane_session.remove(&p);
        }
        let gone = self.session_clients(session);
        self.daemon.server.kill_session(session);
        for id in gone {
            if let Some(h) = self.clients.remove(&id) {
                let _ = h.out.send(ServerMsg::Event(Event::SessionClosed));
                let _ = h.out.send(ServerMsg::Detached);
            }
            self.daemon.unregister_client(id);
        }
    }

    fn apply_action(&mut self, client_id: u64, session: SessionId, action: Action) {
        match action {
            Action::SplitHorizontal => self.do_split(session, SplitDir::Horizontal),
            Action::SplitVertical => self.do_split(session, SplitDir::Vertical),
            Action::NewWindow => self.do_new_window(session),
            Action::NextWindow => self.do_next_window(session),
            Action::PrevWindow => self.do_prev_window(session),
            Action::SelectWindow(n) => self.select_window_by_number(session, n),
            Action::LastWindow => self.do_last_window(session),
            Action::KillWindow => self.do_kill_window(session),
            Action::SelectPaneLeft => self.select_pane(session, Direction::Left),
            Action::SelectPaneRight => self.select_pane(session, Direction::Right),
            Action::SelectPaneUp => self.select_pane(session, Direction::Up),
            Action::SelectPaneDown => self.select_pane(session, Direction::Down),
            Action::LastPane => self.do_last_pane(session),
            Action::ResizePaneLeft => self.resize_pane(session, SplitDir::Horizontal, -RESIZE_STEP),
            Action::ResizePaneRight => self.resize_pane(session, SplitDir::Horizontal, RESIZE_STEP),
            Action::ResizePaneUp => self.resize_pane(session, SplitDir::Vertical, -RESIZE_STEP),
            Action::ResizePaneDown => self.resize_pane(session, SplitDir::Vertical, RESIZE_STEP),
            Action::ZoomPane => self.zoom_pane(session),
            Action::BreakPane => self.do_break_pane(session),
            Action::SwapPanePrev => self.do_swap_pane(session, false),
            Action::SwapPaneNext => self.do_swap_pane(session, true),
            Action::NextLayout => self.next_layout(session),
            Action::RenameWindow => {
                self.daemon
                    .open_prompt(client_id, session, crate::daemon::PromptTarget::Window);
            }
            Action::RenameSession => {
                self.daemon
                    .open_prompt(client_id, session, crate::daemon::PromptTarget::Session);
            }
            Action::FindWindow => {
                self.daemon
                    .open_prompt(client_id, session, crate::daemon::PromptTarget::FindWindow);
            }
            Action::CommandPrompt => {
                self.daemon
                    .open_prompt(client_id, session, crate::daemon::PromptTarget::Command);
            }
            Action::KillPane => self.do_kill_pane(session),
            Action::EnterCopyMode => {
                self.daemon.enter_copy_mode(client_id, session);
            }
            Action::ReloadConfig => self.reload_config(client_id, session),
            Action::ShowHelp => self.daemon.toggle_help(client_id),
            Action::ChooseSession => self.daemon.open_chooser(client_id),
            Action::DisplayPanes => self.daemon.show_pane_numbers(client_id),
            Action::SwapWindowLeft => self.do_move_window(session, -1),
            Action::SwapWindowRight => self.do_move_window(session, 1),
            Action::ToggleSync => {
                self.daemon.toggle_sync(client_id, session);
            }
            Action::PasteBuffer => {
                if !self.daemon.paste_buffer(session) {
                    self.daemon.flash_message(client_id, "no buffers");
                }
            }
            Action::ChooseBuffer => {
                if !self.daemon.open_buffer_chooser(client_id) {
                    self.daemon.flash_message(client_id, "no buffers");
                }
            }
            Action::Detach => {
                // Tell the client to detach; the session keeps running. The
                // client's reader loop breaks on Detached and restores the
                // terminal. ClientGone arrives when its socket closes.
                if let Some(h) = self.clients.get(&client_id) {
                    let _ = h.out.send(ServerMsg::Detached);
                }
            }
            // SendPrefix is handled as pass-through in the keymap, never here.
            Action::SendPrefix => {}
        }
    }

    /// Decode and act on SGR mouse sequences in `bytes`, returning the bytes
    /// that are NOT mouse events (to be handled as keyboard input). Click selects
    /// a pane, wheel scrolls into copy-mode, drag resizes the divider.
    fn extract_and_handle_mouse(
        &mut self,
        client_id: u64,
        session: SessionId,
        bytes: &[u8],
    ) -> Vec<u8> {
        use lumux_core::mouse::{self, MouseButton, MouseKind};
        // Prepend any mouse-report prefix held back from the previous frame (an
        // SGR report split across reads, e.g. over SSH). Taken out of the handle
        // so we don't borrow self.clients across the &mut self calls below.
        let mut input: Vec<u8> = Vec::new();
        if let Some(h) = self.clients.get_mut(&client_id) {
            if !h.pending_mouse.is_empty() {
                input.append(&mut h.pending_mouse);
            }
        }
        let combined: &[u8] = if input.is_empty() {
            bytes
        } else {
            input.extend_from_slice(bytes);
            &input
        };

        let mut rest = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < combined.len() {
            if let Some((ev, used)) = mouse::parse(&combined[i..]) {
                // A mouse *press* selects the pane under the pointer first, even
                // when that pane is a mouse-aware app — otherwise clicking into a
                // pane running Claude Code / vim / htop forwards the click but
                // never switches lumux's focus to it (tmux selects on press too).
                // It also hit-tests the status bar (window-list clicks) and arms a
                // possible divider drag. Scroll/drag/motion do NOT change focus, so
                // hover-to-scroll over an unfocused pane keeps working.
                if matches!(ev.kind, MouseKind::Down(_)) {
                    self.mouse_select_pane(session, ev.col, ev.row);
                    self.daemon.begin_drag(client_id, session, ev.col, ev.row);
                    // A left-press that didn't grab a divider arms a text
                    // selection; the first drag motion turns it into a copy-mode
                    // selection (tmux drag-to-copy). A press on a divider resizes
                    // instead, so don't arm there.
                    if matches!(ev.kind, MouseKind::Down(MouseButton::Left))
                        && !self.daemon.is_dragging_divider(client_id)
                    {
                        self.daemon.mouse_sel_arm(client_id, session, ev.col, ev.row);
                    }
                }
                // If the app in the pane under the pointer enabled mouse
                // reporting, forward the raw event to it (pane-relative) and skip
                // lumux's own handling — so the wheel/clicks work inside vim,
                // htop, Claude Code, etc. (matches tmux).
                if self.try_forward_mouse_to_app(session, &ev) {
                    i += used;
                    continue;
                }
                match ev.kind {
                    // The press already selected the pane + armed a divider drag
                    // above; nothing more to do for a non-mouse-aware pane.
                    MouseKind::Down(_) => {}
                    MouseKind::ScrollUp => self.mouse_scroll(client_id, session, ev.col, ev.row, true),
                    MouseKind::ScrollDown => self.mouse_scroll(client_id, session, ev.col, ev.row, false),
                    MouseKind::Drag(_) => {
                        // A live/armed text selection takes the drag (extending
                        // the copy-mode selection under the pointer). Otherwise
                        // fall back to moving a grabbed divider.
                        if self.daemon.mouse_sel_drag(client_id, session, ev.col, ev.row) {
                            self.render_client(client_id);
                        } else {
                            self.mouse_drag(client_id, session, ev.col, ev.row);
                        }
                    }
                    MouseKind::Up(_) => {
                        // Releasing a text-selection drag yanks it (copy + exit
                        // copy-mode) and emits OSC-52 so the client's local
                        // terminal clipboard is set too — same as keyboard Yank.
                        if self.daemon.mouse_sel_active(client_id) {
                            let text = self.daemon.mouse_sel_finish(client_id, session);
                            if let Some(k) = self.daemon.keymap_mut(client_id) {
                                k.reset();
                            }
                            if let (Some(text), Some(h)) = (text, self.clients.get(&client_id)) {
                                let _ = h.out.send(ServerMsg::Frame(osc52(&text)));
                            }
                            self.render_client(client_id);
                        } else {
                            self.daemon.end_drag(client_id);
                        }
                    }
                    // Bare pointer motion: consumed (so it can't leak as text) but
                    // otherwise ignored — it must not dismiss overlays like help.
                    MouseKind::Move => {}
                }
                i += used;
            } else if mouse::is_partial(&combined[i..]) {
                // An SGR report truncated at this frame's end: hold it for the
                // next frame instead of leaking the bytes to the app as text.
                pending.extend_from_slice(&combined[i..]);
                break;
            } else if mouse::is_introducer_prefix(&combined[i..])
                && self.daemon.mouse_sel_pending(client_id)
            {
                // The frame ended inside the `ESC [ <` introducer of the next
                // report (typically the release, split across an SSH/mosh read).
                // is_partial can't hold a bare ESC/ESC[ — it's also the start of
                // a real Escape/arrow — but a mouse selection is live here, so
                // the next report is expected: hold it so the release
                // reassembles instead of being dropped (which would lose the copy).
                pending.extend_from_slice(&combined[i..]);
                break;
            } else {
                rest.push(combined[i]);
                i += 1;
            }
        }
        if let Some(h) = self.clients.get_mut(&client_id) {
            h.pending_mouse = pending;
        }
        rest
    }

    /// Click: focus the pane under the cursor, or switch windows when the click
    /// lands on a window entry in the status bar's bottom row.
    fn mouse_select_pane(&mut self, session: SessionId, col: u16, row: u16) {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        // The status bar is the last row. A click there hit-tests the window list
        // (tmux: click a window name to switch to it).
        if row == size.rows.saturating_sub(1) {
            if let Some(wid) = self
                .daemon
                .status_window_at(session, col, size.cols as usize)
            {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    s.focus_window(wid);
                }
            }
            return;
        }
        let viewport = lumux_core::layout::Rect::new(0, 0, size.cols, size.rows.saturating_sub(1));
        if let Some(s) = self.daemon.server.session_mut(session) {
            let wid = s.active_window();
            if let Some(w) = s.window_mut(wid) {
                let rects = lumux_core::layout::compute(&w.layout, viewport);
                if let Some(pid) = lumux_core::layout::pane_at(&rects, col, row) {
                    w.focus_pane(pid);
                }
            }
        }
    }

    /// Focus the pane whose rectangle contains (col,row), if any. Returns true if
    /// a pane was focused. Used by scroll to target the pane under the pointer.
    fn focus_pane_at(&mut self, session: SessionId, col: u16, row: u16) -> bool {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        // The status bar row holds no pane.
        if row >= size.rows.saturating_sub(1) {
            return false;
        }
        let viewport = lumux_core::layout::Rect::new(0, 0, size.cols, size.rows.saturating_sub(1));
        if let Some(s) = self.daemon.server.session_mut(session) {
            let wid = s.active_window();
            if let Some(w) = s.window_mut(wid) {
                let rects = lumux_core::layout::compute(&w.layout, viewport);
                if let Some(pid) = lumux_core::layout::pane_at(&rects, col, row) {
                    w.focus_pane(pid);
                    return true;
                }
            }
        }
        false
    }

    /// Wheel: scroll the pane under the pointer. Two cases, matching tmux:
    /// - A pane on the *alternate screen* (vim/less, or a TUI agent like Claude
    ///   Code) owns the viewport and has no scrollback, so the wheel is
    ///   translated into arrow-key input sent to that app, which scrolls itself.
    /// - Otherwise, enter copy-mode on that pane and scroll its history.
    ///
    /// While already in copy-mode the current pane keeps scrolling, so an
    /// in-progress selection isn't hijacked.
    fn mouse_scroll(&mut self, client_id: u64, session: SessionId, col: u16, row: u16, up: bool) {
        use lumux_core::keymap::CopyKey;

        // Not yet in copy-mode: decide between alt-screen passthrough and copy.
        if !self.daemon.in_copy_mode(client_id) {
            if let Some(pid) = self.pane_at_point(session, col, row) {
                if self.daemon.pane_on_alt_screen(pid) {
                    // Send arrow keys to the app (3 per notch, like tmux). CSI
                    // arrows (ESC[A / ESC[B) work for the vast majority of TUIs.
                    let arrow: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
                    for _ in 0..3 {
                        let _ = self.daemon.write_pane(pid, arrow);
                    }
                    return;
                }
            }
            if !up {
                return; // scrolling down in live (non-alt) view does nothing
            }
            // Focus the pane under the pointer so copy-mode opens on it.
            self.focus_pane_at(session, col, row);
            self.daemon.enter_copy_mode(client_id, session);
        }
        let key = if up { CopyKey::Up } else { CopyKey::Down };
        // Scroll a few lines per wheel notch.
        for _ in 0..3 {
            self.daemon.copy_navigate(client_id, session, key);
        }
    }

    /// The pane id whose rectangle contains (col,row), without changing focus.
    fn pane_at_point(&self, session: SessionId, col: u16, row: u16) -> Option<PaneId> {
        let size = self.daemon.server.effective_size(session)?;
        if row >= size.rows.saturating_sub(1) {
            return None; // status bar row
        }
        let viewport = lumux_core::layout::Rect::new(0, 0, size.cols, size.rows.saturating_sub(1));
        let s = self.daemon.server.session(session)?;
        let w = s.window(s.active_window())?;
        let rects = lumux_core::layout::compute(&w.layout, viewport);
        lumux_core::layout::pane_at(&rects, col, row)
    }

    /// The pane and its rectangle containing (col,row) — used to translate a
    /// mouse event into pane-relative coordinates before forwarding to the app.
    fn pane_and_rect_at_point(
        &self,
        session: SessionId,
        col: u16,
        row: u16,
    ) -> Option<(PaneId, lumux_core::layout::Rect)> {
        let size = self.daemon.server.effective_size(session)?;
        if row >= size.rows.saturating_sub(1) {
            return None;
        }
        let viewport = lumux_core::layout::Rect::new(0, 0, size.cols, size.rows.saturating_sub(1));
        let s = self.daemon.server.session(session)?;
        let w = s.window(s.active_window())?;
        let rects = lumux_core::layout::compute(&w.layout, viewport);
        let pid = lumux_core::layout::pane_at(&rects, col, row)?;
        let rect = *rects.get(&pid)?;
        Some((pid, rect))
    }

    /// If the pane under the pointer has mouse reporting on, forward the raw event
    /// to that app re-encoded with pane-relative coordinates and return true (so
    /// the caller skips lumux's own scroll/copy/select handling). tmux behavior:
    /// a mouse-aware TUI (vim, htop, Claude Code) handles the wheel/clicks itself.
    fn try_forward_mouse_to_app(
        &mut self,
        session: SessionId,
        ev: &lumux_core::mouse::MouseEvent,
    ) -> bool {
        let Some((pid, rect)) = self.pane_and_rect_at_point(session, ev.col, ev.row) else {
            return false;
        };
        if !self.daemon.pane_wants_mouse(pid) {
            return false;
        }
        // Translate screen coords to pane-relative (clamped into the rect).
        let rel_col = ev.col.saturating_sub(rect.x).min(rect.cols.saturating_sub(1));
        let rel_row = ev.row.saturating_sub(rect.y).min(rect.rows.saturating_sub(1));
        let bytes = lumux_core::mouse::encode_sgr(ev.raw_button, rel_col, rel_row, ev.press);
        let _ = self.daemon.write_pane(pid, &bytes);
        true
    }

    /// Drag: move the divider grabbed on press (if any) to follow the cursor and
    /// re-fit. A drag that didn't start on a divider is a no-op, so dragging in
    /// open pane area never resizes.
    fn mouse_drag(&mut self, client_id: u64, session: SessionId, col: u16, row: u16) {
        self.daemon.drag_divider(client_id, session, col, row);
    }

    /// Focus a window by the number the user sees (status-bar number, which
    /// includes the config's base-index). Mapping back through base-index is what
    /// fixes the off-by-one when base_index = 1: pressing "1" selects the first
    /// window, not the second. Numbers below base-index clamp to the first window.
    fn select_window_by_number(&mut self, session: SessionId, number: u32) {
        let base = self.daemon.base_index();
        let pos = number.saturating_sub(base) as usize;
        if let Some(s) = self.daemon.server.session_mut(session) {
            let ids = s.window_ids();
            if let Some(&wid) = ids.get(pos) {
                s.focus_window(wid);
            }
        }
    }

    /// Move focus geographically within the active window.
    fn select_pane(&mut self, session: SessionId, dir: Direction) {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        // Content area excludes the status bar row.
        let viewport = lumux_core::layout::Rect::new(0, 0, size.cols, size.rows.saturating_sub(1));
        if let Some(s) = self.daemon.server.session_mut(session) {
            let wid = s.active_window();
            if let Some(w) = s.window_mut(wid) {
                w.focus_direction(dir, viewport);
            }
        }
    }

    /// Adjust the divider nearest the active pane (tmux resize-pane) and re-fit
    /// the PTYs to the new rectangles. `axis`/`step` are passed straight to the
    /// window: positive step moves the divider right/down.
    fn resize_pane(&mut self, session: SessionId, axis: SplitDir, step: f32) {
        let changed = self
            .daemon
            .server
            .session_mut(session)
            .map(|s| {
                let wid = s.active_window();
                s.window_mut(wid)
                    .map(|w| w.resize_active(axis, step))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if changed {
            // Keep PTYs/grids in step with the new layout rectangles.
            if let Some(size) = self.daemon.server.effective_size(session) {
                self.daemon.resize_session(session, size);
            }
        }
    }

    /// Toggle zoom on the active pane (tmux prefix z) and re-fit PTYs, since the
    /// zoomed pane now fills the whole content area (or returns to its split).
    fn zoom_pane(&mut self, session: SessionId) {
        let toggled = self
            .daemon
            .server
            .session_mut(session)
            .map(|s| {
                let wid = s.active_window();
                s.window_mut(wid).map(|w| w.toggle_zoom()).is_some()
            })
            .unwrap_or(false);
        if toggled {
            if let Some(size) = self.daemon.server.effective_size(session) {
                self.daemon.resize_session(session, size);
            }
        }
    }

    /// Cycle the active window to the next preset layout (tmux next-layout,
    /// prefix Space) and re-fit the PTYs to the new pane rectangles.
    fn next_layout(&mut self, session: SessionId) {
        let applied = self
            .daemon
            .server
            .session_mut(session)
            .map(|s| {
                let wid = s.active_window();
                if let Some(w) = s.window_mut(wid) {
                    w.next_layout();
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if applied {
            if let Some(size) = self.daemon.server.effective_size(session) {
                self.daemon.resize_session(session, size);
            }
        }
    }

    /// Re-source the daemon's config file (tmux prefix r) and flash a message.
    /// Reloads from the first existing config candidate, format chosen by
    /// extension (so a tmux-syntax lumux.conf reloads as tmux).
    fn reload_config(&mut self, client_id: u64, session: SessionId) {
        let found = crate::config_candidates()
            .into_iter()
            .find(|p| p.exists());
        let msg = match found {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) => match crate::parse_config(&path, &text) {
                    Ok(cfg) => {
                        self.daemon.set_config(cfg);
                        "lumux configuration reloaded".to_string()
                    }
                    Err(e) => format!("config error: {e}"),
                },
                Err(_) => format!("no config at {}", path.display()),
            },
            None => "no config file found".to_string(),
        };
        self.daemon.flash_message(client_id, msg);
        self.render_session(session);
    }

    /// Drive the session switcher. Returns Some(new_session) when the user
    /// confirms a switch, so the caller can re-point its local session.
    fn handle_session_key(&mut self, client_id: u64, sk: SessionKey) -> Option<SessionId> {
        match sk {
            SessionKey::Up => {
                self.daemon.chooser_move(client_id, -1, None);
                None
            }
            SessionKey::Down => {
                self.daemon.chooser_move(client_id, 1, None);
                None
            }
            SessionKey::Index(n) => {
                self.daemon.chooser_move(client_id, 0, Some(n as usize));
                None
            }
            SessionKey::Expand => {
                self.daemon.chooser_expand(client_id);
                None
            }
            SessionKey::Collapse => {
                self.daemon.chooser_collapse(client_id);
                None
            }
            SessionKey::Cancel => {
                self.daemon.chooser_cancel(client_id);
                None
            }
            SessionKey::Confirm => {
                // A pick may be a session or a specific window (choose-tree). The
                // daemon already switched the client's session (and focused the
                // window for a window pick); we just sync the loop's mapping.
                let pick = self.daemon.chooser_confirm(client_id)?;
                let sid = match pick {
                    crate::daemon::ChooserPick::Session(s)
                    | crate::daemon::ChooserPick::Window(s, _) => s,
                };
                if let Some(h) = self.clients.get_mut(&client_id) {
                    h.session = sid;
                }
                self.daemon.invalidate_client(client_id);
                Some(sid)
            }
        }
    }

    /// Drive the open paste-buffer chooser (tmux prefix `=`): move the selection,
    /// paste the highlighted buffer, delete it, or cancel.
    fn handle_buffer_key(&mut self, client_id: u64, session: SessionId, bk: BufferKey) {
        match bk {
            BufferKey::Up => self.daemon.buffer_chooser_move(client_id, -1, None),
            BufferKey::Down => self.daemon.buffer_chooser_move(client_id, 1, None),
            BufferKey::Index(n) => self.daemon.buffer_chooser_move(client_id, 0, Some(n as usize)),
            BufferKey::Delete => self.daemon.buffer_chooser_delete(client_id),
            BufferKey::Cancel => self.daemon.buffer_chooser_cancel(client_id),
            BufferKey::Confirm => self.daemon.buffer_chooser_confirm(client_id, session),
        }
    }

    /// Drive an open rename prompt: edit the buffer, or commit/cancel it.
    fn handle_prompt_key(&mut self, client_id: u64, session: SessionId, pk: PromptKey) {
        let mut command_line = None;
        match pk {
            PromptKey::Char(c) => self.daemon.prompt_push(client_id, c),
            PromptKey::Backspace => self.daemon.prompt_backspace(client_id),
            PromptKey::Cancel => self.daemon.prompt_cancel(client_id),
            // Confirm may return a command-prompt line for us to dispatch.
            PromptKey::Confirm => command_line = self.daemon.prompt_confirm(client_id, session),
        }
        // Reset the keymap out of Prompt mode once the prompt closes.
        if matches!(pk, PromptKey::Confirm | PromptKey::Cancel) {
            if let Some(k) = self.daemon.keymap_mut(client_id) {
                k.reset();
            }
        }
        if let Some(line) = command_line {
            self.dispatch_command_line(client_id, session, &line);
        }
    }

    /// Parse and run a tmux command-prompt line (prefix `:`). Reuses the same
    /// action paths as the keybindings so behavior is identical.
    fn dispatch_command_line(&mut self, client_id: u64, session: SessionId, line: &str) {
        use lumux_core::command::parse_commands;
        // A command line may chain several commands with `;` (tmux separator).
        // Execute each in order; render once at the end.
        for cmd in parse_commands(line) {
            self.dispatch_parsed(client_id, session, cmd);
        }
        self.render_session(session);
    }

    /// Execute a single parsed command-prompt command. Split out of
    /// [`Self::dispatch_command_line`] so a `;`-chained line runs each segment
    /// through the same logic. Rendering is done once by the caller.
    fn dispatch_parsed(&mut self, client_id: u64, session: SessionId, cmd: lumux_core::command::ParsedCommand) {
        use lumux_core::command::{Dir, ParsedCommand};
        let dir_to_split = |d: Dir| match d {
            Dir::Horizontal => SplitDir::Horizontal,
            Dir::Vertical => SplitDir::Vertical,
        };
        match cmd {
            ParsedCommand::SplitWindow(d) => self.do_split(session, dir_to_split(d)),
            ParsedCommand::NewWindow => self.do_new_window(session),
            ParsedCommand::KillPane => self.do_kill_pane(session),
            ParsedCommand::KillWindow => self.do_kill_window(session),
            ParsedCommand::NextWindow => self.do_next_window(session),
            ParsedCommand::PrevWindow => self.do_prev_window(session),
            ParsedCommand::LastWindow => self.do_last_window(session),
            ParsedCommand::LastPane => self.do_last_pane(session),
            ParsedCommand::SelectWindow(n) => self.select_window_by_number(session, n),
            ParsedCommand::RenameWindow(name) => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    let wid = s.active_window();
                    if let Some(w) = s.window_mut(wid) {
                        w.set_name_manual(name);
                    }
                }
            }
            ParsedCommand::RenameSession(name) => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    s.name = name;
                }
            }
            ParsedCommand::FindWindow(q) => {
                let target = self.daemon.server.session(session).and_then(|s| {
                    let needle = q.to_lowercase();
                    s.window_ids().into_iter().find(|&wid| {
                        s.window(wid)
                            .map(|w| w.name.to_lowercase().contains(&needle))
                            .unwrap_or(false)
                    })
                });
                match target {
                    Some(wid) => {
                        if let Some(s) = self.daemon.server.session_mut(session) {
                            s.focus_window(wid);
                        }
                    }
                    None => self.daemon.flash_message(client_id, format!("no window matching \"{q}\"")),
                }
            }
            ParsedCommand::BreakPane => self.do_break_pane(session),
            ParsedCommand::SwapPane { next } => self.do_swap_pane(session, next),
            ParsedCommand::JoinPane { dir, src } => self.do_join_pane(client_id, session, dir_to_split(dir), src),
            ParsedCommand::SynchronizePanes(state) => {
                let on_now = self.daemon.is_synchronized(session);
                let want = state.unwrap_or(!on_now);
                if want != on_now {
                    self.daemon.toggle_sync(client_id, session);
                }
            }
            ParsedCommand::DisplayPanes => self.daemon.show_pane_numbers(client_id),
            ParsedCommand::CapturePane => match self.daemon.capture_pane(session) {
                Some(name) => self.daemon.flash_message(client_id, format!("captured to {name}")),
                None => self.daemon.flash_message(client_id, "nothing to capture"),
            },
            ParsedCommand::RespawnPane => self.do_respawn_pane(client_id, session),
            ParsedCommand::RunShell(cmd) => {
                let status = self.daemon.run_shell(&cmd);
                self.daemon.flash_message(client_id, status);
            }
            ParsedCommand::SaveState => {
                let path = self.state_path.clone();
                match self.daemon.save_state(&path) {
                    Ok(()) => self.daemon.flash_message(client_id, "state saved"),
                    Err(e) => self.daemon.flash_message(client_id, format!("save failed: {e}")),
                }
            }
            ParsedCommand::Detach => {
                if let Some(h) = self.clients.get(&client_id) {
                    let _ = h.out.send(ServerMsg::Detached);
                }
            }
            ParsedCommand::BadArgs(usage) => self.daemon.flash_message(client_id, usage),
            ParsedCommand::Unknown(verb) => {
                self.daemon.flash_message(client_id, format!("unknown command: {verb}"));
            }
        }
    }
    /// q/Escape quits.
    fn handle_copy_key(&mut self, client_id: u64, session: SessionId, ck: CopyKey) {
        match ck {
            CopyKey::Quit => {
                self.daemon.copy_navigate(client_id, session, ck);
                if let Some(k) = self.daemon.keymap_mut(client_id) {
                    k.reset();
                }
            }
            CopyKey::StartSelection => {
                self.daemon.copy_start_selection(client_id);
                self.render_client(client_id);
            }
            CopyKey::RectangleToggle => {
                self.daemon.copy_toggle_rectangle(client_id);
                self.render_client(client_id);
            }
            CopyKey::Yank => {
                let text = self.daemon.copy_yank(client_id, session);
                // Yank exits copy-mode; reset the keymap and emit OSC-52 so the
                // client's local terminal also gets the text.
                if let Some(k) = self.daemon.keymap_mut(client_id) {
                    k.reset();
                }
                if let (Some(text), Some(h)) = (text, self.clients.get(&client_id)) {
                    let seq = osc52(&text);
                    let _ = h.out.send(ServerMsg::Frame(seq));
                }
                self.render_client(client_id);
            }
            // Open the search query input (forward `/` or backward `?`). The
            // keymap has already switched to Search mode; seed the buffer.
            CopyKey::SearchForward => {
                self.daemon
                    .search_open(client_id, lumux_core::copymode::SearchDir::Forward);
                self.render_client(client_id);
            }
            CopyKey::SearchBackward => {
                self.daemon
                    .search_open(client_id, lumux_core::copymode::SearchDir::Backward);
                self.render_client(client_id);
            }
            // Repeat the last search (n keeps direction, N reverses).
            CopyKey::RepeatSearch | CopyKey::RepeatSearchRev => {
                let same = matches!(ck, CopyKey::RepeatSearch);
                if !self.daemon.search_repeat(client_id, session, same) {
                    self.daemon.flash_message(client_id, "no more matches");
                }
                self.render_client(client_id);
            }
            _ => {
                self.daemon.copy_navigate(client_id, session, ck);
            }
        }
    }

    /// Drive the copy-mode search text input (after `/`/`?`). Confirm runs the
    /// search and jumps the cursor; cancel returns to navigation; chars/backspace
    /// edit the live query (shown incrementally in the status line).
    fn handle_search_key(&mut self, client_id: u64, session: SessionId, sk: SearchKey) {
        match sk {
            SearchKey::Char(c) => self.daemon.search_push(client_id, c),
            SearchKey::Backspace => self.daemon.search_backspace(client_id),
            SearchKey::Cancel => self.daemon.search_cancel(client_id),
            SearchKey::Confirm => {
                if !self.daemon.search_confirm(client_id, session) {
                    self.daemon.flash_message(client_id, "pattern not found");
                }
            }
        }
        self.render_client(client_id);
    }

    /// Break the active pane out into its own new window (tmux break-pane, `!`).
    /// The pane keeps its PTY/grid (both keyed by pane id in the daemon) and its
    /// session, so no spawn/teardown is needed — only the layout changes. The new
    /// window becomes active; we re-fit so the moved pane fills it.
    fn do_break_pane(&mut self, session: SessionId) {
        if self.daemon.server.break_active_pane(session).is_none() {
            return; // single-pane window: nothing to break out (tmux no-op).
        }
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        // The moved pane now fills a fresh full-size window; re-fit BOTH the new
        // window and the source window (whose remaining panes grew), then repaint.
        self.daemon.resize_all_windows(session, size);
        self.invalidate_session(session);
    }

    /// Swap the active pane with its previous (`{`) or next (`}`) sibling in the
    /// same window (tmux swap-pane). Pure layout change — panes keep their grids.
    fn do_swap_pane(&mut self, session: SessionId, next: bool) {
        let Some(other) = self.daemon.server.sibling_pane(session, next) else {
            return; // single pane: nothing to swap with.
        };
        if self.daemon.server.swap_active_pane(session, other) {
            let size = self
                .daemon
                .server
                .effective_size(session)
                .unwrap_or(PtySize::new(80, 24));
            self.daemon.resize_session(session, size);
            self.invalidate_session(session);
        }
    }

    /// Join a pane from a source window into the active window (tmux join-pane).
    /// `src` is a window index (base-index offset); None means the previously-
    /// active window. The moved pane keeps its PTY/grid; if the source window
    /// empties it is closed. Re-fits all windows since two changed.
    fn do_join_pane(&mut self, client_id: u64, session: SessionId, dir: SplitDir, src: Option<u32>) {
        // Resolve the source window id.
        let src_wid = {
            let Some(s) = self.daemon.server.session(session) else {
                return;
            };
            match src {
                Some(n) => {
                    let base = self.daemon.base_index();
                    let idx = n.saturating_sub(base) as usize;
                    s.window_ids().get(idx).copied()
                }
                // No -s given: tmux uses the last (previously-active) window.
                None => s
                    .window_ids()
                    .into_iter()
                    .find(|&w| w != s.active_window()),
            }
        };
        let Some(src_wid) = src_wid else {
            self.daemon.flash_message(client_id, "join-pane: no source window");
            return;
        };
        match self.daemon.server.join_pane(session, src_wid, dir) {
            Some(_) => {
                let size = self
                    .daemon
                    .server
                    .effective_size(session)
                    .unwrap_or(PtySize::new(80, 24));
                self.daemon.resize_all_windows(session, size);
                self.invalidate_session(session);
            }
            None => { /* self-join or unknown window: ignore */ }
        }
    }

    /// Respawn the active pane if it's dead (tmux respawn-pane, remain-on-exit).
    /// Spawns a fresh PTY reusing the pane id, restarts the reader thread, and
    /// re-fits the pane. No-op (with a flash) if the pane is still alive.
    fn do_respawn_pane(&mut self, client_id: u64, session: SessionId) {
        let Some(pane) = self.active_pane(session) else {
            return;
        };
        if !self.daemon.is_pane_dead(pane) {
            self.daemon.flash_message(client_id, "pane is not dead");
            return;
        }
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        match self.daemon.respawn_pane(session, pane, size) {
            Ok(Some(reader)) => {
                self.pane_session.insert(pane, session);
                spawn_pane_reader(pane, reader, self.tx.clone());
                self.daemon.resize_session(session, size);
                self.invalidate_session(session);
            }
            _ => self.daemon.flash_message(client_id, "respawn failed"),
        }
    }

    /// Force a full repaint for every client of `session`. Used after structural
    /// layout changes (break/swap pane) where an incremental diff against the old
    /// screen would be wrong.
    fn invalidate_session(&mut self, session: SessionId) {
        for id in self.session_clients(session) {
            self.daemon.invalidate_client(id);
        }
    }

    /// Move the active window earlier (`delta < 0`) or later (`delta > 0`) in the
    /// window list (tmux swap-window). Only the status-bar window list changes, so
    /// a repaint suffices — no PTY resize.
    fn do_move_window(&mut self, session: SessionId, delta: i32) {
        let moved = self
            .daemon
            .server
            .session_mut(session)
            .map(|s| s.move_active_window(delta))
            .unwrap_or(false);
        if moved {
            self.invalidate_session(session);
        }
    }

    fn do_split(&mut self, session: SessionId, dir: SplitDir) {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        if let Ok(Some((pid, reader))) = self.daemon.split_active(session, dir, size) {
            self.pane_session.insert(pid, session);
            spawn_pane_reader(pid, reader, self.tx.clone());
            // Re-fit every pane in the window to its exact layout rect (the new
            // pane was spawned at the content height; the split means both panes
            // are now smaller).
            self.daemon.resize_session(session, size);
        }
    }

    fn do_new_window(&mut self, session: SessionId) {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        if let Ok(Some((pid, reader))) = self.daemon.new_window(session, size) {
            self.pane_session.insert(pid, session);
            spawn_pane_reader(pid, reader, self.tx.clone());
            self.daemon.resize_session(session, size);
        }
    }

    /// Focus the next / previous / last-active window (tmux next/previous/last-
    /// window). Extracted so the keymap and `:` command surfaces share one impl.
    fn do_next_window(&mut self, session: SessionId) {
        if let Some(s) = self.daemon.server.session_mut(session) {
            s.focus_next_window();
        }
    }

    fn do_prev_window(&mut self, session: SessionId) {
        if let Some(s) = self.daemon.server.session_mut(session) {
            s.focus_prev_window();
        }
    }

    fn do_last_window(&mut self, session: SessionId) {
        if let Some(s) = self.daemon.server.session_mut(session) {
            s.focus_last_window();
        }
    }

    /// Focus the previously-active pane in the active window (tmux `last-pane`).
    fn do_last_pane(&mut self, session: SessionId) {
        if let Some(s) = self.daemon.server.session_mut(session) {
            let wid = s.active_window();
            if let Some(w) = s.window_mut(wid) {
                w.focus_last_pane();
            }
        }
    }

    /// Kill the active pane and run the pane-exit cascade (tmux `kill-pane`).
    fn do_kill_pane(&mut self, session: SessionId) {
        if let Some(pid) = self.active_pane(session) {
            let result = self.daemon.close_pane(session, pid);
            self.pane_session.remove(&pid);
            self.on_pane_exit(session, pid, result);
        }
    }

    /// Kill the active window and all its panes (tmux `kill-window`). Drops the
    /// window's PTYs, clears their pane->session mappings, and reuses the
    /// pane-exit cascade to notify clients / close the session when it was the
    /// last window.
    fn do_kill_window(&mut self, session: SessionId) {
        let (panes, result) = self.daemon.close_active_window(session);
        for pid in &panes {
            self.pane_session.remove(pid);
        }
        // Re-fit remaining panes (a closed window changes nothing layout-wise for
        // the now-active window, but a full repaint keeps the status/window list
        // correct). Pass a representative pane id for the exit event payload.
        let representative = panes.first().copied().unwrap_or(PaneId(0));
        self.on_pane_exit(session, representative, result);
    }

    /// A pane's child exited: apply remain-on-exit (keep dead) or cascade-close,
    /// then fire any `pane-exited` hook. With remain-on-exit the dead pane still
    /// exists, so a hook like `respawn-pane` can act on it.
    fn handle_pane_exited(&mut self, session: SessionId, pane: PaneId) {
        match self.daemon.pane_exited(session, pane) {
            Some(result) => {
                self.pane_session.remove(&pane);
                self.on_pane_exit(session, pane, result);
            }
            None => self.render_session(session),
        }
        // Fire the pane-exited hook if the session still exists.
        if self.daemon.server.session(session).is_some() {
            if let Some(cmd) = self.daemon.hook_command("pane-exited") {
                if let Some(client) = self.session_clients(session).first().copied() {
                    self.dispatch_command_line(client, session, &cmd);
                }
            }
        }
    }

    fn on_pane_exit(&mut self, session: SessionId, pane: PaneId, result: CascadeResult) {
        match result {
            CascadeResult::SessionClosed => {
                let gone: Vec<u64> = self
                    .clients
                    .iter()
                    .filter(|(_, h)| h.session == session)
                    .map(|(id, _)| *id)
                    .collect();
                for id in gone {
                    if let Some(h) = self.clients.remove(&id) {
                        let _ = h.out.send(ServerMsg::Event(Event::SessionClosed));
                        let _ = h.out.send(ServerMsg::Detached);
                    }
                    self.daemon.unregister_client(id);
                }
            }
            CascadeResult::NotFound => {}
            _ => {
                let ids = self.session_clients(session);
                for id in ids {
                    if let Some(h) = self.clients.get(&id) {
                        let _ = h.out.send(ServerMsg::Event(Event::PaneExited {
                            pane: pane.to_string(),
                            status: 0,
                        }));
                    }
                    self.render_client(id);
                }
            }
        }
    }

    fn active_pane(&self, session: SessionId) -> Option<PaneId> {
        let s = self.daemon.server.session(session)?;
        Some(s.window(s.active_window())?.active_pane())
    }

    fn session_clients(&self, session: SessionId) -> Vec<u64> {
        self.clients
            .iter()
            .filter(|(_, h)| h.session == session)
            .map(|(id, _)| *id)
            .collect()
    }

    fn render_session(&mut self, session: SessionId) {
        for id in self.session_clients(session) {
            self.render_client(id);
        }
    }

    fn render_client(&mut self, client_id: u64) {
        let Some(session) = self.clients.get(&client_id).map(|h| h.session) else {
            return;
        };
        if let Some(vt) = self.daemon.render_for_client(client_id, session) {
            if !vt.is_empty() {
                if let Some(h) = self.clients.get(&client_id) {
                    let _ = h.out.send(ServerMsg::Frame(vt.into_bytes()));
                }
            }
        }
    }

    fn list_sessions(&self) -> String {
        let mut out = String::new();
        for sid in self.daemon.server.session_ids() {
            if let Some(s) = self.daemon.server.session(sid) {
                out.push_str(&format!("{}: {} windows\n", s.name, s.window_count()));
            }
        }
        if out.is_empty() {
            out.push_str("(no sessions)\n");
        }
        out
    }
}

/// Background ticker: nudge the control loop on a fixed interval so it can poll
/// pane children for exit. Cheap (one message every 250ms); the loop only does
/// work when a child has actually exited. Ends when the control loop is gone.
fn spawn_ticker(tx: Sender<Msg>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if tx.send(Msg::Tick).is_err() {
            break;
        }
    });
}

/// Accept thread: for each connection, split it and spawn reader+writer threads.
fn spawn_accept<L>(mut listener: L, tx: Sender<Msg>)
where
    L: Listener + 'static,
{
    std::thread::spawn(move || {
        #[allow(clippy::while_let_loop)] // body has accept-error handling
        loop {
            match listener.accept() {
                Ok(conn) => {
                    // Service each client on its own thread so the accept loop
                    // stays free to take more connections — spawn_client runs a
                    // blocking reader loop for the life of the client, so calling
                    // it inline would serve only one client at a time (no
                    // multi-client attach, and a second connection would hang).
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let _ = spawn_client(conn, tx);
                    });
                }
                Err(_) => break,
            }
        }
    });
}

/// Set up a connected client: read the first frame, register it, then run the
/// reader loop while a writer thread drains its outbound channel.
fn spawn_client<C: Transport + 'static>(conn: C, tx: Sender<Msg>) -> std::io::Result<()> {
    let (mut reader, mut writer) = conn.split()?;

    // Protocol handshake: the client sends its Hello first; we validate the
    // version and reply with ours. A mismatch is rejected loudly so skewed
    // builds fail fast instead of corrupting the byte stream.
    let hello_bytes = reader
        .read_frame()?
        .ok_or_else(|| std::io::Error::other("client closed before handshake"))?;
    let client_hello: lumux_core::proto::Hello = lumux_core::proto::decode(&hello_bytes)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Err(mismatch) = client_hello.check() {
        // Best-effort notice, then drop the connection.
        let _ = writer
            .write_frame(&encode(&ServerMsg::Error(mismatch.to_string())).unwrap_or_default());
        return Err(std::io::Error::other(mismatch.to_string()));
    }
    let server_hello =
        lumux_core::proto::Hello::current(format!("lumux_server/{}", crate::DAEMON_VERSION));
    writer
        .write_frame(&encode(&server_hello).map_err(|e| std::io::Error::other(e.to_string()))?)?;

    // First post-handshake frame must be Attach/NewSession.
    let first_bytes = reader
        .read_frame()?
        .ok_or_else(|| std::io::Error::other("client closed before attach"))?;
    let first: ClientMsg = lumux_core::proto::decode(&first_bytes)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let (out_tx, out_rx) = channel::<ServerMsg>();
    let (reply_tx, reply_rx) = channel::<u64>();
    tx.send(Msg::ClientConnected {
        first,
        out: out_tx,
        reply: reply_tx,
    })
    .map_err(|_| std::io::Error::other("control loop gone"))?;

    let client_id = reply_rx.recv().unwrap_or(0);
    if client_id == 0 {
        // Rejected (duplicate session name, spawn failure, …). on_connect put an
        // Error on out_rx before replying 0; flush it to the socket so the client
        // sees the reason, since the writer thread below never starts.
        while let Ok(msg) = out_rx.try_recv() {
            let _ = writer.write_frame(&encode(&msg).unwrap_or_default());
        }
        return Ok(());
    }

    // Writer thread: drain ServerMsgs to the socket.
    std::thread::spawn(move || {
        for msg in out_rx {
            let detach = matches!(msg, ServerMsg::Detached);
            if let Ok(frame) = encode(&msg) {
                if writer.write_frame(&frame).is_err() {
                    break;
                }
            }
            if detach {
                break;
            }
        }
    });

    // Reader loop (this thread): forward client frames as ClientInput.
    #[allow(clippy::while_let_loop)] // body breaks on detach as well as EOF
    loop {
        match reader.read_frame() {
            Ok(Some(bytes)) => {
                if let Ok(msg) = lumux_core::proto::decode::<ClientMsg>(&bytes) {
                    let detach = matches!(msg, ClientMsg::Detach);
                    if tx.send(Msg::ClientInput { client_id, msg }).is_err() {
                        break;
                    }
                    if detach {
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    let _ = tx.send(Msg::ClientGone { client_id });
    Ok(())
}

/// Per-pane reader thread: PTY output -> PaneOutput, PaneExited on EOF.
fn spawn_pane_reader<R: Read + Send + 'static>(pane: PaneId, mut reader: R, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(Msg::PaneOutput {
                            pane,
                            bytes: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(Msg::PaneExited { pane });
    });
}
