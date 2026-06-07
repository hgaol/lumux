//! One-shot control commands (ls / kill-session / kill-server / split-window /
//! new-window / send-keys / source-file).
//!
//! These don't enter raw mode. The byte logic is platform-independent; the
//! per-OS `platform::connect` picks the transport.

use wmux_core::proto::{encode, ClientMsg, Command, ServerMsg, WireSize};
use wmux_core::traits::{FrameReader, FrameWriter};

/// Send a control command and return the daemon's reply text (empty if none).
pub fn send_command(cmd: Command) -> anyhow::Result<String> {
    let (mut reader, mut writer) = match platform::connect() {
        Ok(rw) => rw,
        Err(_) => return Ok("(no server running)\n".to_string()),
    };

    // Attach so the daemon assigns a client slot, then issue the command.
    let attach = ClientMsg::Attach {
        session: None,
        size: WireSize { cols: 80, rows: 24 },
    };
    writer.write_frame(&encode(&attach)?)?;
    let _ = reader.read_frame()?; // drain the Attached ack / first frame

    writer.write_frame(&encode(&ClientMsg::Command(cmd))?)?;
    let reply = read_reply(&mut reader);
    let _ = writer.write_frame(&encode(&ClientMsg::Detach)?);
    Ok(reply.unwrap_or_default())
}

fn read_reply<R: FrameReader>(reader: &mut R) -> Option<String> {
    for _ in 0..16 {
        match reader.read_frame() {
            Ok(Some(bytes)) => {
                if let Ok(ServerMsg::Reply(text)) = wmux_core::proto::decode::<ServerMsg>(&bytes) {
                    return Some(text);
                }
            }
            _ => break,
        }
    }
    None
}

#[cfg(unix)]
mod platform {
    use wmux_backend_unix::{default_socket_path, UnixReader, UnixTransport, UnixWriter};
    use wmux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(UnixReader, UnixWriter)> {
        let path = std::env::var_os("WMUX_SOCK")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(default_socket_path);
        let t = UnixTransport::connect(&path)?;
        Ok(t.split()?)
    }
}

#[cfg(windows)]
mod platform {
    use wmux_backend_win::{default_pipe_path, PipeReader, PipeTransport, PipeWriter};
    use wmux_core::traits::Transport;

    pub fn connect() -> anyhow::Result<(PipeReader, PipeWriter)> {
        let path = std::env::var("WMUX_PIPE").unwrap_or_else(|_| default_pipe_path());
        let t = PipeTransport::connect(&path)?;
        Ok(t.split()?)
    }
}
