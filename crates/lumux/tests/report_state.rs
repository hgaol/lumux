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

fn pane_id_from_shell(client: &mut TestClient) -> String {
    client.send(&ClientMsg::Input(
        b"printf 'LUMUX_TEST_PANE<%s>\\n' \"$LUMUX_PANE\"\n".to_vec(),
    ));
    let frames = client.collect_frames(Duration::from_secs(1));
    for (start, _) in frames.match_indices("LUMUX_TEST_PANE<") {
        let value = &frames[start + "LUMUX_TEST_PANE<".len()..];
        let Some(end) = value.find('>') else {
            continue;
        };
        let candidate = &value[..end];
        if candidate.strip_prefix('%').is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            return candidate.to_string();
        }
    }
    panic!("pane shell did not expose a valid LUMUX_PANE: {frames:?}");
}

fn pane_runtime_from_shell(client: &mut TestClient) -> (String, String) {
    client.send(&ClientMsg::Input(
        b"printf 'LUMUX_TEST_RUNTIME<%s>|PANE<%s>\\n' \"$LUMUX\" \"$LUMUX_PANE\"\n"
            .to_vec(),
    ));
    let frames = client.collect_frames(Duration::from_secs(1));
    for (start, _) in frames.match_indices("LUMUX_TEST_RUNTIME<") {
        let value = &frames[start + "LUMUX_TEST_RUNTIME<".len()..];
        let Some((endpoint, pane_tail)) = value.split_once(">|PANE<") else {
            continue;
        };
        let Some(end) = pane_tail.find('>') else {
            continue;
        };
        let pane = &pane_tail[..end];
        if !endpoint.is_empty()
            && pane.strip_prefix('%').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return (endpoint.to_string(), pane.to_string());
        }
    }
    panic!("pane shell did not expose a valid Lumux runtime: {frames:?}");
}

fn run_provider_wrapper(
    wrapper: &std::path::Path,
    action: &str,
    payload: &str,
    endpoint: &str,
    pane: &str,
) -> std::process::Output {
    // The daemon in this test runs inside the server test process, so its
    // current_exe is not the lumux CLI. Supply the real built CLI explicitly;
    // this helper verifies installed-wrapper behavior and endpoint routing,
    // while real-provider loading is covered by the provider smoke checks.
    let mut child = Command::new("sh")
        .arg(wrapper)
        .arg(action)
        .env("LUMUX", endpoint)
        .env("LUMUX_PANE", pane)
        .env("LUMUX_BIN", env!("CARGO_BIN_EXE_lumux"))
        .env_remove("LUMUX_SOCK")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run installed provider hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
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
    let pane = pane_id_from_shell(&mut interactive);

    let output = Command::new(env!("CARGO_BIN_EXE_lumux"))
        .args(["report-state", "done", "--agent", "claude"])
        .env("LUMUX_SOCK", &socket)
        .env("LUMUX_PANE", pane)
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
fn report_state_uses_the_pane_runtime_endpoint_without_transport_specific_env() {
    let socket = start_daemon();
    let mut interactive = TestClient::connect(&socket);
    interactive.send(&ClientMsg::NewSession {
        name: Some("runtime-endpoint".into()),
        shell: Some("/bin/sh".into()),
        size: WireSize {
            cols: 120,
            rows: 40,
        },
    });
    let pane = pane_id_from_shell(&mut interactive);

    // Panes expose one cross-platform runtime contract: LUMUX identifies the
    // daemon. Provider hooks must not also need to know whether the transport
    // happens to use LUMUX_SOCK or LUMUX_PIPE.
    let output = Command::new(env!("CARGO_BIN_EXE_lumux"))
        .args(["report-state", "working", "--agent", "codex"])
        .env("LUMUX", &socket)
        .env("LUMUX_PANE", pane)
        .env_remove("LUMUX_SOCK")
        .output()
        .expect("run report-state with pane runtime context");
    assert!(
        output.status.success(),
        "report-state failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let frames = interactive.collect_frames(Duration::from_secs(1));
    assert!(
        frames.contains("codex"),
        "the pane runtime endpoint must route the report to its daemon: {frames:?}"
    );
}

#[test]
fn installed_codex_and_copilot_wrappers_route_via_the_spawned_pane_endpoint() {
    let socket = start_daemon();
    let mut interactive = TestClient::connect(&socket);
    interactive.send(&ClientMsg::NewSession {
        name: Some("provider-runtime".into()),
        shell: Some("/bin/sh".into()),
        size: WireSize {
            cols: 120,
            rows: 40,
        },
    });
    let (endpoint, pane) = pane_runtime_from_shell(&mut interactive);
    assert_eq!(std::path::Path::new(&endpoint), socket.as_path());

    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("codex");
    let copilot_home = dir.path().join("copilot");
    for (provider, home_key, home) in [
        ("codex", "CODEX_HOME", &codex_home),
        ("copilot", "COPILOT_HOME", &copilot_home),
    ] {
        let install = Command::new(env!("CARGO_BIN_EXE_lumux"))
            .args(["integration", provider])
            .env(home_key, home)
            .output()
            .expect("install provider integration");
        assert!(
            install.status.success(),
            "failed to install {provider}: {}",
            String::from_utf8_lossy(&install.stderr)
        );
    }

    let codex = run_provider_wrapper(
        &codex_home.join("lumux-agent-state.sh"),
        "working",
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"codex-e2e"}"#,
        &endpoint,
        &pane,
    );
    assert!(codex.status.success());
    assert!(codex.stdout.is_empty());
    assert!(codex.stderr.is_empty());
    let frames = interactive.collect_frames(Duration::from_secs(1));
    assert!(
        frames.contains("codex"),
        "Codex hook did not reach its pane's daemon via LUMUX={endpoint:?}: {frames:?}"
    );

    drop(interactive);

    // Use a fresh projection so the assertion observes Copilot's whole row,
    // rather than only the damage diff from replacing the previous agent name.
    let socket = start_daemon();
    let mut interactive = TestClient::connect(&socket);
    interactive.send(&ClientMsg::NewSession {
        name: Some("provider-runtime-two".into()),
        shell: Some("/bin/sh".into()),
        size: WireSize {
            cols: 120,
            rows: 40,
        },
    });
    let (endpoint, pane) = pane_runtime_from_shell(&mut interactive);
    assert_eq!(std::path::Path::new(&endpoint), socket.as_path());

    let copilot = run_provider_wrapper(
        &copilot_home.join("hooks/lumux-agent-state.sh"),
        "working",
        r#"{"hook_event_name":"UserPromptSubmit","sessionId":"copilot-e2e"}"#,
        &endpoint,
        &pane,
    );
    assert!(copilot.status.success());
    assert!(copilot.stdout.is_empty());
    assert!(copilot.stderr.is_empty());
    let frames = interactive.collect_frames(Duration::from_secs(1));
    assert!(
        frames.contains("copilot"),
        "Copilot hook did not reach its pane's daemon via LUMUX={endpoint:?}: {frames:?}"
    );
}

#[test]
fn provider_installers_explain_activation_requirements() {
    let dir = tempfile::tempdir().unwrap();

    let codex = Command::new(env!("CARGO_BIN_EXE_lumux"))
        .args(["integration", "codex"])
        .env("CODEX_HOME", dir.path().join("codex"))
        .output()
        .expect("install Codex integration");
    assert!(codex.status.success());
    let codex_stdout = String::from_utf8_lossy(&codex.stdout);
    assert!(
        codex_stdout.contains("Restart Codex"),
        "installer omitted the config-reload requirement: {codex_stdout:?}"
    );
    assert!(
        codex_stdout.contains("hooks execute zero times until"),
        "installer understated Codex's trust gate: {codex_stdout:?}"
    );

    let copilot = Command::new(env!("CARGO_BIN_EXE_lumux"))
        .args(["integration", "copilot"])
        .env("COPILOT_HOME", dir.path().join("copilot"))
        .output()
        .expect("install Copilot integration");
    assert!(copilot.status.success());
    let copilot_stdout = String::from_utf8_lossy(&copilot.stdout);
    assert!(
        copilot_stdout.contains("Restart GitHub Copilot CLI"),
        "installer omitted the config-reload requirement: {copilot_stdout:?}"
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
