//! End-to-end integration tests for the daemon over a real Unix socket with a
//! real shell PTY. This is the Phase 7 keystone: it proves wmux genuinely
//! multiplexes, persists across detach, and cascades on exit — all on Linux,
//! before any Windows code exists.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wmux_backend_unix::{UnixPtySystem, UnixSocketListener};
use wmux_core::proto::{decode, encode, ClientMsg, ServerMsg, WireSize};

/// Spawn the daemon control loop on a throwaway socket, returning its path.
fn start_daemon() -> std::path::PathBuf {
    // Unique socket per call: pid + a process-wide monotonic counter (parallel
    // tests must not collide).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("wmux-test-{pid}-{n}.sock"));
    let listener = UnixSocketListener::bind(&path).expect("bind");
    std::thread::spawn(move || {
        let _ = wmuxd::run(UnixPtySystem, listener);
    });
    // Wait for the socket to exist.
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
        let hello = wmux_core::proto::Hello::current("test-client");
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
fn try_decode_hello(buf: &[u8]) -> Option<(wmux_core::proto::Hello, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let h = decode::<wmux_core::proto::Hello>(&buf[4..4 + len]).ok()?;
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
    c.send(&ClientMsg::Input(b"echo WMUX_MARKER_123\n".to_vec()));
    let (saw, vt) = c.collect_until(Duration::from_secs(3), |_| false);
    let _ = saw;
    assert!(
        vt.contains("WMUX_MARKER_123"),
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
    // Ctrl-b | -> split horizontally. Send as raw input through the keymap.
    c.send(&ClientMsg::Input(vec![0x02, b'|']));
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
            ServerMsg::Event(wmux_core::proto::Event::SessionClosed) | ServerMsg::Detached
        )
    });
    assert!(closed, "exiting the last shell must close the session");
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
fn send_keys_command_injects_into_pane() {
    use wmux_core::proto::Command;
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
    use wmux_core::proto::Command;
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
    let cfg_path = std::env::temp_dir().join(format!("wmux-cfg-{}.toml", std::process::id()));
    std::fs::write(&cfg_path, "prefix = \"C-a\"\n").unwrap();
    c.send(&ClientMsg::Command(Command::SourceFile {
        path: cfg_path.to_string_lossy().to_string(),
    }));
    let (sourced, _) = c.collect_until(
        Duration::from_secs(2),
        |m| matches!(m, ServerMsg::Reply(t) if t.contains("sourced")),
    );
    assert!(sourced, "source-file should reply with confirmation");

    // Now Ctrl-a | should split (new prefix); a border appears.
    c.send(&ClientMsg::Input(vec![0x01, b'|']));
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
        shell: Some("/no/such/shell/wmux-nonexistent".into()),
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
    let dir = std::env::temp_dir().join(format!("wmux-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    c.send(&ClientMsg::Input(
        format!("cd {}\n", dir.display()).into_bytes(),
    ));
    c.collect_until(Duration::from_secs(2), |_| false);
    // Split: the new pane's shell should start in the same directory.
    c.send(&ClientMsg::Input(vec![0x02, b'|']));
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
    let cfg = wmux_core::config::Config::from_toml(toml).expect("example config parses");
    assert_eq!(cfg.prefix, "C-b");
    assert!(cfg.mouse);
    assert_eq!(cfg.scrollback, 10000);
    assert_eq!(cfg.base_index, 1);
    assert_eq!(cfg.status_justify, "centre");
    assert_eq!(cfg.status_bg, "colour24");
    // Bindings compile into a usable table (prefix + root nav + reload).
    let b = cfg.to_bindings().expect("bindings build");
    use wmux_core::keymap::{Action, Key, KeyCode};
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
    // Any key closes it and returns to the live shell view.
    c.send(&ClientMsg::Input(b"q".to_vec()));
    let (_d2, vt2) = c.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        !vt2.contains("-- HELP --"),
        "a keypress should dismiss the help overlay"
    );
}
