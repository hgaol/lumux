//! One-shot control commands (ls / kill-session / kill-server).
//!
//! Unlike attach, these don't enter raw mode. They open a transport, send the
//! command as a NewSession-less control message, and print any reply. To reach
//! the command path the client first sends a lightweight Attach to an existing
//! session if there is one; for server-wide commands (ListSessions, KillServer)
//! it attaches to whatever exists, or reports no daemon.

#![cfg(unix)]

use wmux_backend_unix::{default_socket_path, UnixTransport};
use wmux_core::proto::{encode, ClientMsg, Command, ServerMsg, WireSize};
use wmux_core::traits::{FrameReader, FrameWriter, Transport};

/// Send a control command and return the daemon's reply text (empty if none).
pub fn send_command(cmd: Command) -> anyhow::Result<String> {
    let path = std::env::var_os("WMUX_SOCK")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path);

    let transport = match UnixTransport::connect(&path) {
        Ok(t) => t,
        Err(_) => {
            // No daemon: for ls this is simply "no sessions".
            return Ok("(no server running)\n".to_string());
        }
    };
    let (mut reader, mut writer) = transport.split()?;

    // Attach to an existing session so the daemon assigns us a client slot,
    // then issue the command over that connection.
    let attach = ClientMsg::Attach {
        session: None,
        size: WireSize { cols: 80, rows: 24 },
    };
    writer.write_frame(&encode(&attach)?)?;
    // Drain the Attached ack (and ignore the first frame).
    let _ = reader.read_frame()?;

    writer.write_frame(&encode(&ClientMsg::Command(cmd))?)?;

    // Read until a Reply or a short timeout's worth of frames.
    // (Detach right after so the daemon drops our slot.)
    let reply = read_reply(&mut reader);
    let _ = writer.write_frame(&encode(&ClientMsg::Detach)?);
    Ok(reply.unwrap_or_default())
}

fn read_reply<R: FrameReader>(reader: &mut R) -> Option<String> {
    // Look at a bounded number of frames for a Reply.
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
