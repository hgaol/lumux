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

/// The 1-based screen column where `needle` first begins (see [`columns_of`]).
fn column_of(vt: &str, needle: &str) -> Option<usize> {
    columns_of(vt, needle).into_iter().next()
}

/// Every 1-based screen column where `needle` is drawn, by replaying the VT byte
/// stream as a cursor would: cursor-position escapes (`ESC[row;colH`) set the
/// column, other escapes are skipped, and printable bytes advance it. The
/// renderer writes a whole row after a single CUP, so the column must be tracked
/// per character (not read from the CUP). Used to assert pane *positions* (left
/// vs right) and presence-in-multiple-panes in layout tests.
fn columns_of(vt: &str, needle: &str) -> Vec<usize> {
    let nb = needle.as_bytes();
    let b = vt.as_bytes();
    let mut out = Vec::new();
    let mut col: usize = 1;
    let mut matched = 0usize; // how many needle bytes matched at `start_col`
    let mut start_col = 1usize;
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            // Skip an escape sequence. CSI is ESC '[' ... final-byte in @-~.
            if i + 1 < b.len() && b[i + 1] == b'[' {
                let params_start = i + 2;
                let mut j = params_start;
                while j < b.len() && !(0x40..=0x7e).contains(&b[j]) {
                    j += 1;
                }
                if j < b.len() && b[j] == b'H' {
                    // CUP: parse "row;col". Missing col defaults to 1.
                    let body = &vt[params_start..j];
                    col = body
                        .split(';')
                        .nth(1)
                        .and_then(|c| c.parse().ok())
                        .unwrap_or(1);
                }
                i = j + 1;
                matched = 0;
                continue;
            }
            i += 1;
            matched = 0;
            continue;
        }
        // A printable byte at column `col`.
        if b[i] == nb[matched] {
            if matched == 0 {
                start_col = col;
            }
            matched += 1;
            if matched == nb.len() {
                out.push(start_col);
                matched = 0;
            }
        } else {
            matched = if b[i] == nb[0] { 1 } else { 0 };
            if matched == 1 {
                start_col = col;
            }
        }
        col += 1;
        i += 1;
    }
    out
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
fn wheel_is_forwarded_to_a_mouse_aware_app() {
    // Regression for "the scroll wheel sends arrow keys in Claude Code": when the
    // app in the pane has enabled mouse reporting, the wheel must be FORWARDED to
    // it as a raw SGR mouse event (so the app scrolls natively), NOT translated to
    // arrow keys. cat -v echoes the bytes the app receives, so a forwarded wheel
    // shows as the SGR sequence (^[[<64;...M), and there must be no arrow (^[[A).
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("mouseapp".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // App enables mouse reporting (button-event 1002 + SGR 1006), then cat -v.
    c.send(&ClientMsg::Input(b"printf '\\033[?1002h\\033[?1006h'; cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);

    let mut seen = String::new();
    for _ in 0..3 {
        c.send(&ClientMsg::Input(b"\x1b[<64;10;12M".to_vec()));
        let (_d, chunk) = c.collect_until(Duration::from_millis(200), |_| false);
        seen.push_str(&chunk);
    }
    let (_done, tail) = c.collect_until(Duration::from_secs(1), |_| false);
    seen.push_str(&tail);
    assert!(
        seen.contains("[<64;"),
        "a mouse-aware app must receive the raw wheel SGR event; got:\n{seen}"
    );
    assert!(
        !seen.contains("^[[A"),
        "a mouse-aware app must NOT receive translated arrow keys; got:\n{seen}"
    );
    assert!(
        !seen.contains("-- COPY --"),
        "forwarding to the app must not open copy-mode; got:\n{seen}"
    );
}

#[test]
fn split_wheel_sequence_across_frames_is_not_leaked_as_text() {
    // Regression for "scroll over SSH messes up the pane": the client forwards
    // whatever bytes a single stdin read() returned, and over SSH a TCP segment
    // boundary can fall in the MIDDLE of one wheel event — so the daemon receives
    // `\x1b[<64;10` in one Input frame and `;12M` in the next. The mouse parser
    // must buffer the partial sequence across frames; if it doesn't, the leading
    // bytes leak to the app as literal text (showing up as ^[[<64;10 in cat -v)
    // and the wheel does nothing.
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("splitwheel".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Alt-screen app with cat -v, so we can see exactly what bytes reach the app.
    c.send(&ClientMsg::Input(b"printf '\\033[?1049h'; cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);

    let mut seen = String::new();
    for _ in 0..3 {
        // Deliver one wheel-up event SPLIT across two frames, as SSH would.
        c.send(&ClientMsg::Input(b"\x1b[<64;10".to_vec()));
        let (_d, a) = c.collect_until(Duration::from_millis(120), |_| false);
        seen.push_str(&a);
        c.send(&ClientMsg::Input(b";12M".to_vec()));
        let (_d, b) = c.collect_until(Duration::from_millis(120), |_| false);
        seen.push_str(&b);
    }
    let (_done, tail) = c.collect_until(Duration::from_secs(1), |_| false);
    seen.push_str(&tail);
    // The wheel must be decoded and sent to the alt-screen app as an up-arrow.
    assert!(
        seen.contains("^[[A"),
        "a wheel event split across frames must still scroll the app; got:\n{seen}"
    );
    // And NONE of the raw sequence may leak to the app as text.
    assert!(
        !seen.contains("[<64;"),
        "split wheel bytes must not leak to the app as literal text; got:\n{seen}"
    );
}

#[test]
fn copy_mode_scroll_is_incremental_not_a_clear() {
    // Regression: scrolling in copy-mode used to invalidate the renderer every
    // step, forcing a full repaint that clears the screen (ESC[2J) each wheel
    // notch — visible as flicker. A scroll step must instead send an incremental
    // diff with no clear-screen.
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("noflicker".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Produce scrollable history so copy-mode has somewhere to go.
    c.send(&ClientMsg::Input(b"for i in $(seq 1 60); do echo line$i; done\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);
    // First wheel-up enters copy-mode (this frame legitimately repaints, and
    // shows the mode line).
    c.send(&ClientMsg::Input(b"\x1b[<64;40;12M".to_vec()));
    let (_d0, enter) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        enter.contains("-- COPY --"),
        "precondition: first wheel-up should enter copy-mode; got:\n{enter}"
    );
    // A SUBSEQUENT wheel-up is a pure scroll step — capture just its frame(s). It
    // should be an incremental diff: it moves content but never clears the
    // screen. (The mode line is unchanged, so a smooth diff won't even re-send
    // it — that's the point.)
    c.send(&ClientMsg::Input(b"\x1b[<64;40;12M".to_vec()));
    let (_d, step) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        !step.is_empty(),
        "a scroll step should still send an (incremental) frame"
    );
    assert!(
        !step.contains("\u{1b}[2J"),
        "a copy-mode scroll step must not clear the screen (no flicker); got:\n{step:?}"
    );
}

#[test]
fn copy_mode_search_jumps_to_match() {
    // tmux copy-mode `/`/`?` search. Put a unique marker far up in history, push
    // it off the live screen, enter copy-mode, and search backward for it. The
    // match must scroll into view (it isn't visible at the live tail).
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("search".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Print a unique marker, then enough lines to push it into scrollback.
    c.send(&ClientMsg::Input(
        b"echo FINDME_MARKER_42; for i in $(seq 1 60); do echo f$i; done\n".to_vec(),
    ));
    c.collect_until(Duration::from_secs(2), |_| false);
    // Precondition: the marker is NOT on the live screen.
    let (_d0, live) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        !live.contains("FINDME_MARKER_42"),
        "precondition: marker should be scrolled off; got:\n{live}"
    );
    // Enter copy-mode (prefix [), then type a backward search: ? F I N D M E Enter.
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"?FINDME\r".to_vec()));
    // Force a full repaint so we capture the whole post-search screen.
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("FINDME_MARKER_42"),
        "search must scroll the matching line into view; got:\n{vt}"
    );
}

#[test]
fn copy_mode_search_prompt_is_shown_while_typing() {
    // While typing a search query, the bottom row shows it as `?FINDME` (tmux's
    // incremental search prompt), not the normal "-- COPY --" status.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("searchprompt".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(b"for i in $(seq 1 40); do echo l$i; done\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Open backward search and type a few chars (no Enter yet).
    c.send(&ClientMsg::Input(b"?abc".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("?abc"),
        "the search prompt should echo the typed query; got:\n{vt}"
    );
}

#[test]
fn yank_then_paste_round_trips_into_the_pane() {
    // tmux paste buffers: a copy-mode yank becomes the newest buffer, and prefix
    // `]` pastes it into the active pane. We prove the full round trip: select a
    // line containing a unique marker, yank it, CLEAR the screen so the printed
    // copy is gone, then paste — the only way the marker can reappear is via the
    // paste reaching the shell (which echoes it onto the input line).
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("paste".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Print a unique marker on its own line.
    c.send(&ClientMsg::Input(b"printf 'YANKME_ZZ\\n'\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);
    // Enter copy-mode, search onto the marker row, select the whole line (Home,
    // start selection, End), and yank (which exits copy-mode).
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"?YANKME\r".to_vec()));
    c.send(&ClientMsg::Input(b"\x1b[H".to_vec())); // Home
    c.send(&ClientMsg::Input(b" ".to_vec())); // Space starts selection
    c.send(&ClientMsg::Input(b"\x1b[F".to_vec())); // End
    c.send(&ClientMsg::Input(b"y".to_vec())); // yank + exit copy-mode
    c.collect_until(Duration::from_secs(1), |_| false);
    // Wipe the screen so the printed marker is gone from the live view.
    c.send(&ClientMsg::Input(b"clear\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    let (_d, cleared) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        !cleared.contains("YANKME_ZZ"),
        "precondition: clear should wipe the printed marker; got:\n{cleared}"
    );
    // Paste the buffer (prefix ]). The shell receives "YANKME_ZZ" and echoes it
    // onto the now-empty input line.
    c.send(&ClientMsg::Input(vec![0x02, b']']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("YANKME_ZZ"),
        "paste should inject the yanked marker into the pane; got:\n{vt}"
    );
}

#[test]
fn buffer_chooser_lists_a_yanked_buffer() {
    // After a yank, prefix `=` opens the paste-buffer chooser, which lists the
    // buffer with a preview of its text.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("bufchoose".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(b"printf 'CHOOSEME_QQ\\n'\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"?CHOOSEME\r".to_vec()));
    c.send(&ClientMsg::Input(b"\x1b[H".to_vec()));
    c.send(&ClientMsg::Input(b" ".to_vec()));
    c.send(&ClientMsg::Input(b"\x1b[F".to_vec()));
    c.send(&ClientMsg::Input(b"y".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Open the buffer chooser (prefix =).
    c.send(&ClientMsg::Input(vec![0x02, b'=']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_done, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("-- BUFFERS --"),
        "prefix = should open the buffer chooser; got:\n{vt}"
    );
    assert!(
        vt.contains("CHOOSEME_QQ"),
        "the chooser should preview the yanked buffer; got:\n{vt}"
    );
}

#[test]
fn break_pane_creates_a_new_window() {
    // tmux break-pane (prefix !): with two panes in one window, breaking the
    // active pane out leaves two windows (the source + the new one). We observe
    // the window list in the status bar growing from one entry to two.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("brk".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // One window so far: the status list has "0:" but no "1:".
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        before.contains("0:") && !before.contains("1:"),
        "precondition: exactly one window; got:\n{before}"
    );
    // Split into two panes (Ctrl-b %), then break the active pane out (Ctrl-b !).
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'!']));
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d1, after) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        after.contains("0:") && after.contains("1:"),
        "break-pane should create a second window; got:\n{after}"
    );
}

#[test]
fn display_panes_shows_numbers_and_picks_a_pane() {
    // tmux display-panes (prefix q): overlays a number on each pane; pressing the
    // digit focuses that pane. With two panes, the overlay shows "0" and "1"; the
    // right (new) pane is active so it's marked. Picking 0 focuses the left pane —
    // we then type a marker and confirm it lands in the LEFT pane (column 0-ish),
    // proving focus actually moved.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("dp".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Two side-by-side panes (Ctrl-b %); the new right pane is active.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Show pane numbers (Ctrl-b q) and force a full repaint.
    c.send(&ClientMsg::Input(vec![0x02, b'q']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, overlay) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        overlay.contains(" 0 ") && overlay.contains('1'),
        "display-panes should overlay pane numbers; got:\n{overlay}"
    );
    // Pick pane 0 (the left pane), then type a unique marker.
    c.send(&ClientMsg::Input(b"0".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"DPFOCUS_LL".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d2, after) = c.collect_until(Duration::from_secs(2), |_| false);
    // The marker echoes in the left pane: its column is in the left half.
    let col = column_of(&after, "DPFOCUS_LL").expect("marker should echo after focusing pane 0");
    assert!(
        col < 40,
        "picking pane 0 should focus the LEFT pane (marker col {col} should be < 40); got:\n{after}"
    );
}

#[test]
fn swap_pane_exchanges_pane_positions() {
    // tmux swap-pane (prefix {/}): swapping exchanges two panes' on-screen
    // positions. We mark the left and right panes, record which screen column
    // each marker sits in, then swap and assert the markers traded sides (left
    // marker moved right and vice-versa) — proving the swap actually happened,
    // not just that both panes survived.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("swp".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Left pane prints LEFT_PANE_AA.
    c.send(&ClientMsg::Input(b"printf 'LEFT_PANE_AA\\n'\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Split right (Ctrl-b %); the new right pane prints RIGHT_PANE_BB.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"printf 'RIGHT_PANE_BB\\n'\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
    let left_before = column_of(&before, "LEFT_PANE_AA").expect("left marker visible");
    let right_before = column_of(&before, "RIGHT_PANE_BB").expect("right marker visible");
    assert!(
        left_before < right_before,
        "precondition: LEFT marker should start left of RIGHT; got {left_before} vs {right_before}\n{before}"
    );
    // Swap with the previous pane (Ctrl-b {).
    c.send(&ClientMsg::Input(vec![0x02, b'{']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, after) = c.collect_until(Duration::from_secs(2), |_| false);
    let left_after = column_of(&after, "LEFT_PANE_AA").expect("left marker still visible");
    let right_after = column_of(&after, "RIGHT_PANE_BB").expect("right marker still visible");
    assert!(
        left_after > right_after,
        "swap should trade the panes' sides (LEFT now right of RIGHT); got {left_after} vs {right_after}\n{after}"
    );
}

#[test]
fn swap_window_reorders_the_status_list() {
    // tmux swap-window (lumux: prefix < / >): moving the active window changes its
    // position in the status-bar window list. We create three named windows, move
    // the active (last) one left, and assert its name now appears before the
    // window it swapped with in the rendered status row.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("mw".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Rename window 0, then create two more with distinct names.
    c.send(&ClientMsg::Input(vec![0x02, b',']));
    c.send(&ClientMsg::Input(b"AAA\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.send(&ClientMsg::Input(vec![0x02, b',']));
    c.send(&ClientMsg::Input(b"BBB\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.send(&ClientMsg::Input(vec![0x02, b',']));
    c.send(&ClientMsg::Input(b"CCC\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Order is AAA, BBB, CCC with CCC active. Capture the status row.
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
    let (a0, b0, cc0) = (
        before.find("AAA"),
        before.find("BBB"),
        before.find("CCC"),
    );
    assert!(
        a0 < b0 && b0 < cc0,
        "precondition: status order AAA<BBB<CCC; got:\n{before}"
    );
    // Move the active window (CCC) left → order becomes AAA, CCC, BBB.
    c.send(&ClientMsg::Input(vec![0x02, b'<']));
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d1, after) = c.collect_until(Duration::from_secs(2), |_| false);
    let (a1, b1, cc1) = (after.find("AAA"), after.find("BBB"), after.find("CCC"));
    assert!(
        a1 < cc1 && cc1 < b1,
        "after moving left, CCC should sit before BBB; got:\n{after}"
    );
}

#[test]
fn synchronize_panes_broadcasts_input_to_all_panes() {
    // tmux synchronize-panes (lumux: prefix S): with sync on, a keystroke goes to
    // every pane in the window, not just the active one. Both panes run `cat -v`
    // (echoes input). After enabling sync and typing a marker, the marker must
    // appear TWICE on screen — once echoed by each pane.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("sync".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Left pane runs cat -v.
    c.send(&ClientMsg::Input(b"cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Split right; the new pane also runs cat -v.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Enable synchronize-panes (Ctrl-b S).
    c.send(&ClientMsg::Input(vec![0x02, b'S']));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Type a marker; with sync on, BOTH cat -v panes echo it.
    c.send(&ClientMsg::Input(b"SYNCED_QQ\n".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    // The marker must appear in BOTH panes: one occurrence in the left half
    // (col < 40) and one in the right half (col >= 40). A non-synced write would
    // only reach the active (right) pane, leaving the left half without it.
    let cols = columns_of(&vt, "SYNCED_QQ");
    let in_left = cols.iter().any(|&c| c < 40);
    let in_right = cols.iter().any(|&c| c >= 40);
    assert!(
        in_left && in_right,
        "sync should echo the marker in BOTH panes (cols {cols:?}); got:\n{vt}"
    );
}

#[test]
fn find_window_switches_to_named_window() {
    // tmux find-window (prefix f): type a query, jump to the matching window. We
    // make windows AAA/BBB/CCC (CCC active), then find "AAA" and confirm the
    // active-window marker (*) moves onto AAA in the status list.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("fw".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b',']));
    c.send(&ClientMsg::Input(b"AAA\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.send(&ClientMsg::Input(vec![0x02, b',']));
    c.send(&ClientMsg::Input(b"CCC\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // CCC is active. Find-window for AAA.
    c.send(&ClientMsg::Input(vec![0x02, b'f']));
    c.send(&ClientMsg::Input(b"AAA\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    // The active marker (*) should now sit on the AAA entry, not CCC. The window
    // list renders "<idx>:<name>" with the active one marked, e.g. "0:AAA*".
    assert!(
        vt.contains("AAA*") || vt.contains("AAA *"),
        "find-window should switch to AAA (marked active); got:\n{vt}"
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
    assert_eq!(cfg.pane_active_border_fg, "green");
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
