//! The attach client: connect to the daemon (auto-spawning it if needed), put
//! the terminal in raw mode, and shuttle bytes both ways until detach.

#![cfg(unix)]

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use wmux_backend_unix::{default_socket_path, UnixTransport};
use wmux_core::proto::{encode, ClientMsg, ServerMsg, WireSize};
use wmux_core::traits::{FrameReader, FrameWriter, Transport};

use crate::term_unix::RawTerminal;

/// Attach to (or create) a session and run until detached.
pub fn attach(session: Option<String>, new_session: bool, shell: Option<String>) -> anyhow::Result<()> {
    let path = socket_path();
    let transport = connect_or_spawn(&path)?;
    let (mut reader, mut writer) = transport.split()?;

    let size = RawTerminal::size();
    let first = if new_session {
        ClientMsg::NewSession {
            name: session.clone(),
            shell,
            size: WireSize {
                cols: size.cols,
                rows: size.rows,
            },
        }
    } else {
        ClientMsg::Attach {
            session: session.clone(),
            size: WireSize {
                cols: size.cols,
                rows: size.rows,
            },
        }
    };
    writer.write_frame(&encode(&first)?)?;

    // Enter raw mode only once we're talking to the daemon.
    let _term = RawTerminal::enter()?;

    // Reader thread: daemon frames -> stdout.
    let reader_handle = std::thread::spawn(move || {
        let mut stdout = io::stdout();
        #[allow(clippy::while_let_loop)] // body breaks on detach/event, not just EOF
        loop {
            match reader.read_frame() {
                Ok(Some(bytes)) => match wmux_core::proto::decode::<ServerMsg>(&bytes) {
                    Ok(ServerMsg::Frame(vt)) => {
                        if stdout.write_all(&vt).is_err() {
                            break;
                        }
                        let _ = stdout.flush();
                    }
                    Ok(ServerMsg::Detached) | Ok(ServerMsg::Event(_)) => {
                        // Detached or session closed -> end the client.
                        break;
                    }
                    Ok(ServerMsg::Reply(text)) => {
                        let _ = stdout.write_all(text.as_bytes());
                        let _ = stdout.flush();
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                _ => break,
            }
        }
    });

    // Main thread: stdin -> daemon as Input frames.
    let mut stdin = io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        if reader_handle.is_finished() {
            break;
        }
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let msg = ClientMsg::Input(buf[..n].to_vec());
        if writer.write_frame(&encode(&msg)?).is_err() {
            break;
        }
    }
    let _ = reader_handle.join();
    Ok(())
}

fn socket_path() -> std::path::PathBuf {
    std::env::var_os("WMUX_SOCK")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path)
}

/// Connect to the daemon, spawning it (detached) and retrying briefly if the
/// socket isn't there yet.
fn connect_or_spawn(path: &Path) -> anyhow::Result<UnixTransport> {
    if let Ok(t) = UnixTransport::connect(path) {
        return Ok(t);
    }
    spawn_daemon()?;
    // Retry for up to ~2s while the daemon binds.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(t) = UnixTransport::connect(path) {
            return Ok(t);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    anyhow::bail!("failed to connect to wmux daemon at {}", path.display())
}

/// Spawn `wmuxd` detached from this process/console.
fn spawn_daemon() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    // Prefer a sibling `wmuxd` next to this binary; fall back to PATH.
    let exe = std::env::current_exe().ok();
    let wmuxd = exe
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.join("wmuxd"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| "wmuxd".into());

    Command::new(wmuxd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
