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

    // Shared flag: set when the daemon detaches us (or the connection ends), so
    // the stdin loop can notice even while no keys are being pressed.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_done = done.clone();

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
        reader_done.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // Main thread: stdin -> daemon as Input frames. Reads are time-bounded so a
    // detach delivered while the user is idle still ends the client promptly.
    let mut stdin = io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        if done.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        // Wait briefly for stdin input; if nothing arrives, loop to re-check the
        // detach flag rather than blocking forever in read().
        match stdin_ready(&stdin, std::time::Duration::from_millis(100)) {
            StdinState::Ready => {}
            StdinState::Idle => continue,
            StdinState::Closed => break,
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
    // Disable mouse reporting before the terminal is restored (harmless if it
    // was never enabled).
    {
        use std::io::Write;
        let mut out = io::stdout();
        let _ = out.write_all(wmux_core::mouse::DISABLE.as_bytes());
        let _ = out.flush();
    }
    Ok(())
}

/// Result of waiting for stdin readiness.
enum StdinState {
    /// Data is available to read now.
    Ready,
    /// The timeout elapsed with no data (caller should loop and re-check flags).
    Idle,
    /// Stdin reached EOF / errored.
    Closed,
}

/// Wait up to `timeout` for stdin to have data, without consuming it. Lets the
/// attach loop notice an out-of-band detach while the user is idle instead of
/// blocking indefinitely in `read()`.
#[cfg(unix)]
fn stdin_ready(stdin: &std::io::Stdin, timeout: std::time::Duration) -> StdinState {
    use std::os::fd::AsRawFd;
    let fd = stdin.as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
    if rc < 0 {
        // EINTR or similar — treat as idle so the loop re-checks.
        StdinState::Idle
    } else if rc == 0 {
        StdinState::Idle
    } else if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        StdinState::Closed
    } else {
        StdinState::Ready
    }
}

/// Windows: wait on the stdin console/file handle.
#[cfg(windows)]
fn stdin_ready(_stdin: &std::io::Stdin, timeout: std::time::Duration) -> StdinState {
    use std::os::windows::io::AsRawHandle;
    let handle = std::io::stdin().as_raw_handle();
    let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    // WAIT_OBJECT_0 = 0 (signaled), WAIT_TIMEOUT = 0x102.
    let rc = unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(handle as _, ms) };
    match rc {
        0 => StdinState::Ready,
        0x102 => StdinState::Idle,
        _ => StdinState::Closed,
    }
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
        super::spawn_daemon_process()
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
        super::spawn_daemon_process()?;
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

/// Spawn the daemon by re-execing *this* binary with the hidden `--server`
/// flag, detached from the current process/console (tmux's single-binary model).
fn spawn_daemon_process() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("--server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
