//! One-shot control commands (ls / kill-session / kill-server / split-window /
//! new-window / send-keys / source-file).
//!
//! These don't enter raw mode. The byte logic is platform-independent; the
//! per-OS `platform::connect` picks the transport.

use lumux_core::proto::{encode, ClientMsg, Command, ServerMsg, WireSize};
use lumux_core::traits::{FrameReader, FrameWriter};

/// Send a control command and return the daemon's reply text (empty if none).
pub fn send_command(cmd: Command) -> anyhow::Result<String> {
    let (mut reader, mut writer) = match platform::connect() {
        Ok(rw) => rw,
        Err(_) => return Ok("(no server running)\n".to_string()),
    };

    // Protocol handshake before issuing commands.
    crate::attach::handshake(&mut reader, &mut writer)?;

    // Attach so the daemon assigns a client slot, then issue the command.
    let attach = ClientMsg::Attach {
        session: None,
        size: WireSize { cols: 80, rows: 24 },
    };
    writer.write_frame(&encode(&attach)?)?;
    let _ = reader.read_frame()?; // drain the Attached ack / first frame

    // Issue the command, then immediately detach. Detaching is what bounds the
    // read below: most commands (new-window, split-window, send-keys) produce no
    // Reply, only render frames, so waiting for a Reply would block forever once
    // the pane goes idle. The daemon always answers Detach with Detached (then
    // closes the connection), so reading until Detached is guaranteed to finish —
    // and the command is processed before the detach because the daemon handles
    // client messages in order.
    writer.write_frame(&encode(&ClientMsg::Command(cmd))?)?;
    let _ = writer.write_frame(&encode(&ClientMsg::Detach)?);
    let reply = read_until_detached(&mut reader);
    Ok(reply.unwrap_or_default())
}

/// Drain frames until the daemon sends `Detached` (or the connection ends),
/// returning the text of any `Reply` seen along the way. Always terminates:
/// `Detached` is the daemon's guaranteed response to our `Detach`, after which
/// it stops writing, so a subsequent read returns EOF.
fn read_until_detached<R: FrameReader>(reader: &mut R) -> Option<String> {
    let mut reply = None;
    #[allow(clippy::while_let_loop)] // body breaks on Detached, not only on EOF
    loop {
        match reader.read_frame() {
            Ok(Some(bytes)) => match lumux_core::proto::decode::<ServerMsg>(&bytes) {
                Ok(ServerMsg::Reply(text)) => reply = Some(text),
                Ok(ServerMsg::Detached) => break,
                _ => {}
            },
            _ => break, // EOF or decode/read error: nothing more is coming.
        }
    }
    reply
}

#[cfg(unix)]
mod platform {
    use lumux_backend_unix::{default_socket_path, UnixReader, UnixTransport, UnixWriter};
    use lumux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(UnixReader, UnixWriter)> {
        let path = std::env::var_os("LUMUX_SOCK")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(default_socket_path);
        let t = UnixTransport::connect(&path)?;
        Ok(t.split()?)
    }
}

#[cfg(windows)]
mod platform {
    use lumux_backend_win::{default_pipe_path, PipeReader, PipeTransport, PipeWriter};
    use lumux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(PipeReader, PipeWriter)> {
        let path = std::env::var("LUMUX_PIPE").unwrap_or_else(|_| default_pipe_path());
        let t = PipeTransport::connect(&path)?;
        Ok(t.split()?)
    }
}
