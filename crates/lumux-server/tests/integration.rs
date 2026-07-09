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
use lumux_core::proto::{decode, encode, ClientMsg, Event, ServerMsg, WireSize};

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

/// Like [`start_daemon`] but with a custom `status_right` format, for testing
/// status-bar tokens (e.g. the `#{?client_prefix,…}` conditional).
fn start_daemon_status_right(fmt: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(5_000_000);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lumux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    let cfg = lumux_core::config::Config {
        status_right: fmt.to_string(),
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

/// Like [`start_daemon`] but seeded with tmux-syntax config text (for testing
/// user bindings end-to-end).
fn start_daemon_with_tmux(conf: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(7_000_000);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lumux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    let cfg = lumux_core::config::Config::from_tmux(conf).expect("config parses");
    std::thread::spawn(move || {
        let _ = lumux_server::run_with_config(UnixPtySystem, listener, cfg);
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    path
}

/// Like [`start_daemon`] but with remain-on-exit enabled, so a pane whose child
/// exits stays on screen (dead) instead of cascade-closing.
fn start_daemon_remain() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(2_000_000);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lumux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    let cfg = lumux_core::config::Config {
        remain_on_exit: true,
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

/// Like [`start_daemon`] but with emacs copy-mode keys (`mode-keys emacs`).
fn start_daemon_emacs() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(3_000_000);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lumux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    let cfg = lumux_core::config::Config {
        mode_keys: "emacs".to_string(),
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

/// Daemon with remain-on-exit on AND a `pane-exited` hook that respawns the
/// pane — so a shell that exits is automatically restarted.
fn start_daemon_respawn_hook() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(4_000_000);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lumux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    let mut hooks = std::collections::BTreeMap::new();
    hooks.insert("pane-exited".to_string(), "respawn-pane".to_string());
    let cfg = lumux_core::config::Config {
        remain_on_exit: true,
        hooks,
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

/// A persistence-enabled daemon on a caller-chosen socket + state path. Two
/// calls with the SAME `state` path but DIFFERENT sockets simulate a daemon
/// restart: the first saves, the second restores from the shared state file.
fn start_daemon_persist(sock: &std::path::Path, state: &std::path::Path) {
    let listener = UnixSocketListener::bind(sock).expect("bind");
    let cfg = lumux_core::config::Config {
        persist: true,
        ..Default::default()
    };
    let state = state.to_path_buf();
    std::thread::spawn(move || {
        let _ = lumux_server::run_with_config_at(UnixPtySystem, listener, cfg, state);
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
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

/// Whether the rendered status bar lists window number `n`. Window entries
/// render as `<n>:<name>` where the name starts with a letter (the shell
/// basename), so we match `"<n>:"` followed by an ASCII letter. This avoids the
/// status-bar clock (`HH:MM`, a digit after the colon) masquerading as a window
/// entry — a real bug that made `contains("1:")` flaky at times like 11:13.
fn has_window(vt: &str, n: u32) -> bool {
    let needle = format!("{n}:");
    let bytes = vt.as_bytes();
    let mut from = 0;
    while let Some(rel) = vt[from..].find(&needle) {
        let after = from + rel + needle.len();
        if bytes.get(after).is_some_and(|b| b.is_ascii_alphabetic()) {
            return true;
        }
        from += rel + 1;
    }
    false
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
        has_window(&vt, 0),
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
fn mouse_drag_selects_and_copies_text() {
    // tmux drag-to-copy: left-press over a pane, drag across text, release — the
    // selection is copied to the clipboard. We prove the copy by capturing the
    // OSC-52 set-clipboard frame the daemon emits on release and decoding its
    // base64 payload back to the selected text.
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("dragcopy".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Print a unique marker so we can look for it in the copied text.
    c.send(&ClientMsg::Input(b"echo DRAGCOPY_MARKER_XZ\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);

    // Left-press at the top-left, drag across a wide swath of the screen (which
    // enters copy-mode and extends the selection), then release. SGR button
    // codes: 0 = left press/release, 32 = left-drag (motion bit set). Coords are
    // 1-based on the wire.
    c.send(&ClientMsg::Input(b"\x1b[<0;1;1M".to_vec())); // press (1,1)
    c.collect_until(Duration::from_millis(100), |_| false);
    c.send(&ClientMsg::Input(b"\x1b[<32;80;20M".to_vec())); // drag to (80,20)
    c.collect_until(Duration::from_millis(100), |_| false);

    // The release frame carries the OSC-52 clipboard set. Collect until we see a
    // frame containing the OSC-52 introducer.
    c.send(&ClientMsg::Input(b"\x1b[<0;80;20m".to_vec())); // release (80,20)
    let (saw_osc, vt) = c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Frame(b) if find_osc52(b).is_some())
    });
    assert!(saw_osc, "release must emit an OSC-52 clipboard frame; got:\n{vt}");

    // Decode the payload and confirm the marker text was actually copied.
    let payload = osc52_payload(&vt).expect("OSC-52 payload in captured frames");
    let text = String::from_utf8(base64_decode(&payload).expect("valid base64"))
        .expect("utf8 clipboard text");
    assert!(
        text.contains("DRAGCOPY_MARKER_XZ"),
        "copied text should contain the dragged marker; got:\n{text:?}"
    );
}

#[test]
fn copy_command_pipes_the_selection_on_yank() {
    // tmux copy-pipe integration: with `set -s copy-command <cmd>`, a copy-mode
    // yank pipes the selection to that command's stdin. We set copy-command to
    // write the selection to a temp file, drag-copy a marker, then read the file.
    let out = std::env::temp_dir().join(format!("lumux-copypipe-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let conf = format!(
        "set -g mouse on\nset -s copy-command \"cat > {}\"",
        out.display()
    );
    let path = start_daemon_with_tmux(&conf);
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("copypipe".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(b"echo COPYPIPE_MARKER_QZ\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Drag-copy a wide region (press, drag, release) — this yanks, which pipes.
    c.send(&ClientMsg::Input(b"\x1b[<0;1;1M".to_vec()));
    c.collect_until(Duration::from_millis(100), |_| false);
    c.send(&ClientMsg::Input(b"\x1b[<32;80;20M".to_vec()));
    c.collect_until(Duration::from_millis(100), |_| false);
    c.send(&ClientMsg::Input(b"\x1b[<0;80;20m".to_vec())); // release -> yank -> pipe
    c.collect_until(Duration::from_secs(1), |_| false);
    // Give the piped child a moment to write, then read the file.
    std::thread::sleep(Duration::from_millis(300));
    let piped = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    assert!(
        piped.contains("COPYPIPE_MARKER_QZ"),
        "copy-command should have received the selection on yank; got:\n{piped:?}"
    );
}

#[test]
fn mouse_drag_release_split_across_reads_still_copies() {
    // Regression: over SSH/mosh a mouse report can arrive split across reads. A
    // release `ESC [ < 0 ; x ; y m` split *inside its `ESC [ <` introducer* used
    // to be dropped (the bytes leaked as text), so `mouse_sel_finish` never ran
    // and nothing was copied — even though the drag highlight looked fine. The
    // event loop now holds a trailing partial introducer while a drag is live.
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("dragsplit".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(b"echo SPLITCOPY_MARKER_QY\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);

    // Press + drag as whole reports, then deliver the RELEASE split at the worst
    // boundary — a bare ESC, then the rest — as two separate inputs.
    c.send(&ClientMsg::Input(b"\x1b[<0;1;1M".to_vec())); // press (1,1)
    c.collect_until(Duration::from_millis(100), |_| false);
    c.send(&ClientMsg::Input(b"\x1b[<32;80;20M".to_vec())); // drag to (80,20)
    c.collect_until(Duration::from_millis(100), |_| false);
    c.send(&ClientMsg::Input(b"\x1b".to_vec())); // release, part 1: bare ESC
    c.collect_until(Duration::from_millis(100), |_| false);
    c.send(&ClientMsg::Input(b"[<0;80;20m".to_vec())); // release, part 2

    let (saw_osc, vt) = c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Frame(b) if find_osc52(b).is_some())
    });
    assert!(
        saw_osc,
        "a release split across reads must still emit the OSC-52 frame; got:\n{vt}"
    );
    let payload = osc52_payload(&vt).expect("OSC-52 payload in captured frames");
    let text = String::from_utf8(base64_decode(&payload).expect("valid base64"))
        .expect("utf8 clipboard text");
    assert!(
        text.contains("SPLITCOPY_MARKER_QY"),
        "copied text should contain the dragged marker; got:\n{text:?}"
    );
}

/// Locate an OSC-52 set-clipboard sequence (`ESC ] 52 ; c ; <b64> BEL`) in raw
/// frame bytes, returning the base64 payload. Used by the drag-copy test.
fn find_osc52(bytes: &[u8]) -> Option<String> {
    osc52_payload(&String::from_utf8_lossy(bytes))
}

/// A program inside a pane copies to the clipboard via OSC 52 (as Claude Code
/// does with its own mouse selection). lumux must forward that sequence to the
/// client's terminal so the user's local clipboard is set — tmux set-clipboard.
#[test]
fn app_osc52_clipboard_is_forwarded_to_client() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("appclip".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Emit an OSC 52 set-clipboard from inside the pane. printf writes it to the
    // pane's PTY exactly as a TUI would. Q0xJUF9YWg== is base64 of "CLIP_XZ".
    c.send(&ClientMsg::Input(
        b"printf '\\033]52;c;Q0xJUF9YWg==\\007'\n".to_vec(),
    ));
    let (saw_osc, vt) = c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Frame(b) if find_osc52(b).is_some())
    });
    assert!(
        saw_osc,
        "app OSC 52 must be forwarded to the client as a frame; got:\n{vt}"
    );
    let payload = osc52_payload(&vt).expect("OSC-52 payload in captured frames");
    let text = String::from_utf8(base64_decode(&payload).expect("valid base64"))
        .expect("utf8 clipboard text");
    assert!(
        text.contains("CLIP_XZ"),
        "forwarded clipboard text should be what the app copied; got:\n{text:?}"
    );
}

/// Extract the base64 payload of the first OSC-52 sequence in a (lossy) string.
fn osc52_payload(s: &str) -> Option<String> {
    let start = s.find("\x1b]52;")?;
    // After the introducer: "52;<selection>;<b64>BEL". Skip to the 2nd ';'.
    let rest = &s[start + 2..]; // past ESC ]
    let semi1 = rest.find(';')?;
    let after_sel = &rest[semi1 + 1..];
    let semi2 = after_sel.find(';')?;
    let payload = &after_sel[semi2 + 1..];
    let end = payload.find('\x07')?; // BEL terminator
    Some(payload[..end].to_string())
}

/// Minimal standard-base64 decoder (the crate hand-rolls the encoder to avoid a
/// dependency; the test hand-rolls the matching decoder for the same reason).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in s.chunks(4) {
        let mut acc = 0u32;
        let mut pad = 0;
        for (k, &ch) in chunk.iter().enumerate() {
            if ch == b'=' {
                pad += 1;
                acc <<= 6;
            } else {
                acc = (acc << 6) | val(ch)?;
            }
            let _ = k;
        }
        // Missing chars (short final chunk) count as padding.
        for _ in chunk.len()..4 {
            acc <<= 6;
            pad += 1;
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
    }
    Some(out)
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
fn clicking_a_mouse_aware_pane_focuses_it() {
    // Regression: clicking a pane that runs a mouse-aware app (Claude Code / vim)
    // forwarded the click to the app but never switched lumux's focus to that
    // pane — so you couldn't select it by clicking. A press must select the pane
    // first (tmux behavior), then forward. Both panes run `cat -v` (echo input).
    // We focus the LEFT pane by keyboard, click the RIGHT pane, then type a plain
    // marker: it must echo in the RIGHT pane (proving focus moved there).
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("clickfocus".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Left pane: mouse-aware cat -v.
    c.send(&ClientMsg::Input(b"printf '\\033[?1002h\\033[?1006h'; cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Split right; the new right pane (active) also runs mouse-aware cat -v.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"printf '\\033[?1002h\\033[?1006h'; cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Focus the LEFT pane with the keyboard (Ctrl-b Left).
    c.send(&ClientMsg::Input(vec![0x02]));
    c.send(&ClientMsg::Input(b"\x1b[D".to_vec())); // Left arrow
    c.collect_until(Duration::from_secs(1), |_| false);
    // Click the RIGHT pane (column 60 is in the right half of an 80-col split).
    c.send(&ClientMsg::Input(b"\x1b[<0;60;5M".to_vec())); // left-button press
    c.send(&ClientMsg::Input(b"\x1b[<0;60;5m".to_vec())); // release
    c.collect_until(Duration::from_secs(1), |_| false);
    // Now type a plain marker; it must reach the RIGHT pane (now focused).
    c.send(&ClientMsg::Input(b"RIGHTFOCUS_KK".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    let cols = columns_of(&vt, "RIGHTFOCUS_KK");
    assert!(
        cols.iter().any(|&col| col >= 40),
        "after clicking the right pane, typed input must land there (col >= 40); got cols {cols:?}\n{vt}"
    );
    assert!(
        !cols.iter().any(|&col| col < 40),
        "input must NOT land in the left pane after clicking right; got cols {cols:?}\n{vt}"
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
    // One window so far: the status list has "0:sh" but no "1:sh". (Match the
    // full "N:sh" entry, not bare "N:", so the status-bar clock like "11:13"
    // can't masquerade as a window entry.)
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        has_window(&before, 0) && !has_window(&before, 1),
        "precondition: exactly one window; got:\n{before}"
    );
    // Split into two panes (Ctrl-b %), then break the active pane out (Ctrl-b !).
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'!']));
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d1, after) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        has_window(&after, 0) && has_window(&after, 1),
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
fn keymap_and_command_prompt_new_window_agree() {
    // Commit-2 consolidation guard: the keymap `prefix c` and the `:new-window`
    // command must go through the same executor and produce identical state — a
    // new window either way. Drives both on one session and asserts three
    // windows result (the initial 0, plus one from each surface).
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("parity".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Keymap surface: prefix c.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_millis(300), |_| false);
    // Command surface: :new-window.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"new-window\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        has_window(&vt, 0) && has_window(&vt, 1) && has_window(&vt, 2),
        "keymap and command-prompt new-window should both create a window; got:\n{vt}"
    );
}

#[test]
fn select_layout_by_name_rearranges_panes() {
    // `select-layout NAME` must apply that exact preset (not just cycle). Split
    // left/right (a vertical divider `│`), then select even-vertical, which
    // stacks the panes top/bottom — so the vertical divider becomes a horizontal
    // one (`─`). This proves the named layout was applied, not a cycle step.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("layoutname".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b'%'])); // split left/right
    let (_d0, split) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        split.contains('│'),
        "precondition: a left/right split draws a vertical divider; got:\n{split}"
    );
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"select-layout even-vertical\r".to_vec()));
    c.collect_until(Duration::from_millis(400), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, all) = c.collect_until(Duration::from_secs(2), |_| false);
    let after = all.rsplit("\u{1b}[2J").next().unwrap_or(&all);
    assert!(
        after.contains('─') && !after.contains('│'),
        "even-vertical should stack panes (horizontal divider, no vertical); got:\n{after}"
    );
}

#[test]
fn previous_layout_undoes_next_layout() {
    // previous-layout must cycle backward, undoing a next-layout step. Start at
    // even-horizontal (vertical divider), next-layout moves to even-vertical
    // (horizontal divider, per the CYCLE order), previous-layout should return
    // to even-horizontal (vertical divider again).
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("prevlayout".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b'%'])); // split left/right
    c.collect_until(Duration::from_millis(300), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"select-layout even-horizontal\r".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);

    // next-layout: even-horizontal -> even-vertical. next-layout is a keymap
    // action (prefix Space), not a `:` command, so drive it via the keybinding.
    c.send(&ClientMsg::Input(vec![0x02, b' ']));
    c.collect_until(Duration::from_millis(400), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, mid_all) = c.collect_until(Duration::from_secs(2), |_| false);
    let mid = mid_all.rsplit("\u{1b}[2J").next().unwrap_or(&mid_all);
    assert!(
        mid.contains('─') && !mid.contains('│'),
        "next-layout should move to even-vertical; got:\n{mid}"
    );

    // previous-layout: even-vertical -> back to even-horizontal.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"previous-layout\r".to_vec()));
    c.collect_until(Duration::from_millis(400), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, after_all) = c.collect_until(Duration::from_secs(2), |_| false);
    let after = after_all.rsplit("\u{1b}[2J").next().unwrap_or(&after_all);
    assert!(
        after.contains('│') && !after.contains('─'),
        "previous-layout should undo next-layout, back to even-horizontal; got:\n{after}"
    );
}

#[test]
fn send_keys_injects_text_into_the_active_pane() {
    // `send-keys` translates key names: `-l "text"` sends the literal string and
    // `Enter` sends a carriage return. A chained line runs the command in the
    // pane. We confirm the echoed marker appears.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("sendkeys".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Literal text, then a separate send-keys Enter to run it (chained with `;`).
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(
        b"send-keys -l \"echo SENDKEYS_OK_QW\" ; send-keys Enter\r".to_vec(),
    ));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("SENDKEYS_OK_QW"),
        "send-keys should inject text (via -l) and run it (via Enter); got:\n{vt}"
    );
}

#[test]
fn kill_pane_target_kills_the_indexed_pane_not_the_active_one() {
    // `kill-pane -t .N` must kill pane N, not the active pane. Split into two
    // panes (left = pane 0 with a unique marker; the new right pane = pane 1 and
    // is active). `:kill-pane -t .0` kills the LEFT pane: its marker disappears
    // and the split collapses (no divider) — proving the target, not the active
    // pane (pane 1), was killed.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("killtarget".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Print a marker in the left pane (pane 0), then split left/right so a new
    // active pane 1 appears on the right.
    c.send(&ClientMsg::Input(b"echo KILLTARGET_LEFT_QZ\n".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'%'])); // split left/right
    let (_d0, split) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        split.contains('│') && split.contains("KILLTARGET_LEFT_QZ"),
        "precondition: two panes with the left marker visible; got:\n{split}"
    );
    // Kill pane 0 (the left one) by index, while pane 1 (right) is active.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"kill-pane -t .0\r".to_vec()));
    // Drain the kill + prompt-teardown frames, THEN force one clean full repaint
    // so the assertion sees only the final single-pane state (collect_until
    // accumulates every frame, including the prompt overlay drawn over the old
    // split).
    c.collect_until(Duration::from_millis(500), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, all) = c.collect_until(Duration::from_secs(2), |_| false);
    // Assert on the final repaint only: the resize emits ESC[2J then redraws, so
    // take everything after the last clear-screen (collect_until accumulates all
    // frames, including the pre-kill split still under the prompt overlay).
    let after = all.rsplit("\u{1b}[2J").next().unwrap_or(&all);
    assert!(
        !after.contains('│'),
        "killing one of two panes should remove the divider; got:\n{after}"
    );
    assert!(
        !after.contains("KILLTARGET_LEFT_QZ"),
        "the LEFT pane (pane 0) should be gone, not the active right pane; got:\n{after}"
    );
}

#[test]
fn new_session_switches_the_client_by_default() {
    // `:new-session -s NAME` (no -d) creates a session and switches the issuing
    // client to it — the status bar's #S segment should now show the new name.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("orig".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"new-session -s work\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("work"),
        "new-session (no -d) should switch the client to the new session; got:\n{vt}"
    );
}

#[test]
fn new_session_detached_does_not_switch() {
    // `-d` creates the session in the background; the current client stays put.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("orig2".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"new-session -s bg2 -d\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("orig2") && !vt.contains("bg2"),
        "new-session -d must NOT switch the client; got:\n{vt}"
    );
}

#[test]
fn switch_client_and_kill_session_by_name() {
    // Create a second (detached) session, switch to it, switch back, then kill
    // the second by name and confirm switch-client to it now fails (gone).
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("home".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"new-session -s other -d\r".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);

    // Switch to "other".
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"switch-client -t other\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(vt.contains("other"), "switch-client should move to 'other'; got:\n{vt}");

    // Switch back to "home".
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"switch-client -t home\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, vt2) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(vt2.contains("home"), "switch-client should move back to 'home'; got:\n{vt2}");

    // kill-session -t other (not the current session); the client stays alive.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"kill-session -t other\r".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);

    // Trying to switch to it now must fail — it no longer exists. The failure
    // flashes a message on the status row (replacing it), so check that first.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"switch-client -t other\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d2, vt3) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt3.contains("no such session"),
        "killed session must be gone, so switch-client to it fails; got:\n{vt3}"
    );
    // Dismiss the flash (any input clears it) and confirm we're still on "home".
    c.send(&ClientMsg::Input(b" \x08".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d3, vt4) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt4.contains("home"),
        "the client should still be on 'home' after the failed switch; got:\n{vt4}"
    );
}

#[test]
fn kill_server_detaches_every_client() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("last".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"kill-server\r".to_vec()));
    let (detached, _) = c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Detached));
    assert!(detached, "kill-server must detach the client");
}

#[test]
fn resize_pane_honors_the_cell_amount() {
    // `resize-pane -R N` must move the divider by roughly N cells, not the fixed
    // ~5%-of-window nudge the interactive Ctrl/Alt-arrow bindings use. Compare
    // the divider's column after a bare resize (fixed step) against a fresh
    // split resized with an explicit large N — the explicit one should move the
    // divider much further right.
    let bare_delta = {
        let path = start_daemon();
        let mut c = TestClient::connect(&path);
        c.send(&ClientMsg::NewSession {
            name: Some("rpbare".into()),
            shell: Some("/bin/sh".into()),
            size: size(),
        });
        c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
        c.send(&ClientMsg::Input(vec![0x02, b'%'])); // split left/right
        c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
        let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
        let before_col = column_of(&before, "│").expect("divider before resize");
        c.send(&ClientMsg::Input(vec![0x02, b':']));
        c.send(&ClientMsg::Input(b"resize-pane -R\r".to_vec()));
        c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
        let (_d1, all) = c.collect_until(Duration::from_secs(2), |_| false);
        // collect_until accumulates every frame, including the pre-resize one
        // (still under the `:` prompt overlay); take only the final repaint.
        let after = all.rsplit("\u{1b}[2J").next().unwrap_or(&all);
        let after_col = column_of(after, "│").expect("divider after bare resize");
        after_col.abs_diff(before_col)
    };

    let amount_delta = {
        let path = start_daemon();
        let mut c = TestClient::connect(&path);
        c.send(&ClientMsg::NewSession {
            name: Some("rpamount".into()),
            shell: Some("/bin/sh".into()),
            size: size(),
        });
        c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
        c.send(&ClientMsg::Input(vec![0x02, b'%']));
        c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
        let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
        let before_col = column_of(&before, "│").expect("divider before resize");
        c.send(&ClientMsg::Input(vec![0x02, b':']));
        c.send(&ClientMsg::Input(b"resize-pane -R 20\r".to_vec()));
        c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
        let (_d1, all) = c.collect_until(Duration::from_secs(2), |_| false);
        let after = all.rsplit("\u{1b}[2J").next().unwrap_or(&all);
        let after_col = column_of(after, "│").expect("divider after amount resize");
        after_col.abs_diff(before_col)
    };

    assert!(
        amount_delta > bare_delta * 2,
        "resize-pane -R 20 should move the divider much further than the bare \
         (fixed-step) resize; bare_delta={bare_delta} amount_delta={amount_delta}"
    );
    // And the explicit amount should be roughly in the right ballpark (allow
    // slack for the 1-cell divider + rounding), not wildly off.
    assert!(
        (15..=25).contains(&amount_delta),
        "resize-pane -R 20 should move the divider by ~20 cells; got {amount_delta}"
    );
}

#[test]
fn dragging_the_divider_over_a_mouse_aware_pane_still_resizes() {
    // Regression: grabbing the split divider and dragging the pointer into an
    // adjacent pane that runs a mouse-aware app (vim / htop / Claude Code) used
    // to forward the drag to that app, so the divider stopped tracking after a
    // single cell — the classic "can't drag the separator". Once a divider is
    // grabbed the whole gesture belongs to lumux's resize and must NOT be
    // forwarded. Both panes run mouse-aware `cat -v` (echoes what they receive),
    // so a forwarded drag would both freeze the divider AND show up as the raw
    // SGR bytes in the pane.
    let path = start_daemon_mouse();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("dividerdrag".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Left (first) pane: mouse-aware cat -v.
    c.send(&ClientMsg::Input(b"printf '\\033[?1002h\\033[?1006h'; cat -v\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Split left/right; the new right pane (active) is also mouse-aware.
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"printf '\\033[?1002h\\033[?1006h'; cat -v\n".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
    let before_frame = before.rsplit("\u{1b}[2J").next().unwrap_or(&before);
    let before_col = column_of(before_frame, "│").expect("divider before drag");

    // Press exactly on the divider (1-based col == before_col), then drag the
    // pointer far LEFT — deep inside the left, mouse-aware pane — and release.
    // 0x20 is the motion bit (drag); button 0 = left.
    let press = format!("\x1b[<0;{};12M", before_col);
    let drag = "\x1b[<32;12;12M".to_string(); // drag to 1-based col 12
    let release = "\x1b[<0;12;12m".to_string();
    c.send(&ClientMsg::Input(press.into_bytes()));
    c.send(&ClientMsg::Input(drag.into_bytes()));
    c.send(&ClientMsg::Input(release.into_bytes()));
    let (_d1, after) = c.collect_until(Duration::from_secs(1), |_| false);
    let after_frame = after.rsplit("\u{1b}[2J").next().unwrap_or(&after);
    let after_col = column_of(after_frame, "│").expect("divider after drag");

    assert!(
        before_col.saturating_sub(after_col) >= 10,
        "dragging the divider left should move it well left even though the drag \
         passes over a mouse-aware pane; before={before_col} after={after_col}"
    );
    // And the drag must not have leaked to the pane's app as a raw SGR event
    // (cat -v renders ESC as ^[, so a forwarded drag would show `^[[<32;`).
    assert!(
        !after.contains("[<32;"),
        "a grabbed-divider drag must not be forwarded to the pane's app; got:\n{after}"
    );
}

#[test]
fn clock_mode_shows_big_digits_and_any_key_closes_it() {
    // prefix t opens a full-screen overlay with the time in a large block font;
    // any key closes it back to the live pane view.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("clockmode".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b't']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("CLOCK") && vt.contains('█'),
        "clock-mode should show its status line and block-digit art; got:\n{vt}"
    );
    // Any key closes it — back to the normal session view (name in status bar).
    c.send(&ClientMsg::Input(b"x".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, all) = c.collect_until(Duration::from_secs(2), |_| false);
    let after = all.rsplit("\u{1b}[2J").next().unwrap_or(&all);
    assert!(
        after.contains("clockmode") && !after.contains("CLOCK"),
        "any key should close clock-mode, back to the live view; got:\n{after}"
    );
}

#[test]
fn repeatable_bind_fires_again_without_the_prefix() {
    // tmux `bind -r`: after the bound key fires once (with the prefix), the SAME
    // key fires again on its own within the repeat window — no re-pressing the
    // prefix. Bind `-r k` to new-window; press prefix+k (1 new window), then k
    // alone (a 2nd new window), landing at 3 windows total (0 initial + 2 new).
    let path = start_daemon_with_tmux("bind -r k new-window");
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("repeatbind".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Prefix + k: first new-window.
    c.send(&ClientMsg::Input(vec![0x02, b'k']));
    c.collect_until(Duration::from_millis(200), |_| false);
    // k alone (no prefix): repeats within the repeat window.
    c.send(&ClientMsg::Input(vec![b'k']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        has_window(&vt, 0) && has_window(&vt, 1) && has_window(&vt, 2),
        "a repeatable bind should fire again on the bare key; got:\n{vt}"
    );
}

#[test]
fn bound_key_runs_a_command_chain() {
    // A config `bind` with a `\;` chain must run every command with its real
    // args. Bind `prefix g` to `new-window \; split-window -h`; pressing it once
    // should create a new window AND split it (a vertical divider appears).
    let path = start_daemon_with_tmux("bind g new-window \\; split-window -h");
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("boundchain".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Press the prefix (Ctrl-b) then g.
    c.send(&ClientMsg::Input(vec![0x02, b'g']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        has_window(&vt, 0) && has_window(&vt, 1),
        "the chain's new-window must have created a second window; got:\n{vt}"
    );
    assert!(
        vt.contains('│'),
        "the chain's split-window -h must have drawn a vertical divider; got:\n{vt}"
    );
}

#[test]
fn command_prompt_runs_a_split() {
    // tmux command-prompt (prefix :): typing "split-window -h" and Enter splits
    // the active window. We confirm a second pane appears (a vertical divider is
    // drawn) where there was none before.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("cmd".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Open the command prompt (Ctrl-b :) and type the split command.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    // The prompt row should echo ":" as we start typing.
    c.send(&ClientMsg::Input(b"split-window -h\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains('│'),
        "command-prompt split-window -h should draw a vertical divider; got:\n{vt}"
    );
}

#[test]
fn command_prompt_runs_a_chain_of_commands() {
    // tmux command-prompt supports `;`-separated command chains. A single line
    // `new-window ; new-window` must create BOTH windows, so a fresh session
    // (one window, index 0) ends up with three (0, 1, 2).
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("chain".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    let (_b, before) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        has_window(&before, 0) && !has_window(&before, 1),
        "precondition: a fresh session has exactly one window; got:\n{before}"
    );
    // One command line, two commands joined by `;`.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"new-window ; new-window\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, after) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        has_window(&after, 0) && has_window(&after, 1) && has_window(&after, 2),
        "a `;`-chained command line must run every command; got:\n{after}"
    );
}

#[test]
fn command_prompt_unknown_command_flashes() {
    // An unrecognized verb flashes a message rather than doing nothing silently.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("cmd2".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"frobnicate\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("unknown command") && vt.contains("frobnicate"),
        "unknown command should flash a message; got:\n{vt}"
    );
}

#[test]
fn command_prompt_join_pane_merges_windows() {
    // tmux join-pane via the command prompt: with two windows, "join-pane -h"
    // pulls the previous window's pane into the active window. The single-pane
    // source window then closes, so the window list shrinks from two to one while
    // the active window gains a divider.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("jp".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Create a second window (now two windows: 0 and 1, with 1 active).
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        has_window(&before, 0) && has_window(&before, 1),
        "precondition: two windows; got:\n{before}"
    );
    // Join the other window's pane into this one (-h side by side).
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"join-pane -h\r".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, after) = c.collect_until(Duration::from_secs(2), |_| false);
    // Source window (single pane) closed → only one window remains; active window
    // now has two panes (a divider).
    assert!(
        has_window(&after, 0) && !has_window(&after, 1),
        "join-pane should close the emptied source window; got:\n{after}"
    );
    assert!(
        after.contains('│'),
        "the active window should now have two panes; got:\n{after}"
    );
}

#[test]
fn named_buffer_set_and_paste() {
    // set-buffer stores text under a name; paste-buffer -b name injects it into
    // the active pane. `cat` echoes what's pasted so we can see it on screen.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("namedbuf".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Store a named buffer, then run cat so pasted bytes echo back.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"set-buffer -b greet NAMEDBUF_QZ\r".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);
    c.send(&ClientMsg::Input(b"cat\n".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);
    // Paste the named buffer into the pane (cat echoes it).
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"paste-buffer -b greet\r".to_vec()));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("NAMEDBUF_QZ"),
        "paste-buffer -b greet should inject the named buffer's text; got:\n{vt}"
    );
}

#[test]
fn rotate_window_moves_pane_content_between_slots() {
    // rotate-window (prefix C-o) rotates panes within the window. Split left/
    // right: left pane (0) prints a marker; the right pane (1, active) is empty.
    // After rotate, the marker that was on the LEFT should now be on the RIGHT
    // (its column position changed), proving the panes rotated.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("rotw".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Left pane prints a marker, then split left/right (marker stays left).
    c.send(&ClientMsg::Input(b"echo ROTMARK_QZ\n".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'%'])); // split left/right
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, split) = c.collect_until(Duration::from_secs(1), |_| false);
    let left_col = columns_of(&split, "ROTMARK_QZ").into_iter().next();
    assert!(
        left_col.is_some_and(|col| col < 40),
        "precondition: the marker starts in the left pane (col < 40); got:\n{split}"
    );
    // Rotate the panes (prefix C-o = Ctrl-o = 0x0f).
    c.send(&ClientMsg::Input(vec![0x02, 0x0f]));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, all) = c.collect_until(Duration::from_secs(2), |_| false);
    let after = all.rsplit("\u{1b}[2J").next().unwrap_or(&all);
    let new_col = columns_of(after, "ROTMARK_QZ").into_iter().next();
    assert!(
        new_col.is_some_and(|col| col >= 40),
        "after rotate, the marker should have moved to the right pane (col >= 40); got:\n{after}"
    );
}

#[test]
fn after_new_window_hook_fires() {
    // set-hook wires a command to an event; creating a window must fire the
    // after-new-window hook. Bind it to display-message and assert the flash
    // shows after `prefix c`.
    let path = start_daemon_with_tmux(
        "set-hook -g after-new-window \"display-message HOOKWINQZ\"",
    );
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("hookwin".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b'c'])); // new window
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("HOOKWINQZ"),
        "after-new-window hook should flash its message; got:\n{vt}"
    );
}

#[test]
fn marked_pane_swaps_across_windows() {
    // Pane marking (prefix m) lets swap-pane exchange panes across windows. Print
    // a marker in window 0's pane, mark it, create window 1, then `:swap-pane`
    // with no target — the marked pane (win 0) and win 1's active pane swap, so
    // win 1 now shows the marker that used to be in win 0.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("markswap".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Window 0: print a unique marker, then mark this pane (prefix m).
    c.send(&ClientMsg::Input(b"echo MARKSWAP_ZERO_QW\n".to_vec()));
    c.collect_until(Duration::from_millis(300), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'm'])); // mark
    c.collect_until(Duration::from_millis(200), |_| false);
    // Window 1: fresh pane with no marker.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_millis(400), |_| false);
    let (_d0, on_w1) = c.collect_until(Duration::from_millis(300), |_| false);
    assert!(
        !on_w1.contains("MARKSWAP_ZERO_QW"),
        "precondition: window 1 shouldn't show window 0's marker; got:\n{on_w1}"
    );
    // Swap the active (win 1) pane with the marked (win 0) pane.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"swap-pane\r".to_vec()));
    c.collect_until(Duration::from_millis(400), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d1, all) = c.collect_until(Duration::from_secs(2), |_| false);
    let after = all.rsplit("\u{1b}[2J").next().unwrap_or(&all);
    assert!(
        after.contains("MARKSWAP_ZERO_QW"),
        "after swap, window 1 should show the marked pane's content; got:\n{after}"
    );
}

#[test]
fn capture_pane_saves_screen_to_a_buffer() {
    // tmux capture-pane: ":capture-pane" copies the pane's visible text into a
    // paste buffer. We print a marker, capture, then open the buffer chooser
    // (prefix =) and confirm the captured marker shows in the preview.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("cap".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(b"printf 'CAPTURED_WW\\n'\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Capture the pane via the command prompt.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"capture-pane\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Open the buffer chooser; the captured text must appear (it includes the
    // marker plus the shell prompt lines).
    c.send(&ClientMsg::Input(vec![0x02, b'=']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("-- BUFFERS --") && vt.contains("CAPTURED_WW"),
        "capture-pane should put the screen text into a buffer; got:\n{vt}"
    );
}

#[test]
fn automatic_rename_follows_osc_title() {
    // tmux automatic-rename: a window's name tracks the active pane's OSC title.
    // Emit OSC 2 from the shell and confirm the new title shows in the status
    // bar's window list (which renders "<idx>:<name>").
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("ar".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Set the window title via OSC 2 (ESC ] 2 ; TITLE BEL).
    c.send(&ClientMsg::Input(b"printf '\\033]2;TITLE_RR\\007'\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Clear the screen so the echoed printf command line is gone; the title can
    // now only appear via the status-bar window list (driven by window state).
    c.send(&ClientMsg::Input(b"clear\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains(":TITLE_RR"),
        "automatic-rename should show the OSC title in the window list; got:\n{vt}"
    );
}

#[test]
fn remain_on_exit_keeps_pane_then_respawns() {
    // tmux remain-on-exit + respawn-pane: when the shell exits, the pane stays
    // (dead) instead of closing the session. ":respawn-pane" restarts a working
    // shell in place — we prove it by running a command that echoes afterward.
    let path = start_daemon_remain();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("ree".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Print a marker, then exit the shell. With remain-on-exit the session must
    // NOT close — the client stays attached (no Detached/SessionClosed).
    c.send(&ClientMsg::Input(b"echo BEFORE_EXIT_PP\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"exit\n".to_vec()));
    // Give the child time to exit and the daemon to mark the pane dead.
    let (closed, _v) = c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Detached | ServerMsg::Event(Event::SessionClosed))
    });
    assert!(
        !closed,
        "remain-on-exit must keep the session alive after the shell exits"
    );
    // Respawn the pane via the command prompt, then run a fresh command.
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"respawn-pane\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"echo AFTER_RESPAWN_PP\n".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("AFTER_RESPAWN_PP"),
        "respawn-pane should restart a working shell; got:\n{vt}"
    );
}

#[test]
fn emacs_mode_keys_scroll_copy_mode() {
    // With mode-keys emacs, copy-mode accepts emacs motion: C-p scrolls up into
    // history. We push a marker off-screen, enter copy-mode, and C-p repeatedly
    // to bring it back — proving the emacs binding drives the scroll.
    let path = start_daemon_emacs();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("em".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(
        b"echo EMACS_HIST_MM; for i in $(seq 1 40); do echo .; done\n".to_vec(),
    ));
    c.collect_until(Duration::from_secs(2), |_| false);
    let (_d0, live) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        !live.contains("EMACS_HIST_MM"),
        "precondition: marker scrolled off; got:\n{live}"
    );
    // Enter copy-mode (Ctrl-b [), then C-p (0x10) many times to scroll up.
    c.send(&ClientMsg::Input(vec![0x02, b'[']));
    c.collect_until(Duration::from_secs(1), |_| false);
    for _ in 0..45 {
        c.send(&ClientMsg::Input(vec![0x10])); // C-p
    }
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("EMACS_HIST_MM"),
        "emacs C-p should scroll the marker back into view; got:\n{vt}"
    );
}

#[test]
fn run_shell_captures_output_to_a_buffer() {
    // tmux run-shell: ":run-shell echo …" runs the command and its output goes
    // into a paste buffer. We run a marker echo, then open the buffer chooser
    // (prefix =) and confirm the marker shows in the preview.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("rsh".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c.send(&ClientMsg::Input(vec![0x02, b':']));
    c.send(&ClientMsg::Input(b"run-shell echo RUNSHELL_OUT_TT\r".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'=']));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("-- BUFFERS --") && vt.contains("RUNSHELL_OUT_TT"),
        "run-shell output should land in a paste buffer; got:\n{vt}"
    );
}

#[test]
fn pane_exited_hook_respawns_the_pane() {
    // tmux set-hook: a `pane-exited` hook of `respawn-pane` (with remain-on-exit)
    // automatically restarts a shell when it exits. We exit the shell, then —
    // without manually respawning — run a fresh command and see it echo, proving
    // the hook respawned a working shell.
    let path = start_daemon_respawn_hook();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("hook".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Exit the shell — the pane-exited hook should respawn it.
    c.send(&ClientMsg::Input(b"exit\n".to_vec()));
    c.collect_until(Duration::from_secs(2), |_| false);
    // Run a fresh command in the (auto-respawned) shell.
    c.send(&ClientMsg::Input(b"echo HOOK_RESPAWN_VV\n".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("HOOK_RESPAWN_VV"),
        "pane-exited hook should respawn a working shell; got:\n{vt}"
    );
}

#[test]
fn persist_save_and_restore_round_trip() {
    // tmux-resurrect: with persistence on, a saved session is rebuilt by a fresh
    // daemon. Daemon #1 creates a session, splits into two panes, renames the
    // window, and :save-state. Daemon #2 (different socket, SAME state file)
    // restores it — the rebuilt session must have the renamed window and two
    // panes (a divider), proving layout+shell restore across a "restart".
    let dir = std::env::temp_dir();
    let uniq = format!("{}-{:?}", std::process::id(), std::thread::current().id());
    let state = dir.join(format!("lumux-state-{uniq}.bin"));
    let sock1 = dir.join(format!("lumux-persist1-{uniq}.sock"));
    let sock2 = dir.join(format!("lumux-persist2-{uniq}.sock"));
    let _ = std::fs::remove_file(&state);

    // --- Daemon #1: build a session and save it. ---
    start_daemon_persist(&sock1, &state);
    let mut c1 = TestClient::connect(&sock1);
    c1.send(&ClientMsg::NewSession {
        name: Some("persisted".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c1.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
    // Rename the window, split into two panes.
    c1.send(&ClientMsg::Input(vec![0x02, b',']));
    c1.send(&ClientMsg::Input(b"MYWIN\r".to_vec()));
    c1.collect_until(Duration::from_secs(1), |_| false);
    c1.send(&ClientMsg::Input(vec![0x02, b'%']));
    c1.collect_until(Duration::from_secs(1), |_| false);
    // Save the snapshot explicitly.
    c1.send(&ClientMsg::Input(vec![0x02, b':']));
    c1.send(&ClientMsg::Input(b"save-state\r".to_vec()));
    c1.collect_until(Duration::from_secs(1), |_| false);
    assert!(state.exists(), "save-state should write the state file");

    // --- Daemon #2: fresh daemon, same state file → restores. ---
    start_daemon_persist(&sock2, &state);
    let mut c2 = TestClient::connect(&sock2);
    // Attach with no name → picks the (restored) existing session rather than
    // creating a new empty one.
    c2.send(&ClientMsg::Attach { session: None, size: size() });
    c2.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
    c2.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d, vt) = c2.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("MYWIN"),
        "restored session should keep the renamed window; got:\n{vt}"
    );
    assert!(
        vt.contains('│'),
        "restored window should have two panes (a divider); got:\n{vt}"
    );

    let _ = std::fs::remove_file(&state);
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
    // The window list renders entries "0:sh 1:sh 2:sh" with the active one
    // marked '*'. Match the "N:sh" entry (not bare "N:") so the status-bar clock
    // can't masquerade as a window entry.
    assert!(
        has_window(&vt, 0) && has_window(&vt, 1) && has_window(&vt, 2) && vt.contains('*'),
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
fn status_client_prefix_conditional_shows_on_prefix() {
    // Regression for raw "#{?client_prefix,…}" leaking onto the status bar: the
    // conditional must be EVALUATED — hidden normally, shown once the prefix key
    // is armed. Configure status_right with the marker and drive the prefix.
    let path = start_daemon_status_right("#{?client_prefix,PREFIXON,}");
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("pfx".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
    // Repaint with no prefix armed: the marker must NOT show, and the raw token
    // must never appear.
    c.send(&ClientMsg::Resize(WireSize { cols: 80, rows: 24 }));
    let (_d0, before) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        !before.contains("PREFIXON"),
        "marker must be hidden when the prefix isn't armed; got:\n{before}"
    );
    assert!(
        !before.contains("#{?"),
        "the raw #{{?...}} token must never render literally; got:\n{before}"
    );
    // Press the prefix key (Ctrl-b). The status repaints while armed → marker on.
    c.send(&ClientMsg::Input(vec![0x02]));
    let (_d1, armed) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        armed.contains("PREFIXON"),
        "the prefix marker should show once the prefix is armed; got:\n{armed}"
    );
}

#[test]
fn chooser_expand_reveals_windows_and_jumps_to_one() {
    // tmux choose-tree: expand a session (Right) to reveal its windows, navigate
    // to a specific window row, and Enter jumps straight to that window. We make
    // two windows each running a distinct marker command; window 1 is active
    // after creation. Open the chooser, expand, move UP to window 0's row, and
    // Enter — the live view must then show window 0.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("tree".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
    // Window 0 marker.
    c.send(&ClientMsg::Input(b"echo WINZERO_TT\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // New window 1 (now active) with its own marker.
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"echo WINONE_TT\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Open the chooser; cursor starts on the session row.
    c.send(&ClientMsg::Input(vec![0x02, b's']));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Expand the session (Right arrow) → window rows appear.
    c.send(&ClientMsg::Input(b"\x1b[C".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 100, rows: 30 }));
    let (_d0, expanded) = c.collect_until(Duration::from_secs(1), |_| false);
    // The list should now show both window rows as nested (indented) entries.
    assert!(
        expanded.contains("    0:") && expanded.contains("    1:"),
        "expanding should reveal the indented window rows; got:\n{expanded}"
    );
    // Cursor is on the session row (0). Down → window 0 row; Enter selects it.
    c.send(&ClientMsg::Input(b"\x1b[B".to_vec())); // Down to window 0
    c.send(&ClientMsg::Input(b"\r".to_vec())); // Enter: jump to window 0
    // Drain the transition, then force a fresh full repaint to capture the final
    // state (the accumulated stream includes pre-Enter chooser frames).
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Resize(WireSize { cols: 100, rows: 30 }));
    let (_d1, after) = c.collect_until(Duration::from_secs(2), |_| false);
    // The live view now shows window 0's content and its status bar marks window
    // 0 active ("0:sh*") — proving the Enter jumped to window 0, not window 1.
    assert!(
        after.contains("WINZERO_TT"),
        "Enter on the window-0 row should show window 0's content; got:\n{after}"
    );
    assert!(
        after.contains("0:sh*"),
        "after the jump, window 0 must be the active window; got:\n{after}"
    );
}

#[test]
fn chooser_collapse_hides_windows() {
    // Left/collapse on an expanded session hides its window rows again.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("coll".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
    c.send(&ClientMsg::Input(vec![0x02, b'c'])); // 2nd window
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b's']));
    c.send(&ClientMsg::Input(b"\x1b[C".to_vec())); // expand
    c.send(&ClientMsg::Resize(WireSize { cols: 100, rows: 30 }));
    let (_d0, expanded) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(expanded.contains("    1:"), "precondition: expanded shows windows; got:\n{expanded}");
    // Collapse (Left) → the nested window rows disappear.
    c.send(&ClientMsg::Input(b"\x1b[D".to_vec()));
    c.send(&ClientMsg::Resize(WireSize { cols: 100, rows: 30 }));
    let (_d1, collapsed) = c.collect_until(Duration::from_secs(1), |_| false);
    assert!(
        !collapsed.contains("    1:"),
        "collapsing should hide the indented window rows; got:\n{collapsed}"
    );
}

#[test]
fn chooser_preview_shows_all_panes_of_a_window() {
    // Regression: the session-chooser preview showed only each window's active
    // pane. It must render the window's WHOLE split layout — every pane plus the
    // divider between them. Split the window into two panes with distinct
    // markers, open the chooser, and assert BOTH markers AND a `│` divider show
    // in the preview.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("multipane".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
    // Left pane marker, then split right and mark the new pane.
    c.send(&ClientMsg::Input(b"echo LEFTPANE_AA\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'%']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(b"echo RIGHTPANE_BB\n".to_vec()));
    c.collect_until(Duration::from_secs(1), |_| false);
    // Open the chooser (a wide screen so the preview has room for both panes).
    c.send(&ClientMsg::Input(vec![0x02, b's']));
    c.send(&ClientMsg::Resize(WireSize { cols: 120, rows: 30 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("LEFTPANE_AA") && vt.contains("RIGHTPANE_BB"),
        "preview should show BOTH panes' content; got:\n{vt}"
    );
    assert!(
        vt.contains('\u{2502}'),
        "preview should draw a divider between the two panes; got:\n{vt}"
    );
}

#[test]
fn chooser_list_shows_window_count() {
    // The session list must show each session's window count (e.g. "3w"),
    // right-aligned so it survives a long session name.
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("counts".into()),
        shell: Some("/bin/sh".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(2), |m| matches!(m, ServerMsg::Attached { .. }));
    // Three windows total: create two more (prefix c).
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b'c']));
    c.collect_until(Duration::from_secs(1), |_| false);
    c.send(&ClientMsg::Input(vec![0x02, b's']));
    c.send(&ClientMsg::Resize(WireSize { cols: 90, rows: 24 }));
    let (_d, vt) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        vt.contains("choose a session") && vt.contains("3w"),
        "the list should show the window count '3w'; got:\n{vt}"
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
