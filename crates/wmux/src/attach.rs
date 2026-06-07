//! The attach client: connect to the daemon (auto-spawning it if needed), put
//! the terminal in raw mode, and shuttle bytes both ways until detach.
//!
//! The byte-shuttling core ([`run_attach`]) is platform-independent — it works
//! over any split [`FrameReader`]/[`FrameWriter`]. The per-OS entry points wire
//! up the right transport (Unix socket / named pipe) and raw-terminal guard.

use std::io::{self, Read, Write};

use wmux_core::proto::{encode, ClientMsg, Hello, ServerMsg, WireSize};
use wmux_core::traits::{FrameReader, FrameWriter};

#[cfg(unix)]
use crate::term_unix::RawTerminal;
#[cfg(windows)]
use crate::term_win::RawTerminal;

/// Perform the protocol handshake: send our Hello, read the daemon's, and check
/// versions. Shared by attach and the control client.
pub fn handshake<R: FrameReader, W: FrameWriter>(
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<()> {
    let hello = Hello::current(format!("wmux/{}", env!("CARGO_PKG_VERSION")));
    writer.write_frame(&encode(&hello)?)?;
    let reply = reader
        .read_frame()?
        .ok_or_else(|| anyhow::anyhow!("daemon closed during handshake"))?;
    // The daemon may answer a version mismatch with an Error frame instead of a
    // Hello; surface either clearly.
    if let Ok(server_hello) = wmux_core::proto::decode::<Hello>(&reply) {
        server_hello
            .check()
            .map_err(|m| anyhow::anyhow!(m.to_string()))?;
        Ok(())
    } else if let Ok(ServerMsg::Error(e)) = wmux_core::proto::decode::<ServerMsg>(&reply) {
        anyhow::bail!("daemon rejected connection: {e}")
    } else {
        anyhow::bail!("unexpected handshake response from daemon")
    }
}

/// Attach to (or create) a session and run until detached. Platform glue picks
/// the transport and connects (auto-spawning the daemon).
pub fn attach(
    session: Option<String>,
    new_session: bool,
    shell: Option<String>,
) -> anyhow::Result<()> {
    let (reader, writer) = platform::connect()?;
    run_attach(reader, writer, session, new_session, shell)
}

/// Platform-independent attach loop over a split transport.
fn run_attach<R, W>(
    mut reader: R,
    mut writer: W,
    session: Option<String>,
    new_session: bool,
    shell: Option<String>,
) -> anyhow::Result<()>
where
    R: FrameReader + 'static,
    W: FrameWriter,
{
    let size = RawTerminal::size();
    // Protocol handshake before any session message.
    handshake(&mut reader, &mut writer)?;
    let first = if new_session {
        ClientMsg::NewSession {
            name: session,
            shell,
            size: WireSize {
                cols: size.cols,
                rows: size.rows,
            },
        }
    } else {
        ClientMsg::Attach {
            session,
            size: WireSize {
                cols: size.cols,
                rows: size.rows,
            },
        }
    };
    writer.write_frame(&encode(&first)?)?;

    let _term = RawTerminal::enter()?;

    // Reader thread: daemon frames -> stdout.
    let reader_handle = std::thread::spawn(move || {
        let mut stdout = io::stdout();
        #[allow(clippy::while_let_loop)] // breaks on detach/event, not just EOF
        loop {
            match reader.read_frame() {
                Ok(Some(bytes)) => match wmux_core::proto::decode::<ServerMsg>(&bytes) {
                    Ok(ServerMsg::Frame(vt)) => {
                        if stdout.write_all(&vt).is_err() {
                            break;
                        }
                        let _ = stdout.flush();
                    }
                    Ok(ServerMsg::Detached) | Ok(ServerMsg::Event(_)) => break,
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
        if writer
            .write_frame(&encode(&ClientMsg::Input(buf[..n].to_vec()))?)
            .is_err()
        {
            break;
        }
    }
    let _ = reader_handle.join();
    Ok(())
}

#[cfg(unix)]
mod platform {
    use std::path::Path;
    use std::time::{Duration, Instant};
    use wmux_backend_unix::{default_socket_path, UnixReader, UnixTransport, UnixWriter};
    use wmux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(UnixReader, UnixWriter)> {
        let path = std::env::var_os("WMUX_SOCK")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(default_socket_path);
        let transport = connect_or_spawn(&path)?;
        Ok(transport.split()?)
    }

    fn connect_or_spawn(path: &Path) -> anyhow::Result<UnixTransport> {
        if let Ok(t) = UnixTransport::connect(path) {
            return Ok(t);
        }
        spawn_daemon()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(t) = UnixTransport::connect(path) {
                return Ok(t);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        anyhow::bail!("failed to connect to wmux daemon at {}", path.display())
    }

    fn spawn_daemon() -> anyhow::Result<()> {
        super::spawn_daemon_process("wmuxd")
    }
}

#[cfg(windows)]
mod platform {
    use std::time::{Duration, Instant};
    use wmux_backend_win::{default_pipe_path, PipeReader, PipeTransport, PipeWriter};
    use wmux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(PipeReader, PipeWriter)> {
        let path = std::env::var("WMUX_PIPE").unwrap_or_else(|_| default_pipe_path());
        let transport = connect_or_spawn(&path)?;
        Ok(transport.split()?)
    }

    fn connect_or_spawn(path: &str) -> anyhow::Result<PipeTransport> {
        if let Ok(t) = PipeTransport::connect(path) {
            return Ok(t);
        }
        super::spawn_daemon_process("wmuxd.exe")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(t) = PipeTransport::connect(path) {
                return Ok(t);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        anyhow::bail!("failed to connect to wmux daemon pipe {path}")
    }
}

/// Spawn the daemon binary detached from this process. Looks for a sibling
/// binary next to the client first, then falls back to PATH.
fn spawn_daemon_process(exe_name: &str) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().ok();
    let daemon = exe
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.join(exe_name))
        .filter(|p| p.exists())
        .unwrap_or_else(|| exe_name.into());

    Command::new(daemon)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
