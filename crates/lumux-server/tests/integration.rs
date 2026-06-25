//! End-to-end integration tests for the daemon over a real Unix socket with a
//! real shell PTY. This is the Phase 7 keystone: it proves lumux genuinely
//! multiplexes, persists across detach, and cascades on exit — all on Linux,
//! before any Windows code exists.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lumux_backend_unix::{UnixPtySystem, UnixSocketListener};
use lumux_core::proto::{decode, encode, ClientMsg, ServerMsg, WireSize};

/// Spawn the daemon control loop on a throwaway socket, returning its path.
fn start_daemon() -> std::path::PathBuf {
    // Unique socket per call: pid + a process-wide monotonic counter (parallel
    // tests must not collide).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lumux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    std::thread::spawn(move || {
        let _ = lumux_server::run(UnixPtySystem, listener);
    });
    // Wait for the socket to exist.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    path
}

/// Like [`start_daemon`] but with mouse reporting enabled, for tests that drive
/// SGR mouse events (scroll/drag). Mouse is off by default, so without this the
/// daemon passes mouse sequences through as text instead of acting on them.
fn start_daemon_mouse() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(1_000_000);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lumux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    let cfg = lumux_core::config::Config {
        mouse: true,
        ..Default::default()
    };
    std::thread::spawn(move || {
        let _ = lumux_server::run_with_config(UnixPtySystem, listener, cfg);
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    path
}

/// A tiny framed client over a raw UnixStream (mirrors the real client wire
/// behavior without a terminal).
struct TestClient {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl TestClient {
    fn connect(path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(path).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut c = Self {
            stream,
            buf: Vec::new(),
        };
        c.handshake();
        c
    }

    /// Send our Hello and consume the daemon's reply Hello frame.
    fn handshake(&mut self) {
        let hello = lumux_core::proto::Hello::current("test-client");
        self.stream.write_all(&encode(&hello).unwrap()).unwrap();
        self.stream.flush().unwrap();
        // Read one frame (the daemon's Hello) and discard it.
        let mut tmp = [0u8; 1024];
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if try_decode_hello(&self.buf).is_some() {
                let (_h, consumed) = try_decode_hello(&self.buf).unwrap();
                self.buf.drain(..consumed);
                return;
            }
            match self.stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
        }
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.stream.write_all(&encode(msg).unwrap()).unwrap();
        self.stream.flush().unwrap();
    }

    /// Read frames until `pred` matches one, or timeout. Accumulates all
    /// ServerMsg::Frame VT bytes seen for content assertions.
    fn collect_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&ServerMsg) -> bool,
    ) -> (bool, String) {
        let deadline = Instant::now() + timeout;
        let mut vt = String::new();
        let mut tmp = [0u8; 4096];
        while Instant::now() < deadline {
            // Try to parse a frame from the buffer first.
            while let Some((msg, consumed)) = try_decode(&self.buf) {
                self.buf.drain(..consumed);
                if let ServerMsg::Frame(bytes) = &msg {
                    vt.push_str(&String::from_utf8_lossy(bytes));
                }
                if pred(&msg) {
                    return (true, vt);
                }
            }
            match self.stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
        }
        (false, vt)
    }
}

/// Decode one length-prefixed ServerMsg from `buf`, returning bytes consumed.
fn try_decode(buf: &[u8]) -> Option<(ServerMsg, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let msg = decode::<ServerMsg>(&buf[4..4 + len]).ok()?;
    Some((msg, 4 + len))
}

/// Decode a Hello handshake frame (the daemon's reply to ours).
fn try_decode_hello(buf: &[u8]) -> Option<(lumux_core::proto::Hello, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let h = decode::<lumux_core::proto::Hello>(&buf[4..4 + len]).ok()?;
    Some((h, 4 + len))
}

fn size() -> WireSize {
    WireSize { cols: 80, rows: 24 }
}

#[test]
fn attach_creates_session_and_acks() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::Attach {
        session: Some("work".into()),
        size: size(),
    });
    let (ok, _) = c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(ok, "daemon must ack the attach");
}

#[test]
fn shell_command_output_appears_in_frames() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("s".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    let (acked, _) = c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(acked);

    // Type a command with a unique marker and run it.
    c.send(&ClientMsg::Input(b"echo LUMUX_MARKER_123\n".to_vec()));
    let (saw, vt) = c.collect_until(Duration::from_secs(3), |_| false);
    let _ = saw;
    assert!(
        vt.contains("LUMUX_MARKER_123"),
        "shell output should render into frames; got:\n{vt}"
    );
}

#[test]
fn detach_then_reattach_preserves_session() {
    let path = start_daemon();

    // Client 1 creates a session and writes a marker into the shell's screen.
    let mut c1 = TestClient::connect(&path);
    c1.send(&ClientMsg::NewSession {
        name: Some("persist".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c1.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Use a prompt-independent persistent artifact: set a shell variable AND
    // echo a marker so it is on-screen.
    c1.send(&ClientMsg::Input(b"echo PERSIST_ME_456\n".to_vec()));
    c1.collect_until(Duration::from_secs(2), |_| false);
    // Detach cleanly.
    c1.send(&ClientMsg::Detach);
    c1.collect_until(Duration::from_secs(1), |m| matches!(m, ServerMsg::Detached));
    drop(c1);

    // Client 2 reattaches by name; the session (and its on-screen marker) must
    // still be there — proving the daemon kept it alive across detach.
    let mut c2 = TestClient::connect(&path);
    c2.send(&ClientMsg::Attach {
        session: Some("persist".into()),
        size: size(),
    });
    let (ok, vt) = c2.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(ok, "reattach must succeed");
    // The first full repaint after reattach redraws the live screen including
    // the earlier marker.
    let (_done, vt2) = c2.collect_until(Duration::from_secs(2), |_| false);
    let combined = format!("{vt}{vt2}");
    assert!(
        combined.contains("PERSIST_ME_456"),
        "reattached screen should still show pre-detach output; got:\n{combined}"
    );
}

#[test]
fn duplicate_session_name_is_rejected() {
    // tmux refuses `new-session -s <name>` when the name already exists. A second
    // NewSession with the same name must get an Error, not a fresh Attached.
    let path = start_daemon();
    let mut c1 = TestClient::connect(&path);
    c1.send(&ClientMsg::NewSession {
        name: Some("dup".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    let (created, _) = c1.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(created, "first session should be created");

    // Second client tries the same name.
    let mut c2 = TestClient::connect(&path);
    c2.send(&ClientMsg::NewSession {
        name: Some("dup".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    let mut got_error = false;
    let mut got_attached = false;
    let (_done, _) = c2.collect_until(Duration::from_secs(2), |m| match m {
        ServerMsg::Error(e) if e.contains("duplicate session") => {
            got_error = true;
            true
        }
        ServerMsg::Attached { .. } => {
            got_attached = true;
            true
        }
        _ => false,
    });
    assert!(got_error, "duplicate name must be rejected with an error");
    assert!(!got_attached, "duplicate name must NOT create/attach a session");
}

#[test]
fn two_clients_can_attach_concurrently() {
    // Regression: the accept loop used to service each client inline, blocking in
    // its reader loop, so a second client could never connect while the first was
    // alive — no shared/multi-client attach. Both clients must attach at once.
    let path = start_daemon();
    let mut a = TestClient::connect(&path);
    a.send(&ClientMsg::NewSession {
        name: Some("shared".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    let (a_ok, _) = a.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(a_ok, "first client attaches");

    // Second client attaches to the SAME session while the first is still alive.
    let mut b = TestClient::connect(&path);
    b.send(&ClientMsg::Attach {
        session: Some("shared".into()),
        size: size(),
    });
    let (b_ok, _) = b.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(b_ok, "second client must attach while the first is still connected");

    // Both connections are live: input from the second client reaches the shell
    // and the first client (sharing the session) sees the echoed output.
    b.send(&ClientMsg::Input(b"echo SHARED_OK_789\n".to_vec()));
    let (_d, vt_a) = a.collect_until(Duration::from_secs(3), |_| false);
    assert!(
        vt_a.contains("SHARED_OK_789"),
        "first client should see output driven by the second (shared session); got:\n{vt_a}"
    );
}

#[test]
fn split_creates_second_pane_with_border() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("s".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Ctrl-b % -> split horizontally. Send as raw input through the keymap.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    // A vertical border glyph should now appear in the rendered frame.
    assert!(
        vt.contains('│'),
        "split should render a pane border; got:\n{vt}"
    );
}

#[test]
fn exiting_only_shell_closes_session() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("dies".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Exit the shell; its pane is the last one, so the session cascades closed
    // and the daemon should notify us.
    c.send(&ClientMsg::Input(b"exit\n".to_vec()));
    let (closed, _) = c.collect_until(Duration::from_secs(3), |m| {
        matches!(
            m,
            ServerMsg::Event(lumux_core::proto::Event::SessionClosed) | ServerMsg::Detached
        )
    });
    assert!(closed, "exiting the last shell must close the session");
}

#[test]
fn exiting_one_window_keeps_session_alive() {
    // Regression: exiting a shell in a multi-window session must close only that
    // window, not end the session. The daemon must signal this with PaneExited
    // (a survivable event) and NOT SessionClosed — the client uses exactly that
    // distinction to decide whether to stay attached.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("multi".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Open a second window (Ctrl-b c); the session now has two.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Exit the shell in the active (second) window. Its pane is the last in that
    // window, so the window closes — but window 0 remains, so the session lives.
    c.send(&ClientMsg::Input(b"exit\n".to_vec()));
    let (saw_pane_exit, _) = c.collect_until(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Event(lumux_core::proto::Event::PaneExited { .. }))
    });
    assert!(
        saw_pane_exit,
        "exiting a window in a multi-window session should emit PaneExited"
    );
    // The session must still be alive: a resize forces a fresh repaint, and the
    // status bar still lists the surviving window. A closed session would instead
    // have sent SessionClosed/Detached and stopped rendering.
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (closed, vt) = c.collect_until(Duration::from_secs(2), |m| {
        matches!(
            m,
            ServerMsg::Event(lumux_core::proto::Event::SessionClosed) | ServerMsg::Detached
        )
    });
    assert!(
        !closed,
        "session must stay alive after one window of several exits"
    );
    assert!(
        vt.contains("0:"),
        "the surviving window should still render in the status bar; got:\n{vt}"
    );
}

#[test]
fn copy_mode_shows_mode_line() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("copy".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Produce some output so there's content, then enter copy-mode (Ctrl-b [).
    c.send(&ClientMsg::Input(b"echo COPYABLE_TEXT\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("COPY"),
        "copy-mode should render a mode line; got:\n{vt}"
    );
}

#[test]
fn copy_mode_keeps_other_panes_with_multiple_panes() {
    // Regression: entering copy-mode (which scrolling does) used to paint the
    // active pane full-screen, blanking every other pane and erasing the
    // divider. With two panes side by side, copy-mode must still show the split
    // — a vertical divider glyph — not a single full-width pane.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("cpmulti".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Split into two side-by-side panes (Ctrl-b %), so a divider exists.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    let (_d, split_vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        split_vt.contains('│'),
        "precondition: split should draw a divider; got:\n{split_vt}"
    );
    // Enter copy-mode, then force a full repaint via a resize.
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(vt.contains("COPY"), "should be in copy-mode; got:\n{vt}");
    assert!(
        vt.contains('│'),
        "copy-mode must keep the divider/other pane, not paint full-screen; got:\n{vt}"
    );
}

#[test]
fn scroll_targets_the_pane_under_the_pointer() {
    // tmux scrolls the pane the wheel is over, not just the focused one. After a
    // % split the NEW (right) pane is focused; scrolling the wheel over the LEFT
    // pane must focus it and scroll ITS history — revealing a line that has
    // already scrolled off the live screen, which only appears when the left
    // pane is the scroll target.
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("scrl".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // In the left pane, print a unique marker then push it off-screen with enough
    // blank lines that it's only visible by scrolling THIS pane's history.
    c.send(&ClientMsg::Input(b"echo LEFT_HISTORY_XZ; for i in $(seq 1 40); do echo .; done\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);
    // Split right; focus moves to the new (right) pane, which has no such history.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Sanity: the marker has scrolled off — it's not on the live screen now.
    let (_d0, live) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        !live.contains("LEFT_HISTORY_XZ"),
        "precondition: marker should be scrolled off the live view; got:\n{live}"
    );
    // Wheel-up many notches over the LEFT pane (SGR ESC[<64;col;row M, col 10);
    // enough to scroll past the ~40 filler lines back to the marker.
    for _ in 0..20 {
        c.send(&ClientMsg::Input(b"\x1b[<64;10;12M".to_vec()));
        c.collect_until(Duration::from_millis(60), |_| false);
    }
    // Force a full repaint so we capture the complete current screen.
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(vt.contains("COPY"), "wheel should open copy-mode; got:\n{vt}");
    assert!(
        vt.contains("LEFT_HISTORY_XZ"),
        "scrolling over the LEFT pane must scroll ITS history into view; got:\n{vt}"
    );
}

#[test]
fn scroll_on_alt_screen_sends_arrow_keys_to_the_app() {
    // A pane on the alternate screen (a TUI app like vim/less or an agent) owns
    // the viewport and has no scrollback. The wheel must be sent to the app as
    // arrow keys, NOT open copy-mode — otherwise scrolling appears to do nothing.
    // We prove the arrows actually reach the app with `cat -v`, which echoes its
    // stdin and renders control bytes visibly (ESC -> ^[), so a wheel-up shows as
    // ^[[A in the pane.
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("altscrl".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Enter the alternate screen (DEC 1049, as a TUI app does on startup), then
    // run `cat -v` so input bytes echo back with control chars made visible.
    c.send(&ClientMsg::Input(b"printf '\\033[?1049h'; cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);

    // Wheel-up over the pane a few notches, accumulating everything the pane
    // echoes back (cat -v renders the sent arrows as ^[[A).
    let mut seen = String::new();
    for _ in 0..3 {
        c.send(&ClientMsg::Input(b"\x1b[<64;10;12M".to_vec()));
        let (_d, chunk) = c.collect_until(Duration::from_millis(200), |_| false);
        seen.push_str(&chunk);
    }
    let (_done, tail) = c.collect_until(Duration::from_secs(1), |_| false);
    seen.push_str(&tail);
    assert!(
        !seen.contains("-- COPY --"),
        "wheel on an alt-screen pane must not open copy-mode; got:\n{seen}"
    );
    assert!(
        seen.contains("^[[A"),
        "wheel-up must send an up-arrow (ESC[A) to the alt-screen app; got:\n{seen}"
    );
}

#[test]
fn send_keys_command_injects_into_pane() {
    use lumux_core::proto::Command;
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("sk".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Send a command via SendKeys (scripting path, not raw Input).
    c.send(&ClientMsg::Command(Command::SendKeys {
        keys: b"echo SENDKEYS_OK\n".to_vec(),
    }));
    let (_done, vt) = c.collect_until(Duration::from_secs(3), |_| false);
    assert!(
        vt.contains("SENDKEYS_OK"),
        "send-keys should reach the shell; got:\n{vt}"
    );
}

#[test]
fn source_file_rebinds_prefix_live() {
    use lumux_core::proto::Command;
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("cfg".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });

    // Write a config that changes the prefix to Ctrl-a, then source it.
    let cfg_path = std::env::temp_dir().join(format!("lumux-cfg-{}.toml", std::process::id()));
    std::fs::write(&cfg_path, "prefix = \"C-a\"\n").unwrap();
    c.send(&ClientMsg::Command(Command::SourceFile {
        path: cfg_path.to_string_lossy().to_string(),
    }));
    let (sourced, _) = c.collect_until(
        Duration::from_secs(2),
        |m| matches!(m, ServerMsg::Reply(t) if t.contains("sourced")),
    );
    assert!(sourced, "source-file should reply with confirmation");

    // Now Ctrl-a % should split (new prefix); a border appears.
    c.send(&ClientMsg::Input(vec![0x01, b'%']));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains('│'),
        "rebound prefix Ctrl-a should trigger split; got:\n{vt}"
    );
    let _ = std::fs::remove_file(&cfg_path);
}

#[test]
fn bad_shell_argv_does_not_crash_daemon() {
    let path = start_daemon();

    // First client requests a session with a nonexistent shell. Depending on
    // the platform, openpty may succeed and the child then fail to exec (its
    // pane dies), or the spawn may error outright. Either way the daemon must
    // NOT crash.
    let mut bad = TestClient::connect(&path);
    bad.send(&ClientMsg::NewSession {
        name: Some("bad".into()),
        shell: Some("/no/such/shell/lumux-nonexistent".into()),
        size: size(),
    });
    // Drain whatever comes back (Attached+exit event, or Error); we don't
    // assert the specific shape — only that the daemon stays up.
    bad.collect_until(Duration::from_secs(2), |_| false);
    drop(bad);

    // The daemon must still be alive: a good session works.
    let mut good = TestClient::connect(&path);
    good.send(&ClientMsg::NewSession {
        name: Some("good".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    let (ok, _) = good.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(
        ok,
        "daemon must survive a bad-shell client and serve the next one"
    );
}

#[test]
fn split_inherits_current_directory() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("cwd".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // cd into a unique temp dir in the first pane, and wait for the prompt.
    let dir = std::env::temp_dir().join(format!("lumux-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    c.send(&ClientMsg::Input(
        format!("cd {}\n", dir.display()).into_bytes(),
    ));
    c.collect_until(Duration::from_secs(2), |_| false);
    // Split: the new pane's shell should start in the same directory.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(2), |_| false);
    // Print the new pane's cwd; it must contain our unique dir name.
    c.send(&ClientMsg::Input(b"pwd\n".to_vec()));
    let (_done, vt) = c.collect_until(Duration::from_secs(3), |_| false);
    let marker = dir.file_name().unwrap().to_string_lossy();
    assert!(
        vt.contains(&*marker),
        "split pane should inherit cwd ({marker}); got:\n{vt}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn example_parity_config_parses_and_binds() {
    // The shipped example config must always parse and produce the expected
    // tmux-parity bindings (guards against regressions in the config surface).
    let toml = include_str!("../../../examples/config.toml");
    let cfg = lumux_core::config::Config::from_toml(toml).expect("example config parses");
    assert_eq!(cfg.prefix, "C-b");
    assert!(cfg.mouse);
    assert_eq!(cfg.scrollback, 10000);
    assert_eq!(cfg.base_index, 1);
    assert_eq!(cfg.status_justify, "centre");
    assert_eq!(cfg.status_bg, "colour24");
    // Bindings compile into a usable table (prefix + root nav + reload).
    let b = cfg.to_bindings().expect("bindings build");
    use lumux_core::keymap::{Action, Key, KeyCode};
    assert_eq!(b.lookup(&Key::char('|')), Some(&Action::SplitHorizontal));
    assert_eq!(b.lookup(&Key::char('r')), Some(&Action::ReloadConfig));
    assert_eq!(
        b.lookup_root(&Key {
            code: KeyCode::Left,
            ctrl: false,
            alt: true
        }),
        Some(&Action::SelectPaneLeft)
    );
}

#[test]
fn prefix_question_shows_help_overlay() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("help".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Ctrl-b ? opens the help overlay.
    c.send(&ClientMsg::Input(vec![0x02, b'?']));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("key bindings") && vt.contains("HELP"),
        "prefix ? should render the help overlay; got:\n{vt}"
    );
    // q closes it and returns to the live shell view.
    c.send(&ClientMsg::Input(b"q".to_vec()));
    let (_d2, vt2) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        !vt2.contains("-- HELP --"),
        "q should dismiss the help overlay"
    );
}

#[test]
fn help_overlay_scrolls_with_arrows() {
    // tmux shows key bindings in a scrollable view. The binding list is longer
    // than the screen, so scrolling down must reveal later entries and hide the
    // first one — and an unrelated keypress must NOT close the overlay.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("helpscroll".into()),
        shell: Some("/bin/sh".into()),
        // A short screen guarantees the binding list overflows and must scroll.
        size: WireSize { cols: 80, rows: 12 },
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b'?']));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    // The status line shows a position indicator when the list overflows.
    assert!(
        vt.contains("-- HELP --") && vt.contains("scroll"),
        "overflowing help should show a scroll hint; got:\n{vt}"
    );
    assert!(vt.contains("[1-"), "should start at the top (1-..); got:\n{vt}");
    // Page down a few times; the overlay stays open and the window moves.
    for _ in 0..3 {
        c.send(&ClientMsg::Input(b"\x1b[6~".to_vec())); // PageDown
        c.collect_until(Duration::from_millis(120), |_| false);
    }
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 12 }));
    let (_d2, vt2) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt2.contains("-- HELP --"),
        "scrolling must not close the overlay; got:\n{vt2}"
    );
    assert!(
        !vt2.contains("[1-"),
        "after paging down, the view should no longer start at row 1; got:\n{vt2}"
    );
}

#[test]
fn status_bar_shows_window_list() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("wins".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Create two more windows (Ctrl-b c twice), then force a full repaint with
    // a resize so the whole status row (window list) is re-sent.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    // The window list renders entries "0: 1: 2:" with the active one marked '*'.
    assert!(
        vt.contains("0:") && vt.contains("1:") && vt.contains("2:") && vt.contains('*'),
        "status bar should list all windows with an active marker; got:\n{vt}"
    );
}

#[test]
fn prefix_s_switches_session() {
    let path = start_daemon();
    // Create two sessions via two clients (so the daemon holds both).
    let mut a = TestClient::connect(&path);
    a.send(&ClientMsg::NewSession {
        name: Some("alpha".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    a.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Put a unique marker on alpha's screen.
    a.send(&ClientMsg::Input(b"echo ALPHA_HERE\n".to_vec()));
    a.collect_until(Duration::from_secs(1), |_| false);
    a.send(&ClientMsg::Detach);
    drop(a);

    let mut b = TestClient::connect(&path);
    b.send(&ClientMsg::NewSession {
        name: Some("beta".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    b.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    b.send(&ClientMsg::Input(b"echo BETA_HERE\n".to_vec()));
    b.collect_until(Duration::from_secs(1), |_| false);

    // Ctrl-b s opens the switcher; it should list both sessions.
    b.send(&ClientMsg::Input(vec![0x02, b's']));
    let (_d, vt) = b.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("choose a session") && vt.contains("alpha") && vt.contains("beta"),
        "switcher should list sessions; got:\n{vt}"
    );
    // Select the first session (index 0 = alpha) and confirm.
    b.send(&ClientMsg::Input(b"0".to_vec()));
    b.send(&ClientMsg::Input(b"\r".to_vec()));
    let (_d2, vt2) = b.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt2.contains("ALPHA_HERE"),
        "after switching, the client should see alpha's screen; got:\n{vt2}"
    );
}

#[test]
fn prefix_d_detaches_but_keeps_session_alive() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("det".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Leave a marker on screen so we can prove the session survived.
    c.send(&ClientMsg::Input(b"echo DETACH_SURVIVOR\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);

    // Press the prefix (Ctrl-b) then 'd' to detach. The daemon must send
    // ServerMsg::Detached in response (the bug: it used to be a no-op).
    c.send(&ClientMsg::Input(vec![0x02, b'd']));
    let (detached, _) =
        c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Detached));
    assert!(detached, "prefix d must make the daemon send Detached");
    drop(c);

    // Reattach: the session and its on-screen marker must still be there.
    let mut c2 = TestClient::connect(&path);
    c2.send(&ClientMsg::Attach {
        session: Some("det".into()),
        size: size(),
    });
    let (ok, vt) = c2.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(ok, "reattach after prefix-d detach must succeed");
    let (_d, vt2) = c2.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        format!("{vt}{vt2}").contains("DETACH_SURVIVOR"),
        "session must survive prefix-d detach"
    );
}
