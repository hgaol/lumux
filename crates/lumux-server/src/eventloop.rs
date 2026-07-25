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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::Read;
use std::sync::mpsc::{channel, Sender};

use lumux_core::copymode::osc52;
use lumux_core::keymap::{Action, BufferKey, CopyKey, PromptKey, Reaction, SearchKey, SessionKey};
use lumux_core::layout::Direction;
use lumux_core::model::{CascadeResult, PaneId, SessionId, SplitDir};
use lumux_core::proto::{encode, ClientMsg, Command, ControlRequest, Event, ServerMsg};
use lumux_core::traits::{FrameReader, FrameWriter, Listener, Pty, PtySize, PtySystem, Transport};

use crate::daemon::{Daemon, InteractionMap, PaneRuntime, SidebarClick};

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
    /// One-shot CLI command. It has no terminal viewport and never becomes an
    /// interactive client, so it cannot affect PTY sizing or rendering.
    Control {
        request: ControlRequest,
        out: Sender<ServerMsg>,
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
    /// Host-window focus is unknown until DEC mode 1004 emits its first report.
    /// Unknown is deliberately observable: terminals that do not implement
    /// focus reporting retain the historical, conservative behavior.
    outer_focus: OuterFocus,
    /// Bytes of an SGR mouse report that arrived truncated at a frame boundary
    /// (SSH/TCP can split one report across reads). Held here and prepended to
    /// the next frame so the report is reassembled instead of leaking as text.
    pending_mouse: Vec<u8>,
    /// Epoch attached to a mouse report whose bytes span protocol messages.
    /// The first fragment determines which rendered frame its coordinates use.
    pending_mouse_epoch: Option<u64>,
    /// Per-client sequence for composed frames. Epoch zero is reserved for the
    /// pre-first-frame state published by attach.
    next_frame_epoch: u64,
    /// Bounded interaction history for frames already sent to this client. A
    /// slow terminal may report input against an older stdout-applied epoch even
    /// after newer frames have been queued on the independent writer thread.
    frame_history: VecDeque<FrameSnapshot>,
}

#[derive(Clone)]
struct FrameSnapshot {
    epoch: u64,
    interactions: InteractionMap,
}

#[derive(Clone)]
enum InputFrame {
    /// Legacy `ClientMsg::Input`: preserve live hit-testing for protocol fixtures
    /// and direct control tests that do not model an attach terminal.
    Live,
    /// The client named an epoch no longer retained (or epoch zero before its
    /// first frame). Mouse input fails closed rather than targeting newer UI.
    Missing,
    Applied(Box<FrameSnapshot>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum OuterFocus {
    #[default]
    Unknown,
    Focused,
    Lost,
}

impl OuterFocus {
    fn may_observe(self) -> bool {
        self != Self::Lost
    }
}

/// A fact that may make an agent pane observable. All Done -> Idle policy is
/// coordinated from this seam so focus/navigation callers only report what
/// happened; they do not decide which panes count as seen or how to repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisibilityTransition {
    /// Physical input or a DEC 1004 focus-gain report proves this client can
    /// observe its active window.
    ClientObserved(u64),
    /// A DEC 1004 focus-loss report makes this client ineligible to acknowledge
    /// completions until later input/focus gain.
    ClientBlurred(u64),
    /// Navigation, attach, or a topology change exposed the session's current
    /// rendered pane set.
    SessionExposed(SessionId),
    /// A lifecycle report may have completed a pane that was already on screen.
    AgentReported(PaneId),
}

/// State owned exclusively by the control loop.
struct Loop<S: PtySystem> {
    daemon: Daemon<S>,
    clients: HashMap<u64, ClientHandle>,
    pane_session: HashMap<PaneId, SessionId>,
    tx: Sender<Msg>,
    /// Last time auto-save wrote the state file, to throttle to ~every 15s.
    last_autosave: std::time::Instant,
    /// Throttle for agent process detection (the tick runs far more often than
    /// an agent can plausibly start or stop).
    last_agent_scan: std::time::Instant,
    /// Where the session snapshot is saved/restored (tmux-resurrect).
    state_path: std::path::PathBuf,
    /// Per-client deadline for an active repeat window (tmux `bind -r`): while
    /// the keymap is in `Mode::Repeat`, the same key re-fires without the
    /// prefix until this deadline; `Msg::Tick` expires it back to Normal.
    repeat_deadlines: HashMap<u64, std::time::Instant>,
    /// Clients whose final composed view must be emitted when the current
    /// control-loop message finishes. Nested mutations request rendering here
    /// instead of sending intermediate frames, so one serialized mutation batch
    /// produces at most one coherent frame per client.
    pending_renders: BTreeSet<u64>,
}

/// How long a repeatable binding (tmux `bind -r`) stays armed after firing,
/// matching tmux's `repeat-time` default (500ms).
const REPEAT_TIME: std::time::Duration = std::time::Duration::from_millis(500);
const FRAME_HISTORY_LIMIT: usize = 32;

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
    let pane_runtime = PaneRuntime::for_listener(listener.endpoint());
    let (tx, rx) = channel::<Msg>();
    spawn_accept(listener, tx.clone());
    spawn_ticker(tx.clone());

    let mut daemon = Daemon::with_pane_runtime(pty_system, pane_runtime);
    daemon.set_config(config);
    let mut lp = Loop {
        daemon,
        clients: HashMap::new(),
        pane_session: HashMap::new(),
        tx: tx.clone(),
        last_autosave: std::time::Instant::now(),
        last_agent_scan: std::time::Instant::now(),
        state_path,
        repeat_deadlines: HashMap::new(),
        pending_renders: BTreeSet::new(),
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
        if matches!(msg, Msg::ClientConnected { .. } | Msg::Control { .. }) {
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
                // Detach != kill: panes keep running. Reconcile the session
                // through the same geometry seam used by attach/resize/sidebar
                // changes; a departing smallest client may let every window grow.
                let session = self.clients.get(&client_id).map(|client| client.session);
                self.daemon.unregister_client(client_id);
                self.clients.remove(&client_id);
                if let Some(session) = session {
                    self.reflow_session(session);
                }
            }
            Msg::Control { request, out } => {
                if let Some(reply) = self.on_control(request) {
                    let _ = out.send(ServerMsg::Reply(reply));
                }
                let _ = out.send(ServerMsg::Detached);
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
                // Detect agents by their processes so a launch shows up in the
                // sidebar immediately, without waiting for the agent's own
                // hooks. Throttled — a tick is 250ms, far finer than needed.
                if self.last_agent_scan.elapsed() >= std::time::Duration::from_secs(1) {
                    self.last_agent_scan = std::time::Instant::now();
                    if self.daemon.refresh_detected_agents() {
                        self.render_global_views();
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
                // Expire any repeat window (tmux `bind -r`) whose deadline has
                // passed with no matching keypress.
                let now = std::time::Instant::now();
                let expired: Vec<u64> = self
                    .repeat_deadlines
                    .iter()
                    .filter(|(_, &deadline)| now >= deadline)
                    .map(|(&id, _)| id)
                    .collect();
                for id in expired {
                    self.repeat_deadlines.remove(&id);
                    if let Some(k) = self.daemon.keymap_mut(id) {
                        k.cancel_repeat();
                    }
                }
            }
        }
        self.flush_renders();
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
        // New sessions are spawned at the outer terminal size. Reconcile all
        // windows immediately so a default-on sidebar never hides columns and
        // inactive windows cannot retain a stale grid.
        self.reconcile_session_geometry(session);
        self.daemon
            .ensure_sidebar_session_visible(client_id, session);
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
                outer_focus: OuterFocus::Unknown,
                pending_mouse: Vec::new(),
                pending_mouse_epoch: None,
                next_frame_epoch: 1,
                frame_history: VecDeque::new(),
            },
        );
        let _ = reply.send(client_id);
        // Attaching makes the active window visible. Match Herdr's active-tab
        // contract by acknowledging every completed split in that window before
        // its first frame; the visibility coordinator repaints globally when it
        // changes state.
        if !self.coordinate_visibility(VisibilityTransition::SessionExposed(session)) {
            self.render_session(session);
        }
        // Preserve attach ordering even when a configured hook performs slow
        // synchronous work: the Attached acknowledgement and first coherent
        // frame must reach the terminal before the hook starts.
        self.flush_renders();
        // tmux fires client-attached once a client connects to a session.
        self.fire_hook(session, "client-attached");
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
                    let sid = self.spawn_session(name, None, sz).ok_or_else(|| {
                        "failed to start session (check the shell command)".to_string()
                    })?;
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
                let sid = self.spawn_session(name, shell, sz).ok_or_else(|| {
                    "failed to start session (check the shell command)".to_string()
                })?;
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
                // Session rows are a global projection. Centralizing their
                // creation repaint here covers interactive new-session,
                // detached command-prompt creation, and first attach alike.
                self.render_global_views();
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
            input @ (ClientMsg::Input(_) | ClientMsg::InputAt { .. }) => {
                let (bytes, frame_epoch) = match input {
                    ClientMsg::Input(bytes) => (bytes, None),
                    ClientMsg::InputAt { bytes, frame_epoch } => (bytes, Some(frame_epoch)),
                    _ => unreachable!(),
                };
                // Physical keyboard/mouse input proves this host terminal is
                // focused even if its last DEC 1004 notification said Lost
                // (focus-gain reports can be dropped by terminal transports).
                // Restore observability before interpreting the input so a
                // click on a visible Done agent acknowledges it in this same
                // coalesced render phase.
                let input_seen =
                    self.coordinate_visibility(VisibilityTransition::ClientObserved(client_id));
                // Any input dismisses a pending display-message (tmux behavior).
                self.daemon.clear_message(client_id);
                // Mouse reporting sequences are intercepted here (when enabled)
                // and never reach the keymap or the shell; everything else is
                // forwarded to the keymap as before.
                let keyboard = if self.daemon.mouse_enabled() {
                    self.extract_and_handle_mouse(client_id, session, &bytes, frame_epoch)
                } else {
                    bytes.clone()
                };
                // A sidebar click handled above can switch this client to a
                // different session. Route any keyboard bytes batched after the
                // SGR mouse sequence to the newly selected session, never the
                // snapshot taken before mouse extraction.
                let routed_session = self
                    .clients
                    .get(&client_id)
                    .map(|client| client.session)
                    .unwrap_or(session);
                let reactions = self
                    .daemon
                    .keymap_mut(client_id)
                    .map(|k| k.feed(&keyboard))
                    .unwrap_or_default();
                // Track (or clear) the repeat-window deadline: Msg::Tick expires
                // it back to Normal if no matching key arrives within
                // repeat-time, so a repeatable binding doesn't stay armed
                // forever if the user just stops pressing it.
                match self.daemon.keymap_mut(client_id).map(|k| k.mode()) {
                    Some(lumux_core::keymap::Mode::Repeat(_)) => {
                        self.repeat_deadlines
                            .insert(client_id, std::time::Instant::now() + REPEAT_TIME);
                    }
                    _ => {
                        self.repeat_deadlines.remove(&client_id);
                    }
                }
                let mut session = routed_session;
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
                            Some(n) => {
                                self.pick_numbered_pane(client_id, session, n);
                            }
                            None => self.daemon.hide_pane_numbers(client_id),
                        },
                    }
                    // Any reaction can reach a configured command chain that
                    // switches this client. Treat the client handle as the
                    // routing authority between ordered reactions so trailing
                    // bytes in the same input frame follow the switch too.
                    if let Some(client) = self.clients.get(&client_id) {
                        session = client.session;
                    }
                }
                if !input_seen {
                    self.render_session(session);
                }
            }
            ClientMsg::FocusChanged { focused } => {
                // Losing host focus is state-only. On gain, acknowledge every
                // Done pane currently rendered for this client's session before
                // the control loop flushes one coherent global-view update.
                let transition = if focused {
                    VisibilityTransition::ClientObserved(client_id)
                } else {
                    VisibilityTransition::ClientBlurred(client_id)
                };
                self.coordinate_visibility(transition);
            }
            ClientMsg::Resize(size) => {
                // Update this client's stored size first so effective_size (the
                // min over clients) reflects the new dimensions — otherwise the
                // composed screen, and the right-aligned status segment, stay at
                // the attach-time width and overflow/wrap on the real terminal.
                self.daemon.server.set_client_size(client_id, size.into());
                // Never size PTYs from one client's raw dimensions. The model's
                // effective size is the minimum across clients, and every window
                // shares that geometry even while inactive.
                self.reflow_session(session);
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
        if let Some(reply) = self.execute_command(Some(session), cmd) {
            // Attached clients write Reply text directly to their terminal,
            // outside ClientRenderer's screen model. Emit any command mutation
            // first, then invalidate and queue a full repair after the text so
            // the renderer baseline and the real terminal cannot diverge.
            self.flush_renders();
            if let Some(client) = self.clients.get(&client_id) {
                let _ = client.out.send(ServerMsg::Reply(reply));
            }
            self.daemon.invalidate_client(client_id);
            self.render_client(client_id);
        }
    }

    /// Execute a one-shot CLI request without registering a render client. A
    /// caller pane selects the same session context an attached client would;
    /// commands issued outside a pane retain the historical first-session
    /// fallback.
    fn on_control(&mut self, request: ControlRequest) -> Option<String> {
        let session = request
            .pane
            .and_then(|pane| self.pane_session.get(&pane).copied())
            .or_else(|| self.daemon.server.session_ids().first().copied());
        self.execute_command(session, request.command)
    }

    /// Command execution is shared by attached and one-shot callers. Rendering
    /// is an outcome of a command, never a prerequisite for invoking it.
    fn execute_command(&mut self, session: Option<SessionId>, cmd: Command) -> Option<String> {
        match cmd {
            Command::SplitWindow { horizontal } => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                let dir = if horizontal {
                    SplitDir::Horizontal
                } else {
                    SplitDir::Vertical
                };
                self.do_split(session, dir);
                self.render_session(session);
                None
            }
            Command::NewWindow { .. } => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                self.do_new_window(session);
                self.render_session(session);
                None
            }
            Command::KillWindow => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                self.do_kill_window(session);
                None
            }
            Command::NextWindow => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                self.do_next_window(session);
                self.render_session(session);
                None
            }
            Command::PrevWindow => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                self.do_prev_window(session);
                self.render_session(session);
                None
            }
            Command::ListSessions => Some(self.list_sessions()),
            Command::SelectWindow { index } => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                self.select_window_by_number(session, index);
                self.render_session(session);
                None
            }
            Command::SendKeys { keys } => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                // Inject keys as if typed into the active pane (bypassing the
                // prefix keymap — scripting goes straight to the shell).
                if let Some(pid) = self.active_pane(session) {
                    let _ = self.daemon.write_pane(pid, &keys);
                }
                None
            }
            Command::RenameWindow { name } => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                if let Some(window) = self
                    .daemon
                    .server
                    .session(session)
                    .map(|session| session.active_window())
                {
                    self.rename_window(session, window, name);
                }
                None
            }
            Command::RenameSession { name } => {
                let Some(session) = session else {
                    return Some("no sessions\n".into());
                };
                self.rename_session(session, name);
                None
            }
            Command::SourceFile { path } => Some(match std::fs::read_to_string(&path) {
                Ok(text) => match crate::parse_config(std::path::Path::new(&path), &text) {
                    Ok(cfg) => {
                        self.daemon.set_config(cfg);
                        self.reflow_all_sessions();
                        format!("sourced {path}\n")
                    }
                    Err(e) => format!("config error: {e}\n"),
                },
                Err(e) => format!("cannot read {path}: {e}\n"),
            }),
            Command::KillSession { target } => {
                // Match by name; fall back to the current session.
                if let Some(sid) = self.daemon.server.find_session_by_name(&target).or(session) {
                    self.kill_whole_session(sid);
                }
                None
            }
            Command::KillServer => {
                let ids = self.daemon.server.session_ids();
                for id in ids {
                    self.kill_whole_session(id);
                }
                None
            }
            Command::ReportAgentState { pane, report } => {
                // The reporting process (an agent hook) is detached from the
                // interactive client, so the target pane travels in the payload
                // as a typed id. The daemon rejects stale report sequences.
                let mut changed = self.pane_session.contains_key(&pane)
                    && self.daemon.report_agent_state(pane, report);
                // Match Herdr's seen contract. A completion reported while its
                // pane is already rendered for an observing client has been
                // observed; only background completions remain `done`.
                changed |= self.coordinate_visibility(VisibilityTransition::AgentReported(pane));
                // Status shows in the sidebar / chooser, which span all
                // sessions, so refresh every client — not just this one.
                if changed {
                    self.render_global_views();
                }
                None
            }
            Command::ClearAgentState { pane, clear } => {
                if self.daemon.clear_agent_status(pane, clear) {
                    self.render_global_views();
                }
                None
            }
        }
    }

    /// Kill a session and notify/disconnect its clients.
    fn kill_whole_session(&mut self, session: SessionId) {
        for pane in self.daemon.close_session(session) {
            self.pane_session.remove(&pane);
        }
        let gone = self.session_clients(session);
        for id in gone {
            if let Some(h) = self.clients.remove(&id) {
                let _ = h.out.send(ServerMsg::Event(Event::SessionClosed));
                let _ = h.out.send(ServerMsg::Detached);
            }
            self.daemon.unregister_client(id);
        }
        self.render_global_views();
    }

    /// Switch `client_id` to `sid`, then reconcile both sessions. This is the
    /// only seam allowed to move an interactive client: the model registry,
    /// event-loop routing, smallest-client geometry, and repaint stay atomic.
    fn switch_client_session(&mut self, client_id: u64, sid: SessionId) {
        let Some(previous_session) = self.clients.get(&client_id).map(|client| client.session)
        else {
            return;
        };
        if previous_session == sid {
            self.coordinate_visibility(VisibilityTransition::SessionExposed(sid));
            return;
        }
        if !self.daemon.server.set_client_session(client_id, sid) {
            return;
        }
        if let Some(h) = self.clients.get_mut(&client_id) {
            h.session = sid;
        }
        self.daemon.invalidate_client(client_id);
        // Batch both geometry changes and the newly-visible acknowledgement
        // before emitting any frame. Rendering the target between these steps
        // would briefly show Done and then Idle (and could pair new layouts with
        // old-sized grids).
        self.reconcile_session_geometry(previous_session);
        self.reconcile_session_geometry(sid);
        self.daemon.ensure_sidebar_session_visible(client_id, sid);
        self.coordinate_visibility(VisibilityTransition::SessionExposed(sid));
        self.render_global_views();
    }

    /// Coordinate every transition that can make agent state visible. This is
    /// the sole policy seam for host focus, rendered-pane selection, Done ->
    /// Idle acknowledgement, and the global repaint that follows a change.
    ///
    /// Rendering only queues work until the control-loop message completes, so
    /// callers may continue geometry/topology mutations after this returns and
    /// still emit one coherent final frame.
    fn coordinate_visibility(&mut self, transition: VisibilityTransition) -> bool {
        let panes = match transition {
            VisibilityTransition::ClientObserved(client_id) => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                client.outer_focus = OuterFocus::Focused;
                let session = client.session;
                self.observable_panes(session)
            }
            VisibilityTransition::ClientBlurred(client_id) => {
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.outer_focus = OuterFocus::Lost;
                }
                return false;
            }
            VisibilityTransition::SessionExposed(session) => self.observable_panes(session),
            VisibilityTransition::AgentReported(pane) => {
                if self.agent_pane_is_visible(pane) {
                    vec![pane]
                } else {
                    Vec::new()
                }
            }
        };

        let mut changed = false;
        for pane in panes {
            // Do not short-circuit: every visible split must be acknowledged.
            changed |= self.daemon.acknowledge_agent(pane);
        }
        if changed {
            self.render_global_views();
        }
        changed
    }

    /// Return the session's rendered panes only when some attached client can
    /// observe them. Model focus survives detach, but visibility does not.
    fn observable_panes(&self, session: SessionId) -> Vec<PaneId> {
        if self.session_has_observing_client(session) {
            self.visible_panes_in_active_window(session)
        } else {
            Vec::new()
        }
    }

    /// The panes actually rendered in a session's active window. Zoom hides all
    /// but the zoomed pane; without zoom, every split is visible.
    fn visible_panes_in_active_window(&self, session: SessionId) -> Vec<PaneId> {
        self.daemon
            .server
            .session(session)
            .and_then(|session| session.window(session.active_window()))
            .map(|window| match window.zoomed_pane() {
                Some(pane) => vec![pane],
                None => window.pane_ids(),
            })
            .unwrap_or_default()
    }

    /// Whether a pane is currently rendered for at least one connected client.
    /// Window focus and zoom are session-global, so this mirrors the active
    /// window's rendered pane projection.
    fn agent_pane_is_visible(&self, pane: PaneId) -> bool {
        self.pane_session.get(&pane).is_some_and(|session| {
            if !self.session_has_observing_client(*session) {
                return false;
            }
            self.visible_panes_in_active_window(*session)
                .contains(&pane)
        })
    }

    /// Whether at least one client can currently observe this session. A client
    /// remains eligible until its terminal explicitly reports focus lost.
    fn session_has_observing_client(&self, session: SessionId) -> bool {
        self.clients
            .values()
            .any(|client| client.session == session && client.outer_focus.may_observe())
    }

    /// Apply a focus mutation, then expose the resulting rendered pane set when
    /// the target was valid. `accepted` deliberately means the focus request
    /// resolved, not necessarily that the selected id changed: selecting the
    /// already-active target is still evidence that its contents were seen.
    fn coordinate_focus(
        &mut self,
        session: SessionId,
        focus: impl FnOnce(&mut Daemon<S>) -> bool,
    ) -> bool {
        let accepted = focus(&mut self.daemon);
        if accepted {
            self.coordinate_visibility(VisibilityTransition::SessionExposed(session));
        }
        accepted
    }

    /// Apply a topology mutation, then expose the resulting rendered pane set.
    /// Operations return `None` for a rejected/no-op mutation and may return any
    /// payload needed for their follow-up work (for example a newly spawned pane
    /// reader). Keeping zoom-clearing model calls behind this seam makes
    /// Done -> Idle reconciliation an invariant of topology changes rather than a
    /// convention each split/layout/swap caller must remember independently.
    fn coordinate_topology<T>(
        &mut self,
        session: SessionId,
        mutation: impl FnOnce(&mut Daemon<S>) -> Option<T>,
    ) -> Option<T> {
        let result = mutation(&mut self.daemon)?;
        self.coordinate_visibility(VisibilityTransition::SessionExposed(session));
        Some(result)
    }

    /// Focus a window through the shared focus/visibility seam.
    fn focus_window(&mut self, session: SessionId, window: lumux_core::model::WindowId) -> bool {
        self.coordinate_focus(session, |daemon| {
            daemon
                .server
                .session_mut(session)
                .is_some_and(|session| session.focus_window(window))
        })
    }

    /// Focus a pane in the active window through the shared focus/visibility
    /// seam. Invalid or stale pane ids remain total no-ops.
    fn focus_pane(&mut self, session: SessionId, pane: PaneId) -> bool {
        self.coordinate_focus(session, |daemon| {
            daemon.server.session_mut(session).is_some_and(|session| {
                let window = session.active_window();
                session
                    .window_mut(window)
                    .is_some_and(|window| window.focus_pane(pane))
            })
        })
    }

    /// Resolve the display-panes selection and route its focus through the same
    /// coordinator used by mouse, keyboard, prompt, and sidebar navigation.
    fn pick_numbered_pane(&mut self, client_id: u64, session: SessionId, number: u32) -> bool {
        self.coordinate_focus(session, |daemon| {
            daemon
                .pick_pane_number(client_id, session, number)
                .is_some()
        })
    }

    /// Rename through the global-view seam. Session names are embedded in both
    /// sidebar sections and in every open chooser, including clients attached
    /// to other sessions.
    fn rename_session(&mut self, session: SessionId, name: String) -> bool {
        let changed = self
            .daemon
            .server
            .session_mut(session)
            .is_some_and(|session| {
                if session.name == name {
                    false
                } else {
                    session.name = name;
                    true
                }
            });
        if changed {
            self.render_global_views();
        }
        changed
    }

    /// Window names appear in the global chooser. Keep their mutation behind
    /// the same projection lifecycle as prompt and CLI session renames.
    fn rename_window(
        &mut self,
        session: SessionId,
        window: lumux_core::model::WindowId,
        name: String,
    ) -> bool {
        let changed = self
            .daemon
            .server
            .session_mut(session)
            .and_then(|session| session.window_mut(window))
            .is_some_and(|window| {
                if window.name == name {
                    false
                } else {
                    window.set_name_manual(name);
                    true
                }
            });
        if changed {
            self.render_global_views();
        }
        changed
    }

    /// Focus the exact pane represented by an agent row, then apply the active
    /// window's shared seen lifecycle. Keeping this atomic prevents a stale or
    /// partial target from focusing only the containing window.
    fn focus_agent_pane(
        &mut self,
        session: SessionId,
        window: lumux_core::model::WindowId,
        pane: PaneId,
    ) -> bool {
        self.coordinate_focus(session, |daemon| {
            daemon.server.session_mut(session).is_some_and(|session| {
                // Validate the complete target before mutating either focus field.
                // Sidebar rows can age between render and click when another client
                // restructures a window; a stale pane must be a total no-op.
                if !session
                    .window(window)
                    .is_some_and(|window| window.pane_ids().contains(&pane))
                {
                    return false;
                }
                session.focus_window(window)
                    && session
                        .window_mut(window)
                        .is_some_and(|window| window.focus_pane(pane))
            })
        })
    }

    /// Focus a validated window on behalf of an interactive client. Copy state
    /// is pane-owned, so navigation away from its window must retire it before
    /// the shared focus mutation and before trailing bytes in the input batch
    /// are decoded by the keymap.
    fn focus_client_window(
        &mut self,
        client_id: u64,
        session: SessionId,
        window: lumux_core::model::WindowId,
    ) -> bool {
        if self
            .daemon
            .server
            .session(session)
            .and_then(|session| session.window(window))
            .is_none()
        {
            return false;
        }
        self.daemon
            .exit_copy_mode_if_focus_changes(client_id, session, window, None);
        self.focus_window(session, window)
    }

    /// Focus a validated pane on behalf of an interactive client while keeping
    /// the copy-mode target invariant atomic with navigation.
    fn focus_client_pane(
        &mut self,
        client_id: u64,
        session: SessionId,
        window: lumux_core::model::WindowId,
        pane: PaneId,
    ) -> bool {
        if self.daemon.server.session(session).is_none_or(|session| {
            session
                .window(window)
                .is_none_or(|window| !window.pane_ids().contains(&pane))
        }) {
            return false;
        }
        self.daemon
            .exit_copy_mode_if_focus_changes(client_id, session, window, Some(pane));
        self.focus_agent_pane(session, window, pane)
    }

    /// Act on a sidebar row click: switch the client to the picked session, and
    /// for an agent row focus its exact pane and acknowledge an unseen
    /// completion. The target is produced by the same layout used to render.
    fn apply_sidebar_pick(&mut self, client_id: u64, pick: crate::daemon::SidebarPick) {
        use crate::daemon::SidebarPick;
        let current_session = self.clients.get(&client_id).map(|client| client.session);
        let (sid, changes_target) = match pick {
            SidebarPick::Session(session) => {
                // A cached frame may outlive a session. Validate before clearing
                // copy state or mutating client routing; stale targets fail closed.
                if self.daemon.server.session(session).is_none() {
                    return;
                }
                (session, current_session != Some(session))
            }
            SidebarPick::Agent {
                session,
                window,
                pane,
            } => {
                // The row may have gone stale after another client restructured
                // its window. Honor focus_client_pane's all-or-nothing contract:
                // an invalid pane must not partially switch sessions or dismiss
                // this client's copy mode.
                if !self.focus_client_pane(client_id, session, window, pane) {
                    return;
                }
                // focus_client_pane already retired target-bound copy state.
                (session, false)
            }
        };
        if changes_target {
            self.daemon.exit_copy_mode(client_id);
        }
        self.switch_client_session(client_id, sid);
        // Focus affects every client on the target session; acknowledgement is
        // visible in every sidebar, including clients attached elsewhere.
        self.render_global_views();
    }

    /// The lowest numeric name ("0", "1", …) not already taken by a session,
    /// mirroring tmux's auto-naming for `new-session` with no `-s`.
    fn next_free_session_name(&self) -> String {
        let mut n = 0u32;
        loop {
            let candidate = n.to_string();
            if self
                .daemon
                .server
                .find_session_by_name(&candidate)
                .is_none()
            {
                return candidate;
            }
            n += 1;
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
            Action::PrevLayout => self.prev_layout(session),
            Action::RenameWindow => {
                self.daemon
                    .open_prompt(client_id, session, crate::daemon::PromptTarget::Window);
            }
            Action::RenameSession => {
                self.daemon
                    .open_prompt(client_id, session, crate::daemon::PromptTarget::Session);
            }
            Action::FindWindow => {
                self.daemon.open_prompt(
                    client_id,
                    session,
                    crate::daemon::PromptTarget::FindWindow,
                );
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
            Action::MarkPane => {
                let marked = self.daemon.toggle_marked_pane(session);
                self.daemon.flash_message(
                    client_id,
                    if marked {
                        "pane marked"
                    } else {
                        "mark cleared"
                    },
                );
                self.invalidate_session(session);
            }
            Action::RotateWindow => self.do_rotate_window(session, true),
            Action::ClockMode => self.daemon.toggle_clock(client_id),
            // A bound command chain (tmux `bind key cmd1 \; cmd2`): run each
            // parsed command through the same executor as the `:` prompt.
            Action::RunCommands(cmds) => {
                let mut routed_session = session;
                for cmd in cmds {
                    self.dispatch_parsed(client_id, routed_session, cmd);
                    // A command in the chain may switch this client. Route every
                    // following command through its newly selected session,
                    // matching command-prompt chains and batched input routing.
                    if let Some(client) = self.clients.get(&client_id) {
                        routed_session = client.session;
                    }
                }
            }
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
        frame_epoch: Option<u64>,
    ) -> Vec<u8> {
        use lumux_core::mouse::{self, MouseButton, MouseKind};
        // Prepend any mouse-report prefix held back from the previous frame (an
        // SGR report split across reads, e.g. over SSH). Taken out of the handle
        // so we don't borrow self.clients across the &mut self calls below.
        let mut input: Vec<u8> = Vec::new();
        let mut inherited_len = 0;
        let mut inherited_epoch = None;
        if let Some(h) = self.clients.get_mut(&client_id) {
            if !h.pending_mouse.is_empty() {
                inherited_len = h.pending_mouse.len();
                inherited_epoch = h.pending_mouse_epoch.take();
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
        let mut pending_epoch = None;
        let mut i = 0;
        while i < combined.len() {
            if let Some((ev, used)) = mouse::parse(&combined[i..]) {
                let event_epoch = if i < inherited_len {
                    inherited_epoch
                } else {
                    frame_epoch
                };
                let input_frame = self.input_frame(client_id, event_epoch);
                // A preceding mouse report in this same socket frame may have
                // selected a sidebar session. Resolve routing per event so a
                // following wheel/drag never acts on the stale source session.
                let event_session = match &input_frame {
                    InputFrame::Live => self
                        .clients
                        .get(&client_id)
                        .map(|client| client.session)
                        .unwrap_or(session),
                    InputFrame::Applied(frame) => frame.interactions.session(),
                    InputFrame::Missing => session,
                };
                // Full-screen overlays replace the sidebar and pane plane. A
                // mouse report over those coordinates must be consumed by the
                // modal surface, never routed to invisible controls behind it.
                let frame_is_modal_or_unknown = match &input_frame {
                    InputFrame::Live => self.daemon.full_screen_overlay_active(client_id),
                    InputFrame::Missing => true,
                    InputFrame::Applied(frame) => {
                        frame.interactions.is_modal()
                            || self
                                .daemon
                                .server
                                .session(frame.interactions.session())
                                .is_none()
                    }
                };
                if frame_is_modal_or_unknown {
                    if matches!(ev.kind, MouseKind::Down(_) | MouseKind::Up(_)) {
                        self.daemon.cancel_mouse_gestures(client_id);
                    }
                    i += used;
                    continue;
                }
                let applied_session_mismatch = match &input_frame {
                    InputFrame::Applied(frame) => self
                        .clients
                        .get(&client_id)
                        .is_none_or(|client| client.session != frame.interactions.session()),
                    InputFrame::Live | InputFrame::Missing => false,
                };
                if applied_session_mismatch {
                    if matches!(ev.kind, MouseKind::Down(_) | MouseKind::Up(_)) {
                        self.daemon.cancel_mouse_gestures(client_id);
                    }
                    // Sidebar rows are global navigation targets and remain the
                    // only safe action on a retained frame from the previously
                    // routed session. All pane/status/gesture actions fail closed.
                    if matches!(ev.kind, MouseKind::Down(_))
                        && matches!(
                            &input_frame,
                            InputFrame::Applied(frame)
                                if frame.interactions.sidebar_click(ev.col, ev.row).is_some()
                        )
                    {
                        self.mouse_select_pane(
                            client_id,
                            event_session,
                            ev.col,
                            ev.row,
                            &input_frame,
                        );
                    }
                    i += used;
                    continue;
                }
                // A mouse *press* selects the pane under the pointer first, even
                // when that pane is a mouse-aware app — otherwise clicking into a
                // pane running Claude Code / vim / htop forwards the click but
                // never switches lumux's focus to it (tmux selects on press too).
                // It also hit-tests the status bar (window-list clicks) and arms a
                // possible divider drag. Scroll/drag/motion do NOT change focus, so
                // hover-to-scroll over an unfocused pane keeps working.
                if matches!(ev.kind, MouseKind::Down(_)) {
                    self.daemon.cancel_mouse_gestures(client_id);
                    if self.mouse_select_pane(
                        client_id,
                        event_session,
                        ev.col,
                        ev.row,
                        &input_frame,
                    ) {
                        i += used;
                        continue;
                    }
                    match &input_frame {
                        InputFrame::Live => {
                            self.daemon
                                .begin_drag(client_id, event_session, ev.col, ev.row);
                        }
                        InputFrame::Applied(frame) => self.daemon.begin_drag_in_frame(
                            client_id,
                            frame.interactions.divider_at(ev.col, ev.row),
                        ),
                        InputFrame::Missing => {}
                    }
                    // A left-press that didn't grab a divider arms a text
                    // selection; the first drag motion turns it into a copy-mode
                    // selection (tmux drag-to-copy). A press on a divider resizes
                    // instead, so don't arm there.
                    if matches!(ev.kind, MouseKind::Down(MouseButton::Left))
                        && !self.daemon.is_dragging_divider(client_id)
                    {
                        match &input_frame {
                            InputFrame::Live => {
                                self.daemon
                                    .mouse_sel_arm(client_id, event_session, ev.col, ev.row)
                            }
                            InputFrame::Applied(frame) => self.daemon.mouse_sel_arm_in_frame(
                                client_id,
                                &frame.interactions,
                                ev.col,
                                ev.row,
                            ),
                            InputFrame::Missing => {}
                        }
                    }
                }
                // If the app in the pane under the pointer enabled mouse
                // reporting, forward the raw event to it (pane-relative) and skip
                // lumux's own handling — so the wheel/clicks work inside vim,
                // htop, Claude Code, etc. (matches tmux). BUT once a divider or
                // text selection is grabbed, the whole gesture (drag + release)
                // belongs to lumux: forwarding it would hand only the trailing
                // events to whatever mouse-aware app the pointer wanders over.
                // While either server gesture owns the pointer, never forward.
                if !self.daemon.is_dragging_divider(client_id)
                    && !self.daemon.mouse_sel_pending(client_id)
                    && self.try_forward_mouse_to_app(event_session, &ev, &input_frame)
                {
                    i += used;
                    continue;
                }
                match ev.kind {
                    // The press already selected the pane + armed a divider drag
                    // above; nothing more to do for a non-mouse-aware pane.
                    MouseKind::Down(_) => {}
                    MouseKind::ScrollUp => self.mouse_scroll(
                        client_id,
                        event_session,
                        ev.col,
                        ev.row,
                        true,
                        &input_frame,
                    ),
                    MouseKind::ScrollDown => self.mouse_scroll(
                        client_id,
                        event_session,
                        ev.col,
                        ev.row,
                        false,
                        &input_frame,
                    ),
                    MouseKind::Drag(_) => {
                        // A live/armed text selection takes the drag (extending
                        // the copy-mode selection under the pointer). Otherwise
                        // fall back to moving a grabbed divider.
                        let selecting = match &input_frame {
                            InputFrame::Live => {
                                self.daemon
                                    .mouse_sel_drag(client_id, event_session, ev.col, ev.row)
                            }
                            InputFrame::Applied(frame) => self.daemon.mouse_sel_drag_in_frame(
                                client_id,
                                &frame.interactions,
                                ev.col,
                                ev.row,
                            ),
                            InputFrame::Missing => false,
                        };
                        if selecting {
                            self.render_client(client_id);
                        } else {
                            self.mouse_drag(client_id, event_session, ev.col, ev.row);
                        }
                    }
                    MouseKind::Up(_) => {
                        // Releasing a text-selection drag yanks it (copy + exit
                        // copy-mode) and emits OSC-52 so the client's local
                        // terminal clipboard is set too — same as keyboard Yank.
                        if self.daemon.mouse_sel_active(client_id) {
                            let text = self.daemon.mouse_sel_finish(client_id);
                            if let Some(k) = self.daemon.keymap_mut(client_id) {
                                k.reset();
                            }
                            if let (Some(text), Some(h)) = (text, self.clients.get(&client_id)) {
                                let _ = h.out.send(ServerMsg::Frame(osc52(&text)));
                            }
                            self.render_client(client_id);
                        } else {
                            // A click without motion leaves MouseSel::Armed;
                            // release must retire that ownership as well as a
                            // possible divider grab. Otherwise a later drag or
                            // wheel can be captured as a continuation of a
                            // gesture that physically ended here.
                            self.daemon.cancel_mouse_gestures(client_id);
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
                pending_epoch = if i < inherited_len {
                    inherited_epoch
                } else {
                    frame_epoch
                };
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
                pending_epoch = if i < inherited_len {
                    inherited_epoch
                } else {
                    frame_epoch
                };
                break;
            } else {
                rest.push(combined[i]);
                i += 1;
            }
        }
        if let Some(h) = self.clients.get_mut(&client_id) {
            h.pending_mouse = pending;
            h.pending_mouse_epoch = pending_epoch;
        }
        rest
    }

    fn input_frame(&self, client_id: u64, epoch: Option<u64>) -> InputFrame {
        let Some(epoch) = epoch else {
            return InputFrame::Live;
        };
        self.clients
            .get(&client_id)
            .and_then(|client| {
                client
                    .frame_history
                    .iter()
                    .find(|frame| frame.epoch == epoch)
            })
            .cloned()
            .map(Box::new)
            .map(InputFrame::Applied)
            .unwrap_or(InputFrame::Missing)
    }

    /// Click: focus the pane under the cursor, switch windows when the click
    /// lands on a window entry in the status bar's bottom row, or act on a
    /// sidebar row (switch session / jump to an agent's window). Returns true
    /// when session chrome consumed the press and it must not reach a pane.
    fn mouse_select_pane(
        &mut self,
        client_id: u64,
        session: SessionId,
        col: u16,
        row: u16,
        input_frame: &InputFrame,
    ) -> bool {
        match input_frame {
            InputFrame::Missing => return true,
            InputFrame::Applied(frame) => {
                let interactions = &frame.interactions;
                if let Some(click) = interactions.sidebar_click(col, row) {
                    match click {
                        SidebarClick::Toggle { collapsed } => {
                            self.set_session_sidebar_collapsed(interactions.session(), collapsed);
                        }
                        SidebarClick::Pick(pick) => {
                            self.apply_sidebar_pick(client_id, pick);
                        }
                        SidebarClick::Chrome => {}
                    }
                    return true;
                }
                if interactions.on_status_row(row) {
                    if let Some(window) = interactions.status_window_at(col, row) {
                        self.focus_client_window(client_id, interactions.session(), window);
                    }
                    return true;
                }
                if let Some(target) = interactions.pane_at(col, row) {
                    self.focus_client_pane(
                        client_id,
                        target.session(),
                        target.window(),
                        target.pane(),
                    );
                }
                return false;
            }
            InputFrame::Live => {}
        }
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
                self.focus_client_window(client_id, session, wid);
            }
            return true;
        }
        let Some(viewport) = self.daemon.content_viewport(session) else {
            return false;
        };
        // A click in the sidebar columns (left of the content origin) either
        // hits the collapse/expand toggle button or selects the session/agent
        // row it lands on.
        if col < viewport.x {
            if self.daemon.sidebar_toggle_hit(session, col, row) {
                let now = self.daemon.sidebar_collapsed(session);
                self.set_session_sidebar_collapsed(session, !now);
            } else if let Some(pick) =
                self.daemon
                    .sidebar_pick_at(client_id, session, row as usize, size.rows as usize)
            {
                self.apply_sidebar_pick(client_id, pick);
            }
            return true;
        }
        let pane = self.daemon.pane_at_screen(session, col, row);
        if let Some(pane) = pane {
            if let Some(window) = self
                .daemon
                .server
                .session(session)
                .map(|session| session.active_window())
            {
                self.focus_client_pane(client_id, session, window, pane);
            }
        }
        false
    }

    /// Focus the pane whose rectangle contains (col,row), if any. Returns true if
    /// a pane was focused. Used by scroll to target the pane under the pointer.
    fn focus_pane_at(&mut self, session: SessionId, col: u16, row: u16) -> bool {
        let Some(pid) = self.daemon.pane_at_screen(session, col, row) else {
            return false;
        };
        self.focus_pane(session, pid)
    }

    /// Wheel: scroll the pane under the pointer. Two cases, matching tmux:
    /// - A pane on the *alternate screen* (vim/less, or a TUI agent like Claude
    ///   Code) owns the viewport and has no scrollback, so the wheel is
    ///   translated into arrow-key input sent to that app, which scrolls itself.
    /// - Otherwise, enter copy-mode on that pane and scroll its history.
    ///
    /// While already in copy-mode the current pane keeps scrolling, so an
    /// in-progress selection isn't hijacked.
    fn mouse_scroll(
        &mut self,
        client_id: u64,
        session: SessionId,
        col: u16,
        row: u16,
        up: bool,
        input_frame: &InputFrame,
    ) {
        use lumux_core::keymap::CopyKey;

        if matches!(input_frame, InputFrame::Missing) {
            return;
        }
        let applied_sidebar = match input_frame {
            InputFrame::Applied(frame) => frame.interactions.sidebar(),
            InputFrame::Live | InputFrame::Missing => None,
        };
        let live_sidebar = matches!(input_frame, InputFrame::Live)
            && self
                .daemon
                .content_viewport(session)
                .is_some_and(|viewport| col < viewport.x);
        if live_sidebar || applied_sidebar.is_some_and(|sidebar| sidebar.contains(col, row)) {
            let height = match input_frame {
                InputFrame::Applied(frame) => frame.interactions.size().rows as usize,
                InputFrame::Live | InputFrame::Missing => self
                    .daemon
                    .server
                    .effective_size(session)
                    .map(|size| size.rows as usize)
                    .unwrap_or(24),
            };
            let changed = if let Some(sidebar) = applied_sidebar {
                sidebar.section_at(col, row).is_some_and(|section| {
                    self.daemon
                        .scroll_sidebar_section(client_id, session, section, height, up)
                })
            } else {
                self.daemon
                    .scroll_sidebar(client_id, session, row, height, up)
            };
            if changed {
                self.render_session(session);
            }
            return;
        }

        if let InputFrame::Applied(frame) = input_frame {
            let interactions = &frame.interactions;
            let live_copy_mode = self.daemon.in_copy_mode(client_id);
            if live_copy_mode != interactions.is_copy_mode() {
                return;
            }
            if interactions.is_copy_mode()
                && interactions
                    .copy_pane()
                    .is_none_or(|target| !self.daemon.copy_target_matches(client_id, target))
            {
                return;
            }
            if !interactions.is_copy_mode() {
                let Some(target) = interactions.pane_at(col, row) else {
                    return;
                };
                if !self.daemon.interaction_pane_modes_match(target) {
                    return;
                }
                if target.alt_screen() {
                    let arrow: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
                    for _ in 0..3 {
                        let _ = self.daemon.write_pane(target.pane(), arrow);
                    }
                    return;
                }
                if !up {
                    return;
                }
                if !self.focus_agent_pane(target.session(), target.window(), target.pane()) {
                    return;
                }
                self.daemon
                    .enter_copy_mode(client_id, interactions.session());
            }
            let key = if up { CopyKey::Up } else { CopyKey::Down };
            for _ in 0..3 {
                self.daemon
                    .copy_navigate(client_id, interactions.session(), key);
            }
            return;
        }

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
        self.daemon.pane_at_screen(session, col, row)
    }

    /// The pane and its rectangle containing (col,row) — used to translate a
    /// mouse event into pane-relative coordinates before forwarding to the app.
    fn pane_and_rect_at_point(
        &self,
        session: SessionId,
        col: u16,
        row: u16,
    ) -> Option<(PaneId, lumux_core::layout::Rect)> {
        self.daemon.pane_and_rect_at_screen(session, col, row)
    }

    /// If the pane under the pointer has mouse reporting on, forward the raw event
    /// to that app re-encoded with pane-relative coordinates and return true (so
    /// the caller skips lumux's own scroll/copy/select handling). tmux behavior:
    /// a mouse-aware TUI (vim, htop, Claude Code) handles the wheel/clicks itself.
    fn try_forward_mouse_to_app(
        &mut self,
        session: SessionId,
        ev: &lumux_core::mouse::MouseEvent,
        input_frame: &InputFrame,
    ) -> bool {
        let (pid, rect, wants_mouse) = match input_frame {
            InputFrame::Missing => return false,
            InputFrame::Applied(frame) => {
                if frame.interactions.is_copy_mode() {
                    return false;
                }
                let Some(target) = frame.interactions.pane_at(ev.col, ev.row) else {
                    return false;
                };
                if !self.daemon.interaction_pane_modes_match(target) {
                    return false;
                }
                (target.pane(), target.rect(), target.wants_mouse())
            }
            InputFrame::Live => {
                let Some((pane, rect)) = self.pane_and_rect_at_point(session, ev.col, ev.row)
                else {
                    return false;
                };
                (pane, rect, self.daemon.pane_wants_mouse(pane))
            }
        };
        if !wants_mouse {
            return false;
        }
        // Translate screen coords to pane-relative (clamped into the rect).
        let rel_col = ev
            .col
            .saturating_sub(rect.x)
            .min(rect.cols.saturating_sub(1));
        let rel_row = ev
            .row
            .saturating_sub(rect.y)
            .min(rect.rows.saturating_sub(1));
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
        let window = self.daemon.server.session(session).and_then(|s| {
            let ids = s.window_ids();
            ids.get(pos).copied()
        });
        if let Some(window) = window {
            self.focus_window(session, window);
        }
    }

    /// Move focus geographically within the active window.
    fn select_pane(&mut self, session: SessionId, dir: Direction) {
        // Content area excludes the sidebar columns and the status bar row.
        let Some(viewport) = self.daemon.content_viewport(session) else {
            return;
        };
        self.coordinate_focus(session, |daemon| {
            daemon.server.session_mut(session).is_some_and(|session| {
                let window = session.active_window();
                session.window_mut(window).is_some_and(|window| {
                    window.focus_direction(dir, viewport);
                    true
                })
            })
        });
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

    /// `resize-pane -L/-R/-U/-D [N]`: resize by a specific cell count instead of
    /// the fixed interactive step. Converts N cells to a layout ratio delta using
    /// the window's total content span along that axis (`effective_size`) as the
    /// denominator — NOT the active pane's own span, which would make the same N
    /// produce wildly different deltas depending on which side is active and how
    /// large it currently is. Exact for the common single-split 2-pane case; an
    /// approximation for deeper nested splits, since the model only exposes a
    /// ratio-based resize, not an absolute-cell one. No `N` (or an unknown size)
    /// falls back to the same fixed step the interactive Ctrl/Alt-arrow bindings
    /// use.
    fn do_resize_pane_amount(
        &mut self,
        session: SessionId,
        dir: lumux_core::layout::Direction,
        cells: Option<u16>,
    ) {
        use lumux_core::layout::Direction;
        let (axis, sign): (SplitDir, f32) = match dir {
            Direction::Left => (SplitDir::Horizontal, -1.0),
            Direction::Right => (SplitDir::Horizontal, 1.0),
            Direction::Up => (SplitDir::Vertical, -1.0),
            Direction::Down => (SplitDir::Vertical, 1.0),
        };
        let step = match cells {
            Some(n) => {
                // Convert N cells to a ratio against the pane content span (the
                // sidebar's columns aren't part of it), so the divider moves ~N.
                let total = self.daemon.content_viewport(session).map(|vp| {
                    if axis == SplitDir::Horizontal {
                        vp.cols
                    } else {
                        vp.rows
                    }
                });
                match total {
                    Some(total) if total > 0 => (n as f32 / total as f32) * sign,
                    _ => RESIZE_STEP * sign,
                }
            }
            None => RESIZE_STEP * sign,
        };
        self.resize_pane(session, axis, step);
    }

    /// Apply a runtime `:set OPTION VALUE`. Most options just mutate the config
    /// and re-apply live (keymaps rebuild for prefix/mode-keys; the next render
    /// picks up colors/formats/base-index). Two options need extra work: mouse
    /// reporting must be turned on/off in every client's terminal here (the
    /// config flag only gates lumux's own interception), and `synchronize-panes`
    /// is per-session runtime state rather than a config field, so it routes to
    /// the existing toggle instead of Config::set_option.
    fn do_set_option(&mut self, client_id: u64, option: &str, value: &str) {
        // synchronize-panes is a tmux *window* option, but lumux models it as
        // per-session runtime state (not part of Config). Handle it before
        // touching the config so `:set synchronize-panes on/off` works like the
        // command and the `S` binding.
        if option == "synchronize-panes" || option == "synchronize-pane" {
            let session = self.clients.get(&client_id).map(|h| h.session);
            if let Some(session) = session {
                let on_now = self.daemon.is_synchronized(session);
                let want = !matches!(value, "off" | "0" | "false" | "no");
                if want != on_now {
                    self.daemon.toggle_sync(client_id, session);
                }
                self.daemon.flash_message(
                    client_id,
                    format!("synchronize-panes {}", if want { "on" } else { "off" }),
                );
            }
            return;
        }
        // The sidebar's *visibility* is per-session runtime state (session-global
        // under the shared PTY), so route `:set sidebar on|off` to the toggle and
        // reflow the content grid. `sidebar-width` is a config field and falls
        // through to the shared set_option path below (its effect applies on the
        // next visibility change / resize).
        if option == "sidebar" {
            if let Some(session) = self.clients.get(&client_id).map(|h| h.session) {
                let want = !matches!(value, "off" | "0" | "false" | "no");
                self.set_session_sidebar(session, want);
                self.daemon.flash_message(
                    client_id,
                    format!("sidebar {}", if want { "on" } else { "off" }),
                );
            }
            return;
        }
        let was_mouse = self.daemon.mouse_enabled();
        match self.daemon.set_option(option, value) {
            Ok(()) => {
                // If mouse reporting just flipped, tell every client's terminal to
                // start/stop sending SGR mouse reports — the config flag alone
                // only decides whether the daemon *intercepts* them.
                let now_mouse = self.daemon.mouse_enabled();
                if now_mouse != was_mouse {
                    let seq = if now_mouse {
                        lumux_core::mouse::ENABLE
                    } else {
                        lumux_core::mouse::DISABLE
                    };
                    let ids: Vec<u64> = self.clients.keys().copied().collect();
                    for id in ids {
                        if let Some(h) = self.clients.get(&id) {
                            let _ = h.out.send(ServerMsg::Frame(seq.as_bytes().to_vec()));
                        }
                    }
                }
                if option == "sidebar-width" {
                    self.reflow_all_sessions();
                }
                self.daemon
                    .flash_message(client_id, format!("set {option} {value}"));
            }
            Err(msg) => self.daemon.flash_message(client_id, msg),
        }
    }

    /// Toggle zoom on the active pane (tmux prefix z) and re-fit PTYs, since the
    /// zoomed pane now fills the whole content area (or returns to its split).
    fn zoom_pane(&mut self, session: SessionId) {
        let toggled = self
            .coordinate_topology(session, |daemon| {
                daemon.server.session_mut(session).and_then(|session| {
                    let window = session.active_window();
                    session
                        .window_mut(window)
                        .map(|window| window.toggle_zoom())
                })
            })
            .is_some();
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
            .coordinate_topology(session, |daemon| {
                daemon.server.session_mut(session).and_then(|session| {
                    let window = session.active_window();
                    session
                        .window_mut(window)
                        .map(|window| window.next_layout())
                })
            })
            .is_some();
        if applied {
            if let Some(size) = self.daemon.server.effective_size(session) {
                self.daemon.resize_session(session, size);
            }
        }
    }

    /// Cycle the active window to the previous preset layout (tmux
    /// `previous-layout`); mirrors [`Self::next_layout`].
    fn prev_layout(&mut self, session: SessionId) {
        let applied = self
            .coordinate_topology(session, |daemon| {
                daemon.server.session_mut(session).and_then(|session| {
                    let window = session.active_window();
                    session
                        .window_mut(window)
                        .map(|window| window.prev_layout())
                })
            })
            .is_some();
        if applied {
            if let Some(size) = self.daemon.server.effective_size(session) {
                self.daemon.resize_session(session, size);
            }
        }
    }

    /// Apply a specific named preset layout to the active window (tmux
    /// `select-layout <name>`) and re-fit the PTYs. Unlike [`Self::next_layout`],
    /// which cycles, this sets the exact layout the user asked for.
    fn apply_named_layout(&mut self, session: SessionId, kind: lumux_core::model::LayoutKind) {
        let applied = self
            .coordinate_topology(session, |daemon| {
                daemon.server.session_mut(session).and_then(|session| {
                    let window = session.active_window();
                    session
                        .window_mut(window)
                        .map(|window| window.apply_layout(kind))
                })
            })
            .is_some();
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
        let found = crate::config_candidates().into_iter().find(|p| p.exists());
        let mut geometry_may_have_changed = false;
        let msg = match found {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) => match crate::parse_config(&path, &text) {
                    Ok(cfg) => {
                        self.daemon.set_config(cfg);
                        geometry_may_have_changed = true;
                        "lumux configuration reloaded".to_string()
                    }
                    Err(e) => format!("config error: {e}"),
                },
                Err(_) => format!("no config at {}", path.display()),
            },
            None => "no config file found".to_string(),
        };
        self.daemon.flash_message(client_id, msg);
        if geometry_may_have_changed {
            self.reflow_all_sessions();
        } else {
            self.render_session(session);
        }
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
                // The chooser returns navigation intent only. Route it through
                // the same focus + client-switch seams as mouse and commands so
                // acknowledgement and geometry cannot be bypassed.
                let pick = self.daemon.chooser_confirm(client_id)?;
                let sid = match pick {
                    crate::daemon::ChooserPick::Session(s) => s,
                    crate::daemon::ChooserPick::Window(s, window) => {
                        self.focus_window(s, window);
                        s
                    }
                };
                self.switch_client_session(client_id, sid);
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
            BufferKey::Index(n) => self
                .daemon
                .buffer_chooser_move(client_id, 0, Some(n as usize)),
            BufferKey::Delete => self.daemon.buffer_chooser_delete(client_id),
            BufferKey::Cancel => self.daemon.buffer_chooser_cancel(client_id),
            BufferKey::Confirm => self.daemon.buffer_chooser_confirm(client_id, session),
        }
    }

    /// Drive an open prompt: edit it, or route its confirmed intent through the
    /// same focus/command seams used by every other navigation surface.
    fn handle_prompt_key(&mut self, client_id: u64, session: SessionId, pk: PromptKey) {
        let mut outcome = None;
        match pk {
            PromptKey::Char(c) => self.daemon.prompt_push(client_id, c),
            PromptKey::Backspace => self.daemon.prompt_backspace(client_id),
            PromptKey::Cancel => self.daemon.prompt_cancel(client_id),
            PromptKey::Confirm => outcome = self.daemon.prompt_confirm(client_id, session),
        }
        // Reset the keymap out of Prompt mode once the prompt closes.
        if matches!(pk, PromptKey::Confirm | PromptKey::Cancel) {
            if let Some(k) = self.daemon.keymap_mut(client_id) {
                k.reset();
            }
        }
        match outcome {
            Some(crate::daemon::PromptOutcome::FocusWindow(window)) => {
                self.focus_window(session, window);
            }
            Some(crate::daemon::PromptOutcome::Command(line)) => {
                self.dispatch_command_line(client_id, session, &line);
            }
            Some(crate::daemon::PromptOutcome::RenameSession(name)) => {
                self.rename_session(session, name);
            }
            Some(crate::daemon::PromptOutcome::RenameWindow { window, name }) => {
                self.rename_window(session, window, name);
            }
            None => {}
        }
    }

    /// Parse and run a tmux command-prompt line (prefix `:`). Reuses the same
    /// action paths as the keybindings so behavior is identical.
    fn dispatch_command_line(&mut self, client_id: u64, session: SessionId, line: &str) {
        use lumux_core::command::parse_commands;
        // A command line may chain several commands with `;` (tmux separator).
        // Execute each in order; render once at the end.
        let mut session = session;
        for cmd in parse_commands(line) {
            self.dispatch_parsed(client_id, session, cmd);
            // A command may have switched the client onto a different session
            // (switch-client / new-session without -d); re-read it so any
            // remaining chained commands, and the final render below, target
            // the session the client is actually on now.
            if let Some(h) = self.clients.get(&client_id) {
                session = h.session;
            }
        }
        self.render_session(session);
    }

    /// Execute a single parsed command-prompt command. Split out of
    /// [`Self::dispatch_command_line`] so a `;`-chained line runs each segment
    /// through the same logic. Rendering is done once by the caller.
    fn dispatch_parsed(
        &mut self,
        client_id: u64,
        session: SessionId,
        cmd: lumux_core::command::ParsedCommand,
    ) {
        use lumux_core::command::{Dir, ParsedCommand};
        let dir_to_split = |d: Dir| match d {
            Dir::Horizontal => SplitDir::Horizontal,
            Dir::Vertical => SplitDir::Vertical,
        };
        match cmd {
            ParsedCommand::SplitWindow(d) => self.do_split(session, dir_to_split(d)),
            ParsedCommand::NewWindow => self.do_new_window(session),
            ParsedCommand::KillPane(target) => self.do_kill_pane_target(client_id, session, target),
            ParsedCommand::KillWindow => self.do_kill_window(session),
            ParsedCommand::NextWindow => self.do_next_window(session),
            ParsedCommand::PrevWindow => self.do_prev_window(session),
            ParsedCommand::LastWindow => self.do_last_window(session),
            ParsedCommand::LastPane => self.do_last_pane(session),
            ParsedCommand::SelectWindow(n) => self.select_window_by_number(session, n),
            ParsedCommand::RenameWindow(name) => {
                if let Some(window) = self
                    .daemon
                    .server
                    .session(session)
                    .map(|session| session.active_window())
                {
                    self.rename_window(session, window, name);
                }
            }
            ParsedCommand::RenameSession(name) => {
                self.rename_session(session, name);
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
                        self.focus_window(session, wid);
                    }
                    None => self
                        .daemon
                        .flash_message(client_id, format!("no window matching \"{q}\"")),
                }
            }
            ParsedCommand::BreakPane => self.do_break_pane(session),
            ParsedCommand::RotateWindow { down } => self.do_rotate_window(session, down),
            ParsedCommand::SwapWindow { src, dst } => {
                self.do_swap_window(client_id, session, src, dst)
            }
            ParsedCommand::MoveWindow { dst } => self.do_move_window_to(client_id, session, dst),
            ParsedCommand::SwapPane { next, target } => {
                self.do_swap_pane_target(client_id, session, next, target)
            }
            ParsedCommand::JoinPane { dir, src } => {
                self.do_join_pane(client_id, session, dir_to_split(dir), src)
            }
            ParsedCommand::SynchronizePanes(state) => {
                let on_now = self.daemon.is_synchronized(session);
                let want = state.unwrap_or(!on_now);
                if want != on_now {
                    self.daemon.toggle_sync(client_id, session);
                }
            }
            ParsedCommand::DisplayPanes => self.daemon.show_pane_numbers(client_id),
            ParsedCommand::CapturePane => match self.daemon.capture_pane(session) {
                Some(name) => self
                    .daemon
                    .flash_message(client_id, format!("captured to {name}")),
                None => self.daemon.flash_message(client_id, "nothing to capture"),
            },
            ParsedCommand::RespawnPane => self.do_respawn_pane(client_id, session),
            ParsedCommand::RunShell(cmd) => {
                let status = self.daemon.run_shell(&cmd);
                self.daemon.flash_message(client_id, status);
            }
            ParsedCommand::DisplayMessage(text) => {
                self.daemon.flash_message(client_id, text);
            }
            ParsedCommand::SendKeys(bytes) => {
                // Bytes were already resolved from key names / literal text at
                // parse time; inject them into the active pane.
                self.daemon.write_input(session, &bytes);
            }
            ParsedCommand::SelectLayout(name) => match name {
                // A named preset applies that layout; a bad name flashes an error.
                Some(n) => match lumux_core::model::LayoutKind::from_name(&n) {
                    Some(kind) => self.apply_named_layout(session, kind),
                    None => self
                        .daemon
                        .flash_message(client_id, format!("unknown layout: {n}")),
                },
                // Bare select-layout cycles, like next-layout.
                None => self.next_layout(session),
            },
            ParsedCommand::PreviousLayout => self.prev_layout(session),
            ParsedCommand::SaveState => {
                let path = self.state_path.clone();
                match self.daemon.save_state(&path) {
                    Ok(()) => self.daemon.flash_message(client_id, "state saved"),
                    Err(e) => self
                        .daemon
                        .flash_message(client_id, format!("save failed: {e}")),
                }
            }
            ParsedCommand::SetBuffer { name, text } => {
                self.daemon.set_buffer(name.as_deref(), &text);
            }
            ParsedCommand::PasteNamedBuffer { name } => {
                if !self.daemon.paste_named_buffer(session, name.as_deref()) {
                    self.daemon.flash_message(client_id, "no such buffer");
                }
            }
            ParsedCommand::SaveBuffer { name, path } => {
                match self.daemon.save_buffer(name.as_deref(), &path) {
                    Ok(()) => self
                        .daemon
                        .flash_message(client_id, format!("saved to {path}")),
                    Err(e) => self.daemon.flash_message(client_id, e),
                }
            }
            ParsedCommand::LoadBuffer { path } => match self.daemon.load_buffer(&path) {
                Ok(name) => self
                    .daemon
                    .flash_message(client_id, format!("loaded {name}")),
                Err(e) => self.daemon.flash_message(client_id, e),
            },
            ParsedCommand::DeleteBuffer { name } => {
                if !self.daemon.delete_named_buffer(&name) {
                    self.daemon.flash_message(client_id, "no such buffer");
                }
            }
            ParsedCommand::NewSession { name, detached } => {
                let name = name.unwrap_or_else(|| self.next_free_session_name());
                if self.daemon.server.find_session_by_name(&name).is_some() {
                    self.daemon
                        .flash_message(client_id, format!("duplicate session: {name}"));
                } else {
                    let size = self
                        .daemon
                        .server
                        .effective_size(session)
                        .unwrap_or(PtySize::new(80, 24));
                    match self.spawn_session(name, None, size) {
                        Some(sid) if !detached => self.switch_client_session(client_id, sid),
                        Some(_) => {}
                        None => self
                            .daemon
                            .flash_message(client_id, "new-session: failed to start shell"),
                    }
                }
            }
            ParsedCommand::KillSession { target } => {
                let sid = match target {
                    Some(name) => match self.daemon.server.find_session_by_name(&name) {
                        Some(sid) => sid,
                        None => {
                            self.daemon
                                .flash_message(client_id, format!("no such session: {name}"));
                            return;
                        }
                    },
                    None => session,
                };
                self.kill_whole_session(sid);
            }
            ParsedCommand::KillServer => {
                let ids = self.daemon.server.session_ids();
                for id in ids {
                    self.kill_whole_session(id);
                }
            }
            ParsedCommand::SwitchClient { target } => {
                match self.daemon.server.find_session_by_name(&target) {
                    Some(sid) => self.switch_client_session(client_id, sid),
                    None => self
                        .daemon
                        .flash_message(client_id, format!("no such session: {target}")),
                }
            }
            ParsedCommand::ResizePane { dir, cells } => {
                self.do_resize_pane_amount(session, dir, cells)
            }
            ParsedCommand::SetOption { option, value } => {
                self.do_set_option(client_id, &option, &value)
            }
            ParsedCommand::CopyMode => self.daemon.enter_copy_mode(client_id, session),
            ParsedCommand::ClockMode => self.daemon.toggle_clock(client_id),
            ParsedCommand::ZoomPane => self.zoom_pane(session),
            ParsedCommand::SelectPane { dir, target } => {
                use lumux_core::command::Target;
                match (dir, target) {
                    (Some(dir), _) => self.select_pane(session, dir),
                    // -t .N focuses pane N in the active window (base-index
                    // adjusted, reusing the display-panes picker path).
                    (None, Some(Target::Pane(n))) => {
                        self.pick_numbered_pane(client_id, session, n);
                    }
                    // -t :N (a window target) or no argument: nothing to do here
                    // (tmux -t on a window would move focus there; v1 keeps
                    // select-pane pane-scoped).
                    (None, _) => {}
                }
            }
            ParsedCommand::Detach => {
                if let Some(h) = self.clients.get(&client_id) {
                    let _ = h.out.send(ServerMsg::Detached);
                }
            }
            ParsedCommand::BadArgs(usage) => self.daemon.flash_message(client_id, usage),
            ParsedCommand::Unknown(verb) => {
                self.daemon
                    .flash_message(client_id, format!("unknown command: {verb}"));
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
        if self
            .coordinate_topology(session, |daemon| daemon.server.break_active_pane(session))
            .is_none()
        {
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
        self.render_global_views();
    }

    /// Swap the active pane with its previous (`{`) or next (`}`) sibling in the
    /// same window (tmux swap-pane). Pure layout change — panes keep their grids.
    fn do_swap_pane(&mut self, session: SessionId, next: bool) {
        let Some(other) = self.daemon.server.sibling_pane(session, next) else {
            return; // single pane: nothing to swap with.
        };
        if self
            .coordinate_topology(session, |daemon| {
                daemon.server.swap_active_pane(session, other).then_some(())
            })
            .is_some()
        {
            let size = self
                .daemon
                .server
                .effective_size(session)
                .unwrap_or(PtySize::new(80, 24));
            self.daemon.resize_session(session, size);
            self.invalidate_session(session);
            self.render_global_views();
        }
    }

    /// Join a pane from a source window into the active window (tmux join-pane).
    /// `src` is a window index (base-index offset); None means the previously-
    /// active window. The moved pane keeps its PTY/grid; if the source window
    /// empties it is closed. Re-fits all windows since two changed.
    fn do_join_pane(
        &mut self,
        client_id: u64,
        session: SessionId,
        dir: SplitDir,
        src: Option<u32>,
    ) {
        // With no -s, tmux joins the MARKED pane if one is set (which may live in
        // another window) — that's the pane-marking workflow. Fall back to the
        // last window's active pane when nothing is marked.
        if src.is_none() {
            if let Some((msid, mpid)) = self.daemon.marked_pane() {
                if msid == session {
                    match self.coordinate_topology(session, |daemon| {
                        let joined = daemon.server.join_specific_pane(session, mpid, dir);
                        if joined.is_some() {
                            daemon.clear_mark_if(mpid);
                        }
                        joined
                    }) {
                        Some(_) => {
                            let size = self
                                .daemon
                                .server
                                .effective_size(session)
                                .unwrap_or(PtySize::new(80, 24));
                            self.daemon.resize_all_windows(session, size);
                            self.invalidate_session(session);
                            self.render_global_views();
                            return;
                        }
                        None => { /* marked pane already here / gone: fall through */ }
                    }
                }
            }
        }
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
                None => s.window_ids().into_iter().find(|&w| w != s.active_window()),
            }
        };
        let Some(src_wid) = src_wid else {
            self.daemon
                .flash_message(client_id, "join-pane: no source window");
            return;
        };
        match self.coordinate_topology(session, |daemon| {
            daemon.server.join_pane(session, src_wid, dir)
        }) {
            Some(_) => {
                let size = self
                    .daemon
                    .server
                    .effective_size(session)
                    .unwrap_or(PtySize::new(80, 24));
                self.daemon.resize_all_windows(session, size);
                self.invalidate_session(session);
                self.render_global_views();
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
            self.render_global_views();
        }
    }

    /// Rotate the panes in the active window (tmux `rotate-window`, `C-o`) and
    /// re-fit them to their new slots.
    fn do_rotate_window(&mut self, session: SessionId, down: bool) {
        let rotated = self
            .coordinate_topology(session, |daemon| {
                daemon.server.session_mut(session).and_then(|session| {
                    let window = session.active_window();
                    session
                        .window_mut(window)
                        .and_then(|window| window.rotate_panes(down).then_some(()))
                })
            })
            .is_some();
        if rotated {
            let size = self
                .daemon
                .server
                .effective_size(session)
                .unwrap_or(PtySize::new(80, 24));
            self.daemon.resize_session(session, size);
            self.invalidate_session(session);
            self.render_global_views();
        }
    }

    /// Swap two windows by index (tmux `swap-window -s A -t B`); source defaults
    /// to the active window. Indexes are base-index-adjusted.
    fn do_swap_window(&mut self, client_id: u64, session: SessionId, src: Option<u32>, dst: u32) {
        let base = self.daemon.base_index();
        let swapped = self
            .daemon
            .server
            .session_mut(session)
            .map(|s| {
                let ids = s.window_ids();
                let a = match src {
                    Some(n) => (n.saturating_sub(base)) as usize,
                    None => ids
                        .iter()
                        .position(|&w| w == s.active_window())
                        .unwrap_or(0),
                };
                let b = (dst.saturating_sub(base)) as usize;
                s.swap_windows(a, b)
            })
            .unwrap_or(false);
        if swapped {
            self.invalidate_session(session);
            self.render_global_views();
        } else {
            self.daemon
                .flash_message(client_id, "swap-window: bad index");
        }
    }

    /// Move the active window to an index (tmux `move-window -t N`).
    fn do_move_window_to(&mut self, client_id: u64, session: SessionId, dst: u32) {
        let base = self.daemon.base_index();
        let moved = self
            .daemon
            .server
            .session_mut(session)
            .map(|s| s.move_active_window_to((dst.saturating_sub(base)) as usize))
            .unwrap_or(false);
        if moved {
            self.invalidate_session(session);
            self.render_global_views();
        } else {
            self.daemon
                .flash_message(client_id, "move-window: bad index");
        }
    }

    fn do_split(&mut self, session: SessionId, dir: SplitDir) {
        let size = self
            .daemon
            .server
            .effective_size(session)
            .unwrap_or(PtySize::new(80, 24));
        if let Some((pid, reader)) = self.coordinate_topology(session, |daemon| {
            daemon.split_active(session, dir, size).ok().flatten()
        }) {
            self.pane_session.insert(pid, session);
            spawn_pane_reader(pid, reader, self.tx.clone());
            // Re-fit every pane in the window to its exact layout rect (the new
            // pane was spawned at the content height; the split means both panes
            // are now smaller).
            self.daemon.resize_session(session, size);
            self.fire_hook(session, "after-split-window");
            self.render_global_views();
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
            self.fire_hook(session, "after-new-window");
            self.render_global_views();
        }
    }

    /// Focus the next / previous / last-active window (tmux next/previous/last-
    /// window). Extracted so the keymap and `:` command surfaces share one impl.
    fn do_next_window(&mut self, session: SessionId) {
        self.coordinate_focus(session, |daemon| {
            daemon.server.session_mut(session).is_some_and(|session| {
                session.focus_next_window();
                true
            })
        });
    }

    fn do_prev_window(&mut self, session: SessionId) {
        self.coordinate_focus(session, |daemon| {
            daemon.server.session_mut(session).is_some_and(|session| {
                session.focus_prev_window();
                true
            })
        });
    }

    fn do_last_window(&mut self, session: SessionId) {
        self.coordinate_focus(session, |daemon| {
            daemon.server.session_mut(session).is_some_and(|session| {
                session.focus_last_window();
                true
            })
        });
    }

    /// Focus the previously-active pane in the active window (tmux `last-pane`).
    fn do_last_pane(&mut self, session: SessionId) {
        self.coordinate_focus(session, |daemon| {
            daemon.server.session_mut(session).is_some_and(|session| {
                let window = session.active_window();
                session.window_mut(window).is_some_and(|window| {
                    window.focus_last_pane();
                    true
                })
            })
        });
    }

    /// Kill the active pane and run the pane-exit cascade (tmux `kill-pane`).
    fn do_kill_pane(&mut self, session: SessionId) {
        if let Some(pid) = self.active_pane(session) {
            let result = self.daemon.close_pane(session, pid);
            self.pane_session.remove(&pid);
            self.on_pane_exit(session, pid, result);
        }
    }

    /// `kill-pane [-t TARGET]`: kill the target pane (or the active pane when no
    /// target). A `-t .N` pane index that doesn't resolve flashes an error rather
    /// than silently killing the active pane.
    fn do_kill_pane_target(
        &mut self,
        client_id: u64,
        session: SessionId,
        target: Option<lumux_core::command::Target>,
    ) {
        let pid = match self.resolve_target_pane(session, target) {
            Ok(pid) => pid,
            Err(msg) => return self.daemon.flash_message(client_id, msg),
        };
        let result = self.daemon.close_pane(session, pid);
        self.pane_session.remove(&pid);
        self.on_pane_exit(session, pid, result);
    }

    /// `swap-pane [-U|-D] [-t .N]`: swap the active pane with a sibling
    /// (prev/next) or, with `-t .N`, with pane N in the active window.
    fn do_swap_pane_target(
        &mut self,
        client_id: u64,
        session: SessionId,
        next: bool,
        target: Option<lumux_core::command::Target>,
    ) {
        // No explicit target: swap with the MARKED pane if one is set (tmux's
        // swap-pane default), which may be in another window; otherwise do the
        // sibling swap (prefix {/}).
        let Some(target) = target else {
            if let Some((msid, mpid)) = self.daemon.marked_pane() {
                if msid == session {
                    if let Some(active) = self.active_pane(session) {
                        if self
                            .coordinate_topology(session, |daemon| {
                                daemon
                                    .server
                                    .swap_panes(session, active, mpid)
                                    .then_some(())
                            })
                            .is_some()
                        {
                            let size = self
                                .daemon
                                .server
                                .effective_size(session)
                                .unwrap_or(PtySize::new(80, 24));
                            self.daemon.resize_all_windows(session, size);
                            self.invalidate_session(session);
                            self.render_global_views();
                        }
                    }
                    return;
                }
            }
            return self.do_swap_pane(session, next);
        };
        let other = match self.resolve_target_pane(session, Some(target)) {
            Ok(pid) => pid,
            Err(msg) => return self.daemon.flash_message(client_id, msg),
        };
        // Swap across windows if needed (target may be in another window).
        if let Some(active) = self.active_pane(session) {
            if self
                .coordinate_topology(session, |daemon| {
                    daemon
                        .server
                        .swap_panes(session, active, other)
                        .then_some(())
                })
                .is_some()
            {
                let size = self
                    .daemon
                    .server
                    .effective_size(session)
                    .unwrap_or(PtySize::new(80, 24));
                self.daemon.resize_all_windows(session, size);
                self.invalidate_session(session);
                self.render_global_views();
            }
        }
    }

    /// Resolve a `-t` target to a concrete pane id in `session`. `None` → the
    /// active pane. A window target (`-t N`) resolves to that window's active
    /// pane; a pane target (`-t .N`) to pane N of the active window. Returns an
    /// error message (for the caller to flash) when the index doesn't exist.
    fn resolve_target_pane(
        &self,
        session: SessionId,
        target: Option<lumux_core::command::Target>,
    ) -> Result<PaneId, String> {
        use lumux_core::command::Target;
        let base = self.daemon.base_index();
        let Some(s) = self.daemon.server.session(session) else {
            return Err("no such session".to_string());
        };
        match target {
            None => Ok(s
                .window(s.active_window())
                .map(|w| w.active_pane())
                .unwrap_or_else(|| {
                    // active_window always resolves in a live session; unreachable in
                    // practice, but avoid an unwrap.
                    s.window_ids()
                        .first()
                        .and_then(|&w| s.window(w))
                        .map(|w| w.active_pane())
                        .expect("session has at least one window")
                })),
            Some(Target::Window(n)) => {
                let pos = n.saturating_sub(base) as usize;
                s.window_ids()
                    .get(pos)
                    .and_then(|&wid| s.window(wid))
                    .map(|w| w.active_pane())
                    .ok_or_else(|| format!("no window {n}"))
            }
            Some(Target::Pane(n)) => {
                let pos = n.saturating_sub(base) as usize;
                let w = s.window(s.active_window()).ok_or("no active window")?;
                w.pane_ids()
                    .get(pos)
                    .copied()
                    .ok_or_else(|| format!("no pane {n}"))
            }
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
            // remain-on-exit keeps the pane but clears its agent lifecycle.
            // Agent rows are a global projection, so clients attached to other
            // sessions must receive that removal too.
            None => self.render_global_views(),
        }
        // Historically cleanup was visible before pane-exited hooks ran. Keep
        // that responsiveness when a hook invokes a slow synchronous command,
        // while letting any hook-produced mutation form a second coherent
        // render phase at the outer message flush.
        self.flush_renders();
        // Fire the pane-exited hook if the session still exists.
        if self.daemon.server.session(session).is_some() {
            self.fire_hook(session, "pane-exited");
        }
    }

    /// Run the configured command for hook `event` (tmux `set-hook`), if any, in
    /// the context of `session`'s first client. A no-op when the hook is unset or
    /// the session has no client. Centralizes hook dispatch so every fire site
    /// (pane-exited, window-linked, client-attached, …) shares one path.
    fn fire_hook(&mut self, session: SessionId, event: &str) {
        let Some(cmd) = self.daemon.hook_command(event) else {
            return;
        };
        if let Some(client) = self.session_clients(session).first().copied() {
            self.dispatch_command_line(client, session, &cmd);
        }
    }

    fn on_pane_exit(&mut self, session: SessionId, pane: PaneId, result: CascadeResult) {
        // A closed pane can't stay marked.
        self.daemon.clear_mark_if(pane);
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
                // A sibling pane just grew to fill the closed pane's space in
                // the layout tree; push that into its PTY/grid too, or the
                // freed space stays dead (the shell never learns its terminal
                // got bigger, so nothing redraws there) — the same step
                // split/break-pane/swap-pane already take after a layout change.
                let size = self
                    .daemon
                    .server
                    .effective_size(session)
                    .unwrap_or(PtySize::new(80, 24));
                self.daemon.resize_all_windows(session, size);
                self.invalidate_session(session);
                // Closing a pane/window may reveal a background window. Batch
                // its seen transition after PTYs/grids have their final geometry
                // so no intermediate frame combines the new layout with old
                // dimensions.
                self.coordinate_visibility(VisibilityTransition::SessionExposed(session));

                let ids = self.session_clients(session);
                for id in ids {
                    if let Some(h) = self.clients.get(&id) {
                        let _ = h.out.send(ServerMsg::Event(Event::PaneExited {
                            pane: pane.to_string(),
                            status: 0,
                        }));
                    }
                }
            }
        }
        // Pane/session cleanup changes the globally rendered session and agent
        // rows even for clients attached elsewhere. This is the single
        // invalidation seam for that projection; damage tracking keeps the
        // resulting update incremental.
        self.render_global_views();
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

    /// Request a final frame for every client attached to `session`. Requests
    /// are coalesced by client id until the next render-phase flush.
    fn render_session(&mut self, session: SessionId) {
        self.pending_renders.extend(self.session_clients(session));
    }

    /// Apply the session's shared outer size to every window. Outer geometry and
    /// sidebar width are session-global, so inactive windows must be fitted too.
    fn reconcile_session_geometry(&mut self, session: SessionId) {
        if let Some(size) = self.daemon.server.effective_size(session) {
            self.daemon.resize_all_windows(session, size);
        }
    }

    /// Reconcile one session and repaint its attached clients. Attach uses the
    /// lower-level reconciler because its Attached acknowledgement must precede
    /// the first frame; every other lifecycle path uses this complete operation.
    fn reflow_session(&mut self, session: SessionId) {
        self.reconcile_session_geometry(session);
        self.render_session(session);
    }

    /// Re-fit every session after a config change that may alter sidebar
    /// geometry. Source-file, reload, and runtime options share this lifecycle.
    fn reflow_all_sessions(&mut self) {
        let sessions = self.daemon.server.session_ids();
        for session in sessions {
            self.reflow_session(session);
        }
    }

    /// Request a render for every connected client, regardless of session. Used
    /// when a change affects views that span all sessions — e.g. an agent-status
    /// report updates the sidebar / chooser on clients attached to *other* sessions.
    /// Renderers retain their prior screen so an agent transition emits only
    /// the changed sidebar/chooser cells rather than clearing every terminal.
    fn render_global_views(&mut self) {
        self.pending_renders.extend(self.clients.keys().copied());
    }

    /// Show/hide the sidebar for `session` and reflow. Because the sidebar steals
    /// columns from the shared content grid, toggling it resizes every pane of
    /// the session (session-global by design) and forces a full repaint.
    fn set_session_sidebar(&mut self, session: SessionId, on: bool) {
        if self.daemon.sidebar_visible(session) == on {
            return;
        }
        self.daemon.set_sidebar_visible(session, on);
        self.reconcile_session_geometry(session);
        for id in self.session_clients(session) {
            self.daemon.invalidate_client(id);
            self.render_client(id);
        }
    }

    /// Collapse/expand the sidebar for `session` and reflow. Like
    /// `set_session_sidebar`, the width change resizes every pane of the session.
    fn set_session_sidebar_collapsed(&mut self, session: SessionId, collapsed: bool) {
        if self.daemon.sidebar_collapsed(session) == collapsed {
            return;
        }
        self.daemon.set_sidebar_collapsed(session, collapsed);
        self.reconcile_session_geometry(session);
        for id in self.session_clients(session) {
            self.daemon.invalidate_client(id);
            self.render_client(id);
        }
    }

    /// Request one client's final view; repeated requests in the same phase are
    /// intentionally idempotent.
    fn render_client(&mut self, client_id: u64) {
        if self.clients.contains_key(&client_id) {
            self.pending_renders.insert(client_id);
        }
    }

    /// Emit the final view requested during this control-loop message. Rendering
    /// is deliberately delayed until every model, geometry, acknowledgement,
    /// and overlay mutation in the batch has completed.
    fn flush_renders(&mut self) {
        let ids = std::mem::take(&mut self.pending_renders);
        for client_id in ids {
            self.emit_client(client_id);
        }
    }

    fn emit_client(&mut self, client_id: u64) {
        let Some(session) = self.clients.get(&client_id).map(|h| h.session) else {
            return;
        };
        if let Some(frame) = self.daemon.render_client_frame(client_id, session) {
            if let Some(h) = self.clients.get_mut(&client_id) {
                let interactions_changed = h
                    .frame_history
                    .back()
                    .is_none_or(|previous| previous.interactions != frame.interactions);
                if frame.bytes.is_empty() && !interactions_changed {
                    return;
                }
                let epoch = h.next_frame_epoch;
                h.next_frame_epoch = epoch.checked_add(1).unwrap_or(1);
                if epoch == u64::MAX {
                    h.frame_history.clear();
                }
                h.frame_history.push_back(FrameSnapshot {
                    epoch,
                    interactions: frame.interactions,
                });
                while h.frame_history.len() > FRAME_HISTORY_LIMIT {
                    h.frame_history.pop_front();
                }
                let _ = h.out.send(ServerMsg::FrameAt {
                    epoch,
                    bytes: frame.bytes.into_bytes(),
                });
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

    // One-shot control requests never register as terminal clients. Keeping
    // this path before ClientConnected is the architectural guarantee that a
    // hook/CLI process cannot influence effective_size or receive a render.
    if let ClientMsg::Control(request) = first {
        let (out_tx, out_rx) = channel::<ServerMsg>();
        tx.send(Msg::Control {
            request,
            out: out_tx,
        })
        .map_err(|_| std::io::Error::other("control loop gone"))?;
        for msg in out_rx {
            let done = matches!(msg, ServerMsg::Detached);
            writer.write_frame(&encode(&msg).map_err(|e| std::io::Error::other(e.to_string()))?)?;
            if done {
                break;
            }
        }
        return Ok(());
    }

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
