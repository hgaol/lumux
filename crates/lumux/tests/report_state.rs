#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lumux_backend_unix::{UnixPtySystem, UnixSocketListener};
use lumux_core::proto::{decode, encode, ClientMsg, ServerMsg, WireSize};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct TestClient {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl TestClient {
    fn connect(path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(path).expect("connect to test daemon");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut client = Self {
            stream,
            buf: Vec::new(),
        };
        let hello = lumux_core::proto::Hello::current("report-state-test");
        client.stream.write_all(&encode(&hello).unwrap()).unwrap();
        client.stream.flush().unwrap();
        client.read_hello();
        client
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.stream.write_all(&encode(msg).unwrap()).unwrap();
        self.stream.flush().unwrap();
    }

    fn read_hello(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut chunk = [0u8; 4096];
        while Instant::now() < deadline {
            if let Some(consumed) = framed_payload(&self.buf).and_then(|(payload, consumed)| {
                decode::<lumux_core::proto::Hello>(payload)
                    .ok()
                    .map(|_| consumed)
            }) {
                self.buf.drain(..consumed);
                return;
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read daemon hello: {error}"),
            }
        }
        panic!("daemon did not complete handshake");
    }

    fn collect_frames(&mut self, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut vt = String::new();
        let mut chunk = [0u8; 4096];
        while Instant::now() < deadline {
            while let Some((payload, consumed)) = framed_payload(&self.buf) {
                if let Ok(ServerMsg::Frame(bytes) | ServerMsg::FrameAt { bytes, .. }) =
                    decode::<ServerMsg>(payload)
                {
                    vt.push_str(&String::from_utf8_lossy(&bytes));
                }
                self.buf.drain(..consumed);
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read daemon frame: {error}"),
            }
        }
        vt
    }
}

fn framed_payload(buf: &[u8]) -> Option<(&[u8], usize)> {
    let len = u32::from_be_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    (buf.len() >= 4 + len).then(|| (&buf[4..4 + len], 4 + len))
}

fn start_daemon() -> std::path::PathBuf {
    let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!(
        "lumux-report-state-test-{}-{id}",
        std::process::id()
    ));
    let socket = base.with_extension("sock");
    let state = base.with_extension("state");
    let listener = UnixSocketListener::bind(&socket).expect("bind test socket");
    let config = lumux_core::config::Config {
        sidebar: true,
        ..Default::default()
    };
    std::thread::spawn(move || {
        let _ = lumux_server::run_with_config_at(UnixPtySystem, listener, config, state);
    });
    socket
}

#[test]
fn report_state_does_not_render_at_a_fake_control_client_size() {
    let socket = start_daemon();
    let mut interactive = TestClient::connect(&socket);
    interactive.send(&ClientMsg::NewSession {
        name: Some("wide".into()),
        shell: Some("/bin/sh".into()),
        size: WireSize {
            cols: 120,
            rows: 40,
        },
    });
    let _ = interactive.collect_frames(Duration::from_millis(500));

    let output = Command::new(env!("CARGO_BIN_EXE_lumux"))
        .args(["report-state", "done", "--agent", "claude"])
        .env("LUMUX_SOCK", &socket)
        .env("LUMUX_PANE", "%1")
        .env_remove("LUMUX")
        .output()
        .expect("run report-state hook command");
    assert!(
        output.status.success(),
        "report-state failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let frames = interactive.collect_frames(Duration::from_secs(1));
    assert!(
        frames.contains("claude"),
        "the real pane's agent report must reach the sidebar: {frames:?}"
    );
    assert!(
        !frames.contains("\x1b[24;1H"),
        "the hook's control connection rendered the real client at its fake \
         80x24 size and left the window corrupted: {frames:?}"
    );
    assert!(
        !frames.contains("\x1b[2J"),
        "an agent state transition should be a damage-tracked sidebar diff, \
         not a full-terminal clear: {frames:?}"
    );
}

#[test]
fn report_state_is_a_silent_noop_outside_lumux() {
    let output = Command::new(env!("CARGO_BIN_EXE_lumux"))
        .args(["report-state", "idle", "--agent", "claude"])
        .env_remove("LUMUX")
        .env_remove("LUMUX_PANE")
        .env_remove("LUMUX_AGENT")
        .output()
        .expect("run out-of-pane hook command");

    assert!(output.status.success(), "hook should be best-effort");
    assert!(output.stdout.is_empty(), "hook wrote stdout");
    assert!(output.stderr.is_empty(), "hook wrote stderr");
}
