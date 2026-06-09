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
use lumux_core::keymap::{Action, CopyKey, Reaction, SessionKey};
use lumux_core::layout::Direction;
use lumux_core::model::{CascadeResult, PaneId, SessionId, SplitDir};
use lumux_core::proto::{encode, ClientMsg, Command, Event, ServerMsg};
use lumux_core::traits::{FrameReader, FrameWriter, Listener, Pty, PtySize, PtySystem, Transport};

use crate::daemon::Daemon;

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
}

/// State owned exclusively by the control loop.
struct Loop<S: PtySystem> {
    daemon: Daemon<S>,
    clients: HashMap<u64, ClientHandle>,
    pane_session: HashMap<PaneId, SessionId>,
    tx: Sender<Msg>,
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
    };
    // Hold one tx so the loop never sees a disconnected channel while idle.
    drop(tx);

    // The daemon auto-exits once it has served at least one client and then goes
    // idle (no sessions, no clients). `served` gates this so the periodic Tick —
    // which now drives the loop before any client connects — can't trip the
    // emptiness check at startup and kill a freshly-bound daemon.
    let mut served = false;
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
                let rang = self.daemon.feed_pane(pane, &bytes);
                if let Some(sid) = self.pane_session.get(&pane).copied() {
                    if rang {
                        // Forward a bell to each attached client (tmux's default
                        // visual-bell-off behavior: the client emits a BEL so the
                        // user's own terminal flashes/beeps per its settings).
                        for id in self.session_clients(sid) {
                            if let Some(h) = self.clients.get(&id) {
                                let _ = h.out.send(ServerMsg::Event(Event::Bell));
                            }
                        }
                    }
                    self.render_session(sid);
                }
            }
            Msg::PaneExited { pane } => {
                if let Some(sid) = self.pane_session.remove(&pane) {
                    let result = self.daemon.close_pane(sid, pane);
                    self.on_pane_exit(sid, pane, result);
                }
            }
            Msg::Tick => {
                // Reap children that exited without the reader seeing EOF (ConPTY).
                for pane in self.daemon.reap_exited_panes() {
                    if let Some(sid) = self.pane_session.remove(&pane) {
                        let result = self.daemon.close_pane(sid, pane);
                        self.on_pane_exit(sid, pane, result);
                    }
                }
            }
        }
    }

    fn on_connect(&mut self, first: ClientMsg, out: Sender<ServerMsg>, reply: Sender<u64>) {
        let (session, size) = match self.resolve_attach(&first) {
            Some(v) => v,
            None => {
                // Spawn failed (bad shell argv) or malformed first message.
                let _ = out.send(ServerMsg::Error(
                    "failed to start session (check the shell command)".into(),
                ));
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
        self.clients
            .insert(client_id, ClientHandle { out, session });
        let _ = reply.send(client_id);
        self.render_client(client_id);
    }

    /// Determine the session+size for an attach/new-session first message,
    /// spawning the session if needed. Returns None if spawning failed (e.g. a
    /// bad shell argv) so the caller can reject the client cleanly instead of
    /// crashing the daemon.
    fn resolve_attach(&mut self, first: &ClientMsg) -> Option<(SessionId, PtySize)> {
        match first {
            ClientMsg::Attach { session, size } => {
                let sz: PtySize = (*size).into();
                let existing = match session {
                    Some(name) => self.daemon.server.find_session_by_name(name),
                    None => self.daemon.server.session_ids().first().copied(),
                };
                if let Some(sid) = existing {
                    Some((sid, sz))
                } else {
                    let name = session.clone().unwrap_or_else(|| "0".into());
                    Some((self.spawn_session(name, None, sz)?, sz))
                }
            }
            ClientMsg::NewSession { name, shell, size } => {
                let sz: PtySize = (*size).into();
                let shell = shell.clone().map(|s| vec![s]);
                let name = name.clone().unwrap_or_else(|| "0".into());
                Some((self.spawn_session(name, shell, sz)?, sz))
            }
            _ => None,
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
                            if let Some(pid) = self.active_pane(session) {
                                let _ = self.daemon.write_pane(pid, &data);
                            }
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
                    }
                }
                self.render_session(session);
            }
            ClientMsg::Resize(size) => {
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
                if let Some(s) = self.daemon.server.session_mut(session) {
                    let ids = s.window_ids();
                    if let Some(&wid) = ids.get(index as usize) {
                        s.focus_window(wid);
                    }
                }
                self.render_session(session);
            }
            Command::SendKeys { keys } => {
                // Inject keys as if typed into the active pane (bypassing the
                // prefix keymap — scripting goes straight to the shell).
                if let Some(pid) = self.active_pane(session) {
                    let _ = self.daemon.write_pane(pid, &keys);
                }
            }
            Command::SourceFile { path } => {
                let reply = match std::fs::read_to_string(&path) {
                    Ok(text) => match lumux_core::config::Config::from_toml(&text) {
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
            Action::NextWindow => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    s.focus_next_window();
                }
            }
            Action::PrevWindow => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    s.focus_prev_window();
                }
            }
            Action::SelectWindow(n) => {
                if let Some(s) = self.daemon.server.session_mut(session) {
                    let ids = s.window_ids();
                    if let Some(&wid) = ids.get(n as usize) {
                        s.focus_window(wid);
                    }
                }
            }
            Action::SelectPaneLeft => self.select_pane(session, Direction::Left),
            Action::SelectPaneRight => self.select_pane(session, Direction::Right),
            Action::SelectPaneUp => self.select_pane(session, Direction::Up),
            Action::SelectPaneDown => self.select_pane(session, Direction::Down),
            Action::KillPane => {
                if let Some(pid) = self.active_pane(session) {
                    let result = self.daemon.close_pane(session, pid);
                    self.pane_session.remove(&pid);
                    self.on_pane_exit(session, pid, result);
                }
            }
            Action::EnterCopyMode => {
                self.daemon.enter_copy_mode(client_id, session);
            }
            Action::ReloadConfig => self.reload_config(client_id, session),
            Action::ShowHelp => self.daemon.toggle_help(client_id),
            Action::ChooseSession => self.daemon.open_chooser(client_id),
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
        use lumux_core::mouse::{self, MouseKind};
        let mut rest = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if let Some((ev, used)) = mouse::parse(&bytes[i..]) {
                match ev.kind {
                    MouseKind::Down(_) => self.mouse_select_pane(session, ev.col, ev.row),
                    MouseKind::ScrollUp => self.mouse_scroll(client_id, session, true),
                    MouseKind::ScrollDown => self.mouse_scroll(client_id, session, false),
                    MouseKind::Drag(_) => self.mouse_drag(session, ev.col, ev.row),
                    MouseKind::Up(_) => self.mouse_drag_end(),
                }
                i += used;
            } else {
                rest.push(bytes[i]);
                i += 1;
            }
        }
        rest
    }

    /// Click: focus the pane under the cursor.
    fn mouse_select_pane(&mut self, session: SessionId, col: u16, row: u16) {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
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

    /// Wheel: enter copy-mode (if not already) and scroll the history.
    fn mouse_scroll(&mut self, client_id: u64, session: SessionId, up: bool) {
        use lumux_core::keymap::CopyKey;
        if !self.daemon.in_copy_mode(client_id) {
            if !up {
                return; // scrolling down in live view does nothing
            }
            self.daemon.enter_copy_mode(client_id, session);
        }
        let key = if up { CopyKey::Up } else { CopyKey::Down };
        // Scroll a few lines per wheel notch.
        for _ in 0..3 {
            self.daemon.copy_navigate(client_id, session, key);
        }
    }

    /// Drag: adjust the split ratio under the cursor (resize panes).
    fn mouse_drag(&mut self, session: SessionId, col: u16, row: u16) {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        let viewport = lumux_core::layout::Rect::new(0, 0, size.cols, size.rows.saturating_sub(1));
        if let Some(s) = self.daemon.server.session_mut(session) {
            let wid = s.active_window();
            if let Some(w) = s.window_mut(wid) {
                w.resize_split_at(col, row, viewport);
            }
        }
    }

    fn mouse_drag_end(&mut self) {
        // Stateless for v1: each drag event re-derives the ratio from position.
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

    /// Re-source the daemon's config file (tmux prefix r) and flash a message.
    fn reload_config(&mut self, client_id: u64, session: SessionId) {
        let path = crate::config_path();
        let msg = match std::fs::read_to_string(&path) {
            Ok(text) => match lumux_core::config::Config::from_toml(&text) {
                Ok(cfg) => {
                    self.daemon.set_config(cfg);
                    "lumux configuration reloaded".to_string()
                }
                Err(e) => format!("config error: {e}"),
            },
            Err(_) => format!("no config at {}", path.display()),
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
            SessionKey::Cancel => {
                self.daemon.chooser_cancel(client_id);
                None
            }
            SessionKey::Confirm => {
                let new_session = self.daemon.chooser_confirm(client_id);
                if let Some(sid) = new_session {
                    // Update the event-loop's client->session mapping and force a
                    // full repaint of the newly-shown session.
                    if let Some(h) = self.clients.get_mut(&client_id) {
                        h.session = sid;
                    }
                    self.daemon.invalidate_client(client_id);
                }
                new_session
            }
        }
    }

    /// Drive copy-mode navigation/selection/yank for a client. Space/'v' starts
    /// a selection, Enter/'y' yanks (and forwards an OSC-52 clipboard sequence),
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
            _ => {
                self.daemon.copy_navigate(client_id, session, ck);
            }
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
                    if spawn_client(conn, tx.clone()).is_err() {
                        continue;
                    }
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
