//! Windows integration tests over ConPTY + named pipes.
//!
//! Mirrors the Unix keystone scenarios on the real Windows backend. These
//! cannot run on the Linux dev box; they execute on the `windows-latest` CI
//! runner. From Linux they are still type-checked via the msvc cross-target.

#![cfg(windows)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wmux_backend_win::{PipeListener, PipeTransport, WinPtySystem};
use wmux_core::proto::{encode, ClientMsg, ServerMsg, WireSize};
use wmux_core::traits::{FrameReader, FrameWriter, Transport};

fn unique_pipe() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(r"\\.\pipe\wmux-test-{pid}-{n}")
}

fn start_daemon() -> String {
    let path = unique_pipe();
    let listener = PipeListener::bind(path.clone()).expect("bind pipe");
    std::thread::spawn(move || {
        let _ = wmuxd::run(WinPtySystem, listener);
    });
    // Give the listener a moment to call CreateNamedPipe before first connect.
    std::thread::sleep(Duration::from_millis(100));
    path
}

struct TestClient {
    reader: wmux_backend_win::PipeReader,
    writer: wmux_backend_win::PipeWriter,
}

impl TestClient {
    fn connect(path: &str) -> Self {
        // Retry briefly: the server creates a fresh instance per accept().
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(t) = PipeTransport::connect(path) {
                let (reader, writer) = t.split().expect("split");
                let mut c = Self { reader, writer };
                c.handshake();
                return c;
            }
            if Instant::now() >= deadline {
                panic!("could not connect to test daemon pipe");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Send our Hello and consume the daemon's reply.
    fn handshake(&mut self) {
        let hello = wmux_core::proto::Hello::current("win-test-client");
        self.writer.write_frame(&encode(&hello).unwrap()).unwrap();
        let _ = self.reader.read_frame();
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.writer.write_frame(&encode(msg).unwrap()).unwrap();
    }

    fn collect_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&ServerMsg) -> bool,
    ) -> (bool, String) {
        let deadline = Instant::now() + timeout;
        let mut vt = String::new();
        while Instant::now() < deadline {
            match self.reader.read_frame() {
                Ok(Some(bytes)) => {
                    if let Ok(msg) = wmux_core::proto::decode::<ServerMsg>(&bytes) {
                        if let ServerMsg::Frame(b) = &msg {
                            vt.push_str(&String::from_utf8_lossy(b));
                        }
                        if pred(&msg) {
                            return (true, vt);
                        }
                    }
                }
                _ => break,
            }
        }
        (false, vt)
    }
}

fn size() -> WireSize {
    WireSize { cols: 80, rows: 24 }
}

#[test]
fn attach_creates_session_over_named_pipe() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::Attach {
        session: Some("work".into()),
        size: size(),
    });
    let (ok, _) = c.collect_until(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(ok, "daemon must ack attach over the named pipe");
}

#[test]
fn cmd_shell_runs_under_conpty() {
    let path = start_daemon();
    let mut c = TestClient::connect(&path);
    c.send(&ClientMsg::NewSession {
        name: Some("s".into()),
        shell: Some("cmd.exe".into()),
        size: size(),
    });
    c.collect_until(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    // Echo a unique marker; ConPTY should render it into frames.
    c.send(&ClientMsg::Input(b"echo WMUX_WIN_MARKER\r\n".to_vec()));
    let (_done, vt) = c.collect_until(Duration::from_secs(5), |_| false);
    assert!(
        vt.contains("WMUX_WIN_MARKER"),
        "cmd.exe output should render via ConPTY; got:\n{vt}"
    );
}

#[test]
fn detach_then_reattach_preserves_session_windows() {
    let path = start_daemon();

    let mut c1 = TestClient::connect(&path);
    c1.send(&ClientMsg::NewSession {
        name: Some("persist".into()),
        shell: Some("cmd.exe".into()),
        size: size(),
    });
    c1.collect_until(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    c1.send(&ClientMsg::Input(b"echo PERSIST_WIN\r\n".to_vec()));
    c1.collect_until(Duration::from_secs(3), |_| false);
    c1.send(&ClientMsg::Detach);
    drop(c1);

    let mut c2 = TestClient::connect(&path);
    c2.send(&ClientMsg::Attach {
        session: Some("persist".into()),
        size: size(),
    });
    let (ok, vt) = c2.collect_until(Duration::from_secs(3), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(ok, "reattach over named pipe must succeed");
    let (_d, vt2) = c2.collect_until(Duration::from_secs(2), |_| false);
    assert!(
        format!("{vt}{vt2}").contains("PERSIST_WIN"),
        "reattached screen should still show pre-detach output"
    );
}
