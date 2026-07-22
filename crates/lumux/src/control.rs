//! One-shot control commands (ls / kill-session / kill-server / split-window /
//! new-window / send-keys / source-file).
//!
//! These don't enter raw mode. The byte logic is platform-independent; the
//! per-OS `platform::connect` picks the transport.

use lumux_core::proto::{encode, ClientMsg, Command, ControlRequest, ServerMsg};
use lumux_core::traits::{FrameReader, FrameWriter};

/// Send a control command and return the daemon's reply text (empty if none).
pub fn send_command(cmd: Command) -> anyhow::Result<String> {
    let (mut reader, mut writer) = match platform::connect() {
        Ok(rw) => rw,
        Err(_) => return Ok("(no server running)\n".to_string()),
    };

    // Protocol handshake before issuing commands.
    crate::attach::handshake(&mut reader, &mut writer)?;

    // A one-shot control request is deliberately not an Attach. It carries no
    // viewport, never enters the interactive client registry, and therefore
    // cannot participate in smallest-client-wins sizing or receive VT frames.
    // `$LUMUX_PANE` supplies caller context for session-scoped CLI commands.
    let pane = std::env::var_os("LUMUX_PANE")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse().ok());
    writer.write_frame(&encode(&ClientMsg::Control(ControlRequest {
        command: cmd,
        pane,
    }))?)?;
    let reply = read_until_detached(&mut reader);
    Ok(reply.unwrap_or_default())
}

/// Drain frames until the daemon sends `Detached` (or the connection ends),
/// returning the text of any `Reply` seen along the way. Always terminates:
/// `Detached` terminates every one-shot [`ClientMsg::Control`] request, after
/// which the server closes this control connection.
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
    use lumux_backend_unix::{UnixReader, UnixTransport, UnixWriter};
    use lumux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(UnixReader, UnixWriter)> {
        let path = crate::runtime::socket_path();
        let t = UnixTransport::connect(&path)?;
        Ok(t.split()?)
    }
}

#[cfg(windows)]
mod platform {
    use lumux_backend_win::{PipeReader, PipeTransport, PipeWriter};
    use lumux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(PipeReader, PipeWriter)> {
        let path = crate::runtime::pipe_path();
        let t = PipeTransport::connect(&path)?;
        Ok(t.split()?)
    }
}
