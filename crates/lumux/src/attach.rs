//! The attach client: connect to the daemon (auto-spawning it if needed), put
//! the terminal in raw mode, and shuttle bytes both ways until detach.
//!
//! The byte-shuttling core ([`run_attach`]) is platform-independent — it works
//! over any split [`FrameReader`]/[`FrameWriter`]. The per-OS entry points wire
//! up the right transport (Unix socket / named pipe) and raw-terminal guard.

use std::io::{self, Read, Write};

use lumux_core::proto::{encode, ClientMsg, Event, Hello, ServerMsg, WireSize};
use lumux_core::terminal_input::{OuterInputDecoder, OuterInputEvent};
use lumux_core::traits::{FrameReader, FrameWriter};

#[cfg(unix)]
use crate::term_unix::RawTerminal;
#[cfg(windows)]
use crate::term_win::RawTerminal;

/// What the reader thread should do with one decoded message from the daemon.
#[derive(Debug, PartialEq, Eq)]
pub enum ReaderAction {
    /// Write these bytes to stdout (a VT frame, a reply, or the bell).
    Write(Vec<u8>),
    /// Nothing to do — e.g. a pane/window closed but the session survives and a
    /// fresh frame follows to repaint.
    Ignore,
    /// End the attach: the session is gone or the daemon detached us.
    Stop,
}

/// Decide what one daemon message means for the attach loop. Pure (no I/O) so
/// the session-lifecycle rules are unit-testable.
///
/// The load-bearing rule: only a *session-ending* signal stops the attach.
/// `Event::PaneExited` fires whenever a pane or window closes while the session
/// is still alive (the daemon sends a frame right after to repaint), so it must
/// NOT tear the client down — otherwise exiting one window kills the whole
/// session for the user.
pub fn reader_action(msg: ServerMsg) -> ReaderAction {
    match msg {
        ServerMsg::Frame(vt) => ReaderAction::Write(vt),
        ServerMsg::FrameAt { bytes, .. } => ReaderAction::Write(bytes),
        ServerMsg::Reply(text) => ReaderAction::Write(text.into_bytes()),
        ServerMsg::Detached => ReaderAction::Stop,
        ServerMsg::Event(Event::SessionClosed) => ReaderAction::Stop,
        ServerMsg::Event(Event::Bell) => ReaderAction::Write(vec![0x07]),
        // LayoutChanged / PaneExited: the session lives; a frame follows.
        ServerMsg::Event(_) => ReaderAction::Ignore,
        _ => ReaderAction::Ignore,
    }
}

/// Perform the protocol handshake: send our Hello, read the daemon's, and check
/// versions. Shared by attach and the control client.
pub fn handshake<R: FrameReader, W: FrameWriter>(
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<()> {
    let hello = Hello::current(format!("lumux/{}", env!("CARGO_PKG_VERSION")));
    writer.write_frame(&encode(&hello)?)?;
    let reply = reader
        .read_frame()?
        .ok_or_else(|| anyhow::anyhow!("daemon closed during handshake"))?;
    // The daemon may answer a version mismatch with an Error frame instead of a
    // Hello; surface either clearly.
    if let Ok(server_hello) = lumux_core::proto::decode::<Hello>(&reply) {
        server_hello
            .check()
            .map_err(|m| anyhow::anyhow!(m.to_string()))?;
        Ok(())
    } else if let Ok(ServerMsg::Error(e)) = lumux_core::proto::decode::<ServerMsg>(&reply) {
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
    W: FrameWriter + 'static,
{
    let size = RawTerminal::size();
    // Whether to track terminal-size changes after attach (config `auto_resize`,
    // on by default). Loaded here in the client because resize is a client-side
    // concern; same binary/machine as the daemon, so we can read the config
    // directly rather than plumbing it through the handshake.
    let auto_resize = lumux_server::load_config().auto_resize;
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

    // Wait for the daemon's verdict before touching the terminal: Attached means
    // we're in; Error means it refused (e.g. a duplicate session name) — print
    // the reason and exit non-zero without entering raw mode.
    loop {
        let reply = reader
            .read_frame()?
            .ok_or_else(|| anyhow::anyhow!("daemon closed before attach"))?;
        match lumux_core::proto::decode::<ServerMsg>(&reply) {
            Ok(ServerMsg::Attached { .. }) => break,
            Ok(ServerMsg::Error(e)) => anyhow::bail!("{e}"),
            // Stray frames before the ack (none expected) are ignored.
            Ok(_) | Err(_) => continue,
        }
    }

    let _term = RawTerminal::enter()?;

    // Enable bracketed paste on the outer terminal so pasted text arrives wrapped
    // in ESC[200~..ESC[201~ and is forwarded to the pane verbatim (never
    // interpreted as the prefix or a binding). Disabled again at cleanup below.
    {
        use std::io::Write;
        let mut out = io::stdout();
        let _ = out.write_all(lumux_core::keymap::PASTE_ENABLE.as_bytes());
        let _ = out.flush();
    }

    // Shared flag: set when the daemon detaches us (or the connection ends), so
    // the stdin loop can notice even while no keys are being pressed.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_done = done.clone();
    // Epoch 0 means no composed frame has reached the terminal yet. The reader
    // advances it only after a successful stdout flush; the input thread reads
    // that published value when it packages terminal bytes for the daemon.
    let applied_frame_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reader_frame_epoch = applied_frame_epoch.clone();

    // Reader thread: daemon frames -> stdout.
    let reader_handle = std::thread::spawn(move || {
        let mut stdout = io::stdout();
        #[allow(clippy::while_let_loop)] // breaks on detach/event, not just EOF
        loop {
            match reader.read_frame() {
                Ok(Some(bytes)) => match lumux_core::proto::decode::<ServerMsg>(&bytes) {
                    Ok(msg) => {
                        let frame_epoch = match &msg {
                            ServerMsg::FrameAt { epoch, .. } => Some(*epoch),
                            _ => None,
                        };
                        match reader_action(msg) {
                            ReaderAction::Write(bytes) => {
                                if stdout.write_all(&bytes).is_err() {
                                    break;
                                }
                                if stdout.flush().is_err() {
                                    break;
                                }
                                if let Some(epoch) = frame_epoch {
                                    reader_frame_epoch
                                        .store(epoch, std::sync::atomic::Ordering::Release);
                                }
                            }
                            ReaderAction::Ignore => {}
                            ReaderAction::Stop => break,
                        }
                    }
                    Err(_) => break,
                },
                _ => break,
            }
        }
        reader_done.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // Wrap the writer so both the stdin loop and the resize-watcher thread can
    // send frames over the single transport.
    let writer = std::sync::Arc::new(std::sync::Mutex::new(writer));

    // Resize-watcher thread: sample the terminal size on its own timer and send a
    // Resize whenever it changes. This MUST be its own thread: the stdin loop
    // below blocks in read() — on Windows ReadConsoleW simply ignores
    // window-resize events, so a size poll placed there only runs after the next
    // keypress, and the UI would not re-fit until you typed. Polling on an
    // independent timer keeps resize responsive regardless of keyboard activity.
    let resize_handle = if auto_resize {
        let writer = writer.clone();
        let done = done.clone();
        Some(std::thread::spawn(move || {
            let mut last_size = size;
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let now = RawTerminal::size();
                if now == last_size {
                    continue;
                }
                last_size = now;
                let msg = ClientMsg::Resize(WireSize {
                    cols: now.cols,
                    rows: now.rows,
                });
                let bytes = match encode(&msg) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let Ok(mut w) = writer.lock() else { break };
                if w.write_frame(&bytes).is_err() {
                    break;
                }
            }
        }))
    } else {
        None
    };

    // Main thread: stdin -> daemon as Input frames. Reads are time-bounded so a
    // detach delivered while the user is idle still ends the client promptly.
    let mut stdin = io::stdin();
    let mut buf = [0u8; 4096];
    let mut input_decoder = OuterInputDecoder::default();
    loop {
        if done.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        // Wait briefly for stdin input; if nothing arrives, loop to re-check the
        // detach flag rather than blocking forever in read().
        match stdin_ready(&stdin, std::time::Duration::from_millis(100)) {
            StdinState::Ready => {}
            StdinState::Idle => {
                // ESC and ESC[ are ambiguous at a read boundary. Once stdin has
                // gone idle they cannot be a fragmented focus report, so release
                // them promptly (notably preserving a literal Escape key).
                let Some(event) = input_decoder.flush_pending() else {
                    continue;
                };
                if write_outer_input(&writer, &applied_frame_epoch, std::iter::once(event)).is_err()
                {
                    break;
                }
                continue;
            }
            StdinState::Closed => break,
        }
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let events = input_decoder.feed(&buf[..n]);
        if write_outer_input(&writer, &applied_frame_epoch, events).is_err() {
            break;
        }
    }
    let _ = reader_handle.join();
    if let Some(h) = resize_handle {
        let _ = h.join();
    }
    // Disable mouse reporting and bracketed paste before the terminal is
    // restored (harmless if they were never enabled).
    {
        use std::io::Write;
        let mut out = io::stdout();
        let _ = out.write_all(lumux_core::mouse::DISABLE.as_bytes());
        let _ = out.write_all(lumux_core::keymap::PASTE_DISABLE.as_bytes());
        let _ = out.flush();
    }
    Ok(())
}

/// Write decoded outer-terminal events to the daemon in their original order.
/// Focus reports cross the protocol as typed state and can therefore never be
/// mistaken for keyboard input by the server or pane.
fn write_outer_input<W: FrameWriter>(
    writer: &std::sync::Mutex<W>,
    applied_frame_epoch: &std::sync::atomic::AtomicU64,
    events: impl IntoIterator<Item = OuterInputEvent>,
) -> anyhow::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("attach transport writer lock poisoned"))?;
    for event in events {
        let msg = outer_input_message(
            event,
            applied_frame_epoch.load(std::sync::atomic::Ordering::Acquire),
        );
        writer.write_frame(&encode(&msg)?)?;
    }
    Ok(())
}

fn outer_input_message(event: OuterInputEvent, applied_frame_epoch: u64) -> ClientMsg {
    match event {
        OuterInputEvent::Input(bytes) => ClientMsg::InputAt {
            bytes,
            frame_epoch: applied_frame_epoch,
        },
        OuterInputEvent::FocusChanged { focused } => ClientMsg::FocusChanged { focused },
    }
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
    use lumux_backend_unix::{default_socket_path, UnixReader, UnixTransport, UnixWriter};
    use lumux_core::traits::Transport;
    use std::path::Path;
    use std::time::{Duration, Instant};

    pub fn connect() -> anyhow::Result<(UnixReader, UnixWriter)> {
        let path = std::env::var_os("LUMUX_SOCK")
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
        anyhow::bail!("failed to connect to lumux server at {}", path.display())
    }

    fn spawn_daemon() -> anyhow::Result<()> {
        super::spawn_daemon_process()
    }
}

#[cfg(windows)]
mod platform {
    use lumux_backend_win::{default_pipe_path, PipeReader, PipeTransport, PipeWriter};
    use lumux_core::traits::Transport;
    use std::time::{Duration, Instant};

    pub fn connect() -> anyhow::Result<(PipeReader, PipeWriter)> {
        let path = std::env::var("LUMUX_PIPE").unwrap_or_else(|_| default_pipe_path());
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
        anyhow::bail!("failed to connect to lumux server pipe {path}")
    }
}

/// Spawn the daemon by re-execing *this* binary with the hidden `--server`
/// flag, detached from the current process/console (tmux's single-binary model).
fn spawn_daemon_process() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // On Windows, fully detach: no console window, separate process group, so
    // the server neither flashes a window nor dies with the client.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x0800_0000) | DETACHED_PROCESS (0x0000_0008)
        cmd.creation_flags(0x0800_0000 | 0x0000_0008);
    }
    // On Unix, put the daemon in its OWN session (setsid) so it leaves the
    // client's process group and drops the controlling terminal. Without this the
    // daemon is a child of the SSH-launched shell's session, so a disconnect
    // SIGHUPs the whole group and kills the daemon — taking the last session with
    // it. We also ignore SIGHUP in the child as a belt-and-suspenders guard.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // New session + process group, detached from the controlling tty.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Ignore SIGHUP so a hangup on the old tty can't reach us.
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                Ok(())
            });
        }
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{outer_input_message, reader_action, ReaderAction};
    use lumux_core::proto::{ClientMsg, Event, ServerMsg};
    use lumux_core::terminal_input::OuterInputEvent;

    #[test]
    fn pane_exit_does_not_end_the_attach() {
        // The bug: exiting a shell in one window of a multi-window session tore
        // down the whole client. The daemon signals that case with PaneExited
        // (the session is still alive), so it must be ignored, not Stop.
        assert_eq!(
            reader_action(ServerMsg::Event(Event::PaneExited {
                pane: "1".into(),
                status: 0,
            })),
            ReaderAction::Ignore,
        );
    }

    #[test]
    fn layout_change_does_not_end_the_attach() {
        assert_eq!(
            reader_action(ServerMsg::Event(Event::LayoutChanged)),
            ReaderAction::Ignore,
        );
    }

    #[test]
    fn session_closed_ends_the_attach() {
        assert_eq!(
            reader_action(ServerMsg::Event(Event::SessionClosed)),
            ReaderAction::Stop,
        );
    }

    #[test]
    fn detached_ends_the_attach() {
        assert_eq!(reader_action(ServerMsg::Detached), ReaderAction::Stop);
    }

    #[test]
    fn bell_writes_the_bel_byte_and_stays_attached() {
        assert_eq!(
            reader_action(ServerMsg::Event(Event::Bell)),
            ReaderAction::Write(vec![0x07]),
        );
    }

    #[test]
    fn frame_is_written_to_stdout() {
        assert_eq!(
            reader_action(ServerMsg::Frame(vec![b'h', b'i'])),
            ReaderAction::Write(vec![b'h', b'i']),
        );
    }

    #[test]
    fn epoch_frame_is_written_to_stdout() {
        assert_eq!(
            reader_action(ServerMsg::FrameAt {
                epoch: 9,
                bytes: vec![b'h', b'i'],
            }),
            ReaderAction::Write(vec![b'h', b'i']),
        );
    }

    #[test]
    fn terminal_input_carries_the_last_flushed_frame_epoch() {
        assert_eq!(
            outer_input_message(OuterInputEvent::Input(b"click".to_vec()), 73),
            ClientMsg::InputAt {
                bytes: b"click".to_vec(),
                frame_epoch: 73,
            }
        );
        assert_eq!(
            outer_input_message(OuterInputEvent::FocusChanged { focused: true }, 73),
            ClientMsg::FocusChanged { focused: true }
        );
    }
}
