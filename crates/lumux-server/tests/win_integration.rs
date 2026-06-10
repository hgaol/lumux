//! Windows integration tests over ConPTY + named pipes.
//!
//! Mirrors the Unix keystone scenarios on the real Windows backend, covering all
//! six feature areas end-to-end: sessions, keyboard shortcuts, windows, panes,
//! configuration, and the rest (copy-mode, send-keys, status bar, help/chooser
//! overlays). These run on a real Windows host (ConPTY + named pipes); from
//! Linux they are still type-checked via the msvc cross-target.
//!
//! Harness note: each [`TestClient`] runs a dedicated reader thread that decodes
//! frames into an mpsc channel, so `collect_*` waits are bounded by
//! `recv_timeout` and can never block forever. (A naive loop calling the
//! pipe's blocking `read_frame()` directly deadlocks once the shell goes idle,
//! because the overlapped read waits on its event with an INFINITE timeout and
//! the surrounding deadline is never re-checked.)

#![cfg(windows)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lumux_backend_win::{PipeListener, PipeTransport, WinPtySystem};
use lumux_core::proto::{encode, ClientMsg, Command, Event, ServerMsg, WireSize};
use lumux_core::traits::{FrameReader, FrameWriter, Transport};

fn unique_pipe() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(r"\\.\pipe\lumux-test-{pid}-{n}")
}

fn start_daemon() -> String {
    let path = unique_pipe();
    let listener = PipeListener::bind(path.clone()).expect("bind pipe");
    std::thread::spawn(move || {
        let _ = lumux_server::run(WinPtySystem, listener);
    });
    // Give the listener a moment to call CreateNamedPipe before first connect.
    std::thread::sleep(Duration::from_millis(100));
    path
}

/// Like `start_daemon` but seeds the daemon with a specific config (used to
/// exercise the styled status bar / time tokens).
fn start_daemon_with_config(cfg: lumux_core::config::Config) -> String {
    let path = unique_pipe();
    let listener = PipeListener::bind(path.clone()).expect("bind pipe");
    std::thread::spawn(move || {
        let _ = lumux_server::run_with_config(WinPtySystem, listener, cfg);
    });
    std::thread::sleep(Duration::from_millis(100));
    path
}

/// A framed test client over the real named-pipe transport. The reader half runs
/// on its own thread and forwards decoded `ServerMsg`s through a channel, so all
/// waits below are timeout-bounded.
struct TestClient {
    writer: lumux_backend_win::PipeWriter,
    rx: Receiver<ServerMsg>,
    _reader: JoinHandle<()>,
}

impl TestClient {
    fn connect(path: &str) -> Self {
        // Retry briefly: the server creates a fresh instance per accept().
        let deadline = Instant::now() + Duration::from_secs(5);
        let transport = loop {
            if let Ok(t) = PipeTransport::connect(path) {
                break t;
            }
            if Instant::now() >= deadline {
                panic!("could not connect to test daemon pipe");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let (mut reader, mut writer) = transport.split().expect("split");

        // Handshake synchronously (one Hello each way) before the reader thread
        // takes ownership of the reader half.
        let hello = lumux_core::proto::Hello::current("win-test-client");
        writer.write_frame(&encode(&hello).unwrap()).unwrap();
        let _ = reader.read_frame().expect("read daemon hello");

        let (tx, rx) = channel();
        let reader = std::thread::spawn(move || loop {
            match reader.read_frame() {
                Ok(Some(bytes)) => {
                    if let Ok(msg) = lumux_core::proto::decode::<ServerMsg>(&bytes) {
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                }
                _ => break,
            }
        });

        Self {
            writer,
            rx,
            _reader: reader,
        }
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.writer.write_frame(&encode(msg).unwrap()).unwrap();
    }

    /// Wait until a server message satisfies `pred`, accumulating all VT frame
    /// bytes seen along the way. Bounded by `timeout`.
    fn wait_for(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&ServerMsg) -> bool,
    ) -> (bool, String) {
        let deadline = Instant::now() + timeout;
        let mut vt = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return (false, vt);
            }
            match self.rx.recv_timeout(remaining) {
                Ok(msg) => {
                    if let ServerMsg::Frame(b) = &msg {
                        vt.push_str(&String::from_utf8_lossy(b));
                    }
                    if pred(&msg) {
                        return (true, vt);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    return (false, vt)
                }
            }
        }
    }

    /// Convenience: wait for the attach ack.
    fn wait_attached(&mut self) -> bool {
        self.wait_for(Duration::from_secs(5), |m| {
            matches!(m, ServerMsg::Attached { .. })
        })
        .0
    }

    /// Collect VT frames until the accumulated text contains `needle` (returns
    /// early on match) or `timeout` elapses. Returns (found, accumulated_vt).
    fn collect_text(&mut self, timeout: Duration, needle: &str) -> (bool, String) {
        let deadline = Instant::now() + timeout;
        let mut vt = String::new();
        loop {
            if vt.contains(needle) {
                return (true, vt);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return (vt.contains(needle), vt);
            }
            match self.rx.recv_timeout(remaining) {
                Ok(ServerMsg::Frame(b)) => vt.push_str(&String::from_utf8_lossy(&b)),
                Ok(_) => {}
                Err(_) => return (vt.contains(needle), vt),
            }
        }
    }

    /// Drain frames for a fixed settle window, returning all VT text seen. Used
    /// when we want the full repaint after an action with no single marker.
    fn drain(&mut self, settle: Duration) -> String {
        let deadline = Instant::now() + settle;
        let mut vt = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return vt;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(ServerMsg::Frame(b)) => vt.push_str(&String::from_utf8_lossy(&b)),
                Ok(_) => {}
                Err(_) => return vt,
            }
        }
    }
}

fn size() -> WireSize {
    WireSize { cols: 80, rows: 24 }
}

fn new_cmd_session(path: &str, name: &str) -> TestClient {
    let mut c = TestClient::connect(path);
    c.send(&ClientMsg::NewSession {
        name: Some(name.into()),
        shell: Some("cmd.exe".into()),
        size: size(),
    });
    assert!(c.wait_attached(), "daemon must ack the new session");
    c
}

// ===========================================================================
// 1. Sessions
// ===========================================================================

#[test]
fn attach_creates_session_over_named_pipe() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::Attach {
        session: Some("work".into()),
        size: size(),
    });
    assert!(c.wait_attached(), "daemon must ack the attach over the pipe");
}

#[test]
fn default_shell_session_spawns_a_working_windows_shell() {
    // Regression: with no shell specified and no default_shell in config, the
    // daemon used a Unix path (SHELL or /bin/sh) that ConPTY can't spawn on
    // Windows — the pane died instantly, giving an empty window that exits on
    // the first keypress. The default shell must now be a real Windows shell, so
    // a shell:None session renders live output. (PowerShell echoes the marker;
    // we look for the marker text rather than a specific prompt.)
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("defshell".into()),
        shell: None, // <- exercises default_shell()
        size: size(),
    });
    assert!(c.wait_attached(), "default-shell session must attach");
    // Run a command and see it come back — proves the shell actually started.
    c.send(&ClientMsg::Command(Command::SendKeys {
        keys: b"echo LUMUX_DEFAULT_SHELL_OK\r\n".to_vec(),
    }));
    let (ok, vt) = c.collect_text(Duration::from_secs(12), "LUMUX_DEFAULT_SHELL_OK");
    assert!(
        ok,
        "default shell must spawn and produce output (not an empty, dead pane); got:\n{vt}"
    );
}

#[test]
fn daemon_survives_idle_ticks_before_first_client() {
    // Regression: the periodic exit-poll Tick must NOT make a freshly-bound
    // daemon self-terminate before any client connects. Wait well past several
    // 250ms tick intervals, then connect — the daemon must still be there.
    let path = start_daemon();
    std::thread::sleep(Duration::from_millis(900));
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::Attach {
        session: Some("late".into()),
        size: size(),
    });
    assert!(
        c.wait_attached(),
        "daemon must stay alive through idle ticks and serve a late client"
    );
}

#[test]
fn cmd_shell_runs_under_conpty() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "s");
    c.send(&ClientMsg::Input(b"echo LUMUX_WIN_MARKER\r\n".to_vec()));
    let (saw, vt) = c.collect_text(Duration::from_secs(10), "LUMUX_WIN_MARKER");
    assert!(saw, "cmd.exe output should render via ConPTY; got:\n{vt}");
}

#[test]
fn detach_then_reattach_preserves_session_windows() {
    let path = start_daemon();

    let mut c1 = new_cmd_session(&path, "persist");
    c1.send(&ClientMsg::Input(b"echo PERSIST_WIN\r\n".to_vec()));
    c1.collect_text(Duration::from_secs(8), "PERSIST_WIN");
    c1.send(&ClientMsg::Detach);
    c1.wait_for(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Detached)
    });
    drop(c1);

    let mut c2 = TestClient::connect(&path);
    c2.send(&ClientMsg::Attach {
        session: Some("persist".into()),
        size: size(),
    });
    assert!(c2.wait_attached(), "reattach over named pipe must succeed");
    let (saw, vt) = c2.collect_text(Duration::from_secs(5), "PERSIST_WIN");
    assert!(
        saw,
        "reattached screen should still show pre-detach output; got:\n{vt}"
    );
}

#[test]
fn list_sessions_reports_session() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "alpha");
    c.send(&ClientMsg::Command(Command::ListSessions));
    let (ok, reply) = c.wait_for(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Reply(t) if t.contains("alpha"))
    });
    assert!(ok, "ls should report the live session; got:\n{reply}");
}

#[test]
fn kill_session_detaches_client() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "doomed");
    c.send(&ClientMsg::Command(Command::KillSession {
        target: "doomed".into(),
    }));
    let (closed, _) = c.wait_for(Duration::from_secs(3), |m| {
        matches!(
            m,
            ServerMsg::Event(Event::SessionClosed) | ServerMsg::Detached
        )
    });
    assert!(closed, "kill-session must close and detach the client");
}

#[test]
fn exiting_only_shell_closes_session() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "dies");
    c.send(&ClientMsg::Input(b"exit\r\n".to_vec()));
    let (closed, _) = c.wait_for(Duration::from_secs(8), |m| {
        matches!(
            m,
            ServerMsg::Event(Event::SessionClosed) | ServerMsg::Detached
        )
    });
    assert!(closed, "exiting the last shell must close the session");
}

#[test]
fn bad_shell_argv_does_not_crash_daemon() {
    let path = start_daemon();
    let mut bad = TestClient::connect(&path);
    bad.send(&ClientMsg::NewSession {
        name: Some("bad".into()),
        shell: Some(r"C:\no\such\lumux-nonexistent.exe".into()),
        size: size(),
    });
    bad.drain(Duration::from_secs(2));
    drop(bad);

    // The daemon must still serve a good session afterwards.
    let mut good = TestClient::connect(&path);
    good.send(&ClientMsg::NewSession {
        name: Some("good".into()),
        shell: Some("cmd.exe".into()),
        size: size(),
    });
    assert!(
        good.wait_attached(),
        "daemon must survive a bad-shell client and serve the next one"
    );
}

#[test]
fn control_command_then_detach_is_bounded() {
    // Regression: the CLI control client (`lumux new-window`, `split-window`,
    // `send-keys`) issues a Command that produces NO Reply — only render frames —
    // then detaches. It must rely on the guaranteed Detached response to know
    // when to stop reading; waiting for a Reply would block forever. This test
    // mirrors that exact sequence and asserts the daemon answers Detached.
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "ctl");
    // A no-Reply command (NewWindow), immediately followed by Detach — the
    // ordering the control client uses.
    c.send(&ClientMsg::Command(Command::NewWindow { name: None }));
    c.send(&ClientMsg::Detach);
    let (detached, _) = c.wait_for(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Detached)
    });
    assert!(
        detached,
        "daemon must answer Detach with Detached so the CLI client is bounded"
    );

    // And the command took effect: reattaching shows a session with 2 windows.
    // (Attach first — the daemon expects Attach/NewSession as the first frame,
    // exactly as the real control client does — then issue ListSessions.)
    let mut c2 = TestClient::connect(&path);
    c2.send(&ClientMsg::Attach {
        session: None,
        size: size(),
    });
    assert!(c2.wait_attached(), "control client attach must succeed");
    c2.send(&ClientMsg::Command(Command::ListSessions));
    let (ok, reply) = c2.wait_for(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Reply(t) if t.contains("2 windows"))
    });
    assert!(
        ok,
        "the NewWindow command must have been applied before detach; got:\n{reply}"
    );
}

// ===========================================================================
// 2. Keyboard shortcuts
// ===========================================================================

#[test]
fn prefix_question_shows_help_overlay() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "help");
    // Ctrl-b ? opens the help overlay.
    c.send(&ClientMsg::Input(vec![0x02, b'?']));
    let (saw, vt) = c.collect_text(Duration::from_secs(3), "key bindings");
    assert!(saw, "prefix ? should render the help overlay; got:\n{vt}");
    assert!(vt.contains("HELP"), "help overlay should show a HELP banner");
    // Any key dismisses it.
    c.send(&ClientMsg::Input(b"q".to_vec()));
    let vt2 = c.drain(Duration::from_secs(2));
    assert!(
        !vt2.contains("-- HELP --"),
        "a keypress should dismiss the help overlay"
    );
}

#[test]
fn send_prefix_twice_passes_literal_to_shell() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "litp");
    // Ctrl-b Ctrl-b sends a literal Ctrl-b (0x02) to the shell. It must NOT be
    // treated as a command; the daemon stays alive and keeps rendering.
    c.send(&ClientMsg::Input(vec![0x02, 0x02]));
    c.drain(Duration::from_secs(1));
    // The session is still usable afterwards.
    c.send(&ClientMsg::Input(b"echo AFTER_LITERAL\r\n".to_vec()));
    let (saw, vt) = c.collect_text(Duration::from_secs(8), "AFTER_LITERAL");
    assert!(saw, "shell still works after a literal prefix; got:\n{vt}");
}

// ===========================================================================
// 3. Windows
// ===========================================================================

#[test]
fn fresh_pane_reserves_status_row_in_pty_height() {
    // Regression: panes were spawned at the full client height, so the shell
    // thought it owned the bottom row and wrote its prompt there — overlapping
    // the status bar (which the daemon paints on that same row). A pane's grid
    // (and thus its PTY) must be sized to rows-1. Drive the Daemon directly so we
    // can read the grid dimensions of the freshly-spawned pane.
    use lumux_server::Daemon;
    let mut d = Daemon::new(WinPtySystem);
    let (sid, pid, _reader) = d
        .new_session("h", Some(vec!["cmd.exe".into()]), size().into())
        .expect("spawn session");
    let _ = sid;
    let pane = d.live_pane_mut(pid).expect("pane exists");
    let (cols, rows) = pane.grid.dimensions();
    assert_eq!(cols, 80, "full width is used");
    assert_eq!(
        rows, 23,
        "pane grid must reserve the status row (24 -> 23), got {rows}"
    );
}

#[test]
fn new_window_and_status_lists_windows() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "wins");
    // Ctrl-b c twice -> three windows (0,1,2). Resize forces a full repaint so
    // the whole status row (window list) is re-sent.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.drain(Duration::from_secs(1));
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.drain(Duration::from_secs(1));
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let vt = c.drain(Duration::from_secs(2));
    assert!(
        vt.contains("0:") && vt.contains("1:") && vt.contains("2:") && vt.contains('*'),
        "status bar should list all windows with an active marker; got:\n{vt}"
    );
}

#[test]
fn next_prev_window_navigation() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "nav");
    // Two windows total (0,1); creating window 1 focuses it (marked '*').
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.drain(Duration::from_secs(1));
    // Ctrl-b p -> back to window 0. Force repaint.
    c.send(&ClientMsg::Input(vec![0x02, b'p']));
    c.send(&ClientMsg::Resize(WireSize { cols: 92, rows: 24 }));
    let vt = c.drain(Duration::from_secs(2));
    assert!(
        vt.contains("0:") && vt.contains("1:"),
        "window list should show both windows after navigation; got:\n{vt}"
    );
}

#[test]
fn select_window_respects_base_index() {
    // Regression: with base_index = 1 the status bar numbers windows 1,2,…, but
    // pressing the digit indexed the window list directly (0-based), so "1"
    // selected the SECOND window and the first was unreachable. The selection
    // must now map the displayed number back through base_index.
    let mut cfg = lumux_core::config::Config::default();
    cfg.base_index = 1;
    let path = start_daemon_with_config(cfg);
    let mut c = new_cmd_session(&path, "win1");
    // Create a second window (now numbered 2); it becomes active.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.drain(Duration::from_secs(1));

    // Press prefix + 1: must focus the FIRST window. Its name is "0" (from the
    // session's initial window), so the active marker lands on "1:0*".
    c.send(&ClientMsg::Input(vec![0x02, b'1']));
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let vt = c.drain(Duration::from_secs(2));
    assert!(
        vt.contains("1:0*"),
        "prefix 1 should select window #1 under base_index=1; got:\n{vt}"
    );

    // And prefix + 2 selects the second window (numbered 2, blank name): "2:*".
    c.send(&ClientMsg::Input(vec![0x02, b'2']));
    c.send(&ClientMsg::Resize(WireSize { cols: 91, rows: 24 }));
    let vt2 = c.drain(Duration::from_secs(2));
    assert!(
        vt2.contains("2:*") && !vt2.contains("1:0*"),
        "prefix 2 should move the active marker to window #2; got:\n{vt2}"
    );
}

// ===========================================================================
// 4. Panes
// ===========================================================================

#[test]
fn split_horizontal_draws_vertical_border() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "splith");
    // Ctrl-b | -> side-by-side split; a vertical border glyph appears.
    c.send(&ClientMsg::Input(vec![0x02, b'|']));
    let (saw, vt) = c.collect_text(Duration::from_secs(5), "\u{2502}");
    assert!(saw, "horizontal split should draw a │ border; got:\n{vt}");
}

#[test]
fn split_vertical_draws_horizontal_border() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "splitv");
    // Ctrl-b - -> stacked split; a horizontal border glyph appears.
    c.send(&ClientMsg::Input(vec![0x02, b'-']));
    let (saw, vt) = c.collect_text(Duration::from_secs(5), "\u{2500}");
    assert!(saw, "vertical split should draw a ─ border; got:\n{vt}");
}

#[test]
fn kill_pane_removes_split() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "killp");
    // Split, confirm a border, then kill the active pane (Ctrl-b x). The border
    // should be gone after the layout collapses back to a single pane.
    c.send(&ClientMsg::Input(vec![0x02, b'|']));
    let (saw, _) = c.collect_text(Duration::from_secs(5), "\u{2502}");
    assert!(saw, "split should appear before kill");
    c.send(&ClientMsg::Input(vec![0x02, b'x']));
    // Force a full repaint so the collapsed (border-free) layout is fully sent.
    c.send(&ClientMsg::Resize(WireSize { cols: 81, rows: 24 }));
    let vt = c.drain(Duration::from_secs(2));
    assert!(
        !vt.contains('\u{2502}'),
        "killing a pane should remove the split border; got:\n{vt}"
    );
}

#[test]
fn mouse_enabled_sends_reporting_and_acts_on_events() {
    // With mouse = true the daemon must (1) tell the client terminal to start
    // reporting (DECSET 1002/1003/1006) on attach, and (2) act on incoming SGR
    // mouse sequences. A scroll-up over a pane enters copy-mode (the only
    // mouse action with an observable, shell-independent result).
    let mut cfg = lumux_core::config::Config::default();
    cfg.mouse = true;
    let path = start_daemon_with_config(cfg);
    let mut c = new_cmd_session(&path, "mouse");

    // (1) The mouse-enable sequence is pushed as a frame right after attach.
    let (enabled, vt) = c.collect_text(Duration::from_secs(3), "\x1b[?1006h");
    assert!(
        enabled,
        "mouse=true should send SGR mouse-reporting enable; got:\n{vt:?}"
    );

    // (2) Inject an SGR scroll-up (button 64) at row 1; the daemon consumes it
    // (not forwarded to the shell) and enters copy-mode, showing the mode line.
    c.send(&ClientMsg::Input(b"\x1b[<64;5;2M".to_vec()));
    let (in_copy, vt2) = c.collect_text(Duration::from_secs(3), "COPY");
    assert!(
        in_copy,
        "scroll-up with mouse on should enter copy-mode; got:\n{vt2}"
    );

    // (3) Regression: pressing `q` must EXIT copy-mode. Entering via the mouse
    // wheel bypasses the keymap's `feed`, so the keymap has to be forced into
    // Copy mode at entry — otherwise `q` leaks to the shell and copy-mode sticks.
    // After `q`, force a repaint and confirm the COPY mode line is gone.
    c.send(&ClientMsg::Input(b"q".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 81, rows: 24 }));
    let vt3 = c.drain(Duration::from_secs(2));
    assert!(
        !vt3.contains("-- COPY"),
        "`q` must exit a mouse-initiated copy-mode; got:\n{vt3}"
    );
}

#[test]
fn mouse_click_status_bar_switches_window() {
    // Clicking a window entry in the status bar switches to that window (tmux).
    // Use a left-justified bar with an empty left segment so the window list
    // starts at column 0 and the click column is deterministic.
    let mut cfg = lumux_core::config::Config::default();
    cfg.mouse = true;
    cfg.status_justify = "left".into();
    cfg.status_left = String::new();
    cfg.status_format = String::new(); // left segment empty -> centre at col 0
    let path = start_daemon_with_config(cfg);
    let mut c = new_cmd_session(&path, "wclick");
    // Create a second window (index 1); it becomes active. Status row (default
    // 24 rows -> row index 23) now reads "0:0 1:" at the far left.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.drain(Duration::from_secs(1));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let before = c.drain(Duration::from_secs(1));
    assert!(
        before.contains("1:") && before.contains('*'),
        "window 1 should be active before the click; got:\n{before}"
    );

    // SGR left-click (button 0) on the status row at column 1 (1-based) = the
    // "0" of entry "0:0" (window 0). Row is 24 (1-based) = bottom row.
    c.send(&ClientMsg::Input(b"\x1b[<0;1;24M".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 81, rows: 24 }));
    let after = c.drain(Duration::from_secs(2));
    // Window 0 ("0:0") must now carry the active marker.
    assert!(
        after.contains("0:0*"),
        "clicking the first window entry should activate window 0; got:\n{after}"
    );
}

#[test]
fn zoom_hides_other_panes_then_restores() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "zoom");
    // Split so there are two panes with a divider.
    c.send(&ClientMsg::Input(vec![0x02, b'|']));
    let (saw, _) = c.collect_text(Duration::from_secs(5), "\u{2502}");
    assert!(saw, "split should appear before zoom");

    // Ctrl-b z zooms the active pane: only it shows, so the divider disappears.
    c.send(&ClientMsg::Input(vec![0x02, b'z']));
    c.send(&ClientMsg::Resize(WireSize { cols: 81, rows: 24 }));
    let vt = c.drain(Duration::from_secs(2));
    assert!(
        !vt.contains('\u{2502}'),
        "zoom should hide the other pane (no divider); got:\n{vt}"
    );

    // Ctrl-b z again unzooms: the divider comes back.
    c.send(&ClientMsg::Input(vec![0x02, b'z']));
    c.send(&ClientMsg::Resize(WireSize { cols: 82, rows: 24 }));
    let (back, vt2) = c.collect_text(Duration::from_secs(3), "\u{2502}");
    assert!(back, "unzoom should restore the split divider; got:\n{vt2}");
}

#[test]
fn resize_pane_keeps_session_usable() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "resz");
    c.send(&ClientMsg::Input(vec![0x02, b'|']));
    c.collect_text(Duration::from_secs(5), "\u{2502}");
    // Ctrl-b L / H nudge the divider; the daemon must stay alive and rendering.
    c.send(&ClientMsg::Input(vec![0x02, b'L']));
    c.send(&ClientMsg::Input(vec![0x02, b'H']));
    c.drain(Duration::from_secs(1));
    c.send(&ClientMsg::Command(Command::SendKeys {
        keys: b"echo RESIZE_OK\r\n".to_vec(),
    }));
    let (ok, vt) = c.collect_text(Duration::from_secs(8), "RESIZE_OK");
    assert!(ok, "session still works after resize-pane; got:\n{vt}");
}

// ===========================================================================
// 5. Configuration
// ===========================================================================

#[test]
fn source_file_rebinds_prefix_live() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "cfg");

    let cfg_path = std::env::temp_dir().join(format!("lumux-wincfg-{}.toml", std::process::id()));
    std::fs::write(&cfg_path, "prefix = \"C-a\"\n").unwrap();
    c.send(&ClientMsg::Command(Command::SourceFile {
        path: cfg_path.to_string_lossy().to_string(),
    }));
    let (sourced, _) = c.wait_for(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Reply(t) if t.contains("sourced"))
    });
    assert!(sourced, "source-file should reply with confirmation");

    // Now Ctrl-a | should split (new prefix); a border appears.
    c.send(&ClientMsg::Input(vec![0x01, b'|']));
    let (saw, vt) = c.collect_text(Duration::from_secs(5), "\u{2502}");
    assert!(
        saw,
        "rebound prefix Ctrl-a should trigger a split; got:\n{vt}"
    );
    let _ = std::fs::remove_file(&cfg_path);
}

#[test]
fn status_bar_clock_shows_real_local_time() {
    // Regression: now_parts() returned zeros on Windows, so %H:%M rendered as
    // "00:00". With GetLocalTime wired, the status bar must show the real local
    // hour. Configure a clock in status_left and assert the current hour appears.
    let mut cfg = lumux_core::config::Config::default();
    cfg.status_left = "T%H:%M".to_string();
    let path = start_daemon_with_config(cfg);
    let mut c = new_cmd_session(&path, "clock");
    // Force a full repaint so the whole status row is sent.
    c.send(&ClientMsg::Resize(WireSize { cols: 88, rows: 24 }));
    let vt = c.drain(Duration::from_secs(2));

    // The rendered clock must not be the old zero default…
    assert!(
        !vt.contains("T00:00") || real_hour() == 0,
        "clock should show real local time, not the 00:00 stub; got:\n{vt}"
    );
    // …and must contain this machine's current "T<HH>:" prefix.
    let needle = format!("T{:02}:", real_hour());
    assert!(
        vt.contains(&needle),
        "status bar should show the local hour {needle}; got:\n{vt}"
    );
}

/// Current local hour via the same Win32 call the daemon uses — keeps the test
/// independent of the daemon's internal formatting.
#[cfg(windows)]
fn real_hour() -> u8 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    st.wHour as u8
}

// ===========================================================================
// 6. Others (copy-mode, send-keys, session chooser)
// ===========================================================================

#[test]
fn copy_mode_shows_mode_line() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "copy");
    c.send(&ClientMsg::Input(b"echo COPYABLE_TEXT\r\n".to_vec()));
    c.collect_text(Duration::from_secs(8), "COPYABLE_TEXT");
    // Ctrl-b [ enters copy-mode.
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    let (saw, vt) = c.collect_text(Duration::from_secs(3), "COPY");
    assert!(saw, "copy-mode should render a mode line; got:\n{vt}");
}

#[test]
fn send_keys_command_injects_into_pane() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "sk");
    c.send(&ClientMsg::Command(Command::SendKeys {
        keys: b"echo SENDKEYS_OK\r\n".to_vec(),
    }));
    let (saw, vt) = c.collect_text(Duration::from_secs(8), "SENDKEYS_OK");
    assert!(saw, "send-keys should reach the shell; got:\n{vt}");
}

#[test]
fn bell_in_pane_output_notifies_client() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "bell");
    // Echo a literal BEL (0x07) so it shows up in the pane's PTY output. ConPTY
    // relays it; the emulator decodes BEL and the daemon forwards Event::Bell.
    c.send(&ClientMsg::Command(Command::SendKeys {
        keys: b"echo \x07\r\n".to_vec(),
    }));
    let (rang, _) = c.wait_for(Duration::from_secs(8), |m| {
        matches!(m, ServerMsg::Event(Event::Bell))
    });
    assert!(rang, "a BEL in pane output must reach the client as Event::Bell");
}

#[test]
fn prefix_s_opens_session_chooser() {
    let path = start_daemon();
    // Two sessions so the chooser has something to list.
    let mut a = new_cmd_session(&path, "alpha");
    a.send(&ClientMsg::Detach);
    a.wait_for(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Detached)
    });
    drop(a);

    let mut b = new_cmd_session(&path, "beta");
    // Ctrl-b s opens the switcher; it lists both sessions.
    b.send(&ClientMsg::Input(vec![0x02, b's']));
    let (saw, vt) = b.collect_text(Duration::from_secs(3), "choose a session");
    assert!(saw, "prefix s should open the chooser; got:\n{vt}");
    assert!(
        vt.contains("alpha") && vt.contains("beta"),
        "chooser should list both sessions; got:\n{vt}"
    );
}

#[test]
fn rename_window_via_prompt_updates_status() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "rn");
    // Ctrl-b , opens the rename-window prompt; the prompt label appears.
    c.send(&ClientMsg::Input(vec![0x02, b',']));
    let (opened, _) = c.collect_text(Duration::from_secs(3), "rename-window");
    assert!(opened, "prefix , should open the rename-window prompt");
    // Clear the seeded name (a few backspaces), type a new one, confirm.
    c.send(&ClientMsg::Input(vec![0x7f; 8])); // backspaces
    c.send(&ClientMsg::Input(b"editor".to_vec()));
    c.send(&ClientMsg::Input(b"\r".to_vec())); // Enter commits
    // Force a full repaint so the status row (window list) is re-sent.
    c.send(&ClientMsg::Resize(WireSize { cols: 91, rows: 24 }));
    let (named, vt) = c.collect_text(Duration::from_secs(3), "editor");
    assert!(named, "renamed window should appear in the status bar; got:\n{vt}");
}

#[test]
fn rename_session_via_cli_command() {
    let path = start_daemon();
    let mut c = new_cmd_session(&path, "oldname");
    // The CLI rename-session path is a structured Command.
    c.send(&ClientMsg::Command(Command::RenameSession {
        name: "newname".into(),
    }));
    // List sessions: the new name must be reported, the old one gone.
    c.send(&ClientMsg::Command(Command::ListSessions));
    let (ok, reply) = c.wait_for(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Reply(t) if t.contains("newname"))
    });
    assert!(ok, "rename-session should take effect; got:\n{reply}");
}
