use super::*;

fn roundtrip_client(msg: ClientMsg) {
    let frame = encode(&msg).unwrap();
    // Strip the 4-byte length prefix to get the body for decode().
    let body = &frame[4..];
    let back: ClientMsg = decode(body).unwrap();
    assert_eq!(msg, back);
}

fn roundtrip_server(msg: ServerMsg) {
    let frame = encode(&msg).unwrap();
    let back: ServerMsg = decode(&frame[4..]).unwrap();
    assert_eq!(msg, back);
}

#[test]
fn client_messages_roundtrip() {
    roundtrip_client(ClientMsg::Attach {
        session: Some("work".into()),
        size: WireSize { cols: 80, rows: 24 },
    });
    roundtrip_client(ClientMsg::NewSession {
        name: None,
        shell: Some("ps5".into()),
        size: WireSize {
            cols: 100,
            rows: 30,
        },
    });
    roundtrip_client(ClientMsg::Input(vec![0x1b, b'[', b'A']));
    roundtrip_client(ClientMsg::Resize(WireSize { cols: 1, rows: 1 }));
    roundtrip_client(ClientMsg::Detach);
    roundtrip_client(ClientMsg::Command(Command::ListSessions));
    roundtrip_client(ClientMsg::Command(Command::SplitWindow {
        horizontal: true,
    }));
    roundtrip_client(ClientMsg::Command(Command::KillSession {
        target: "$1".into(),
    }));
    roundtrip_client(ClientMsg::Command(Command::SendKeys {
        keys: b"echo hi\r".to_vec(),
    }));
}

#[test]
fn server_messages_roundtrip() {
    roundtrip_server(ServerMsg::Attached {
        client_id: 7,
        size: WireSize { cols: 80, rows: 24 },
    });
    roundtrip_server(ServerMsg::Frame(vec![0x1b, b'[', b'2', b'J']));
    roundtrip_server(ServerMsg::Event(Event::LayoutChanged));
    roundtrip_server(ServerMsg::Event(Event::PaneExited {
        pane: "%3".into(),
        status: 0,
    }));
    roundtrip_server(ServerMsg::Event(Event::Bell));
    roundtrip_server(ServerMsg::Reply("sessions:\n$1 work".into()));
    roundtrip_server(ServerMsg::Detached);
    roundtrip_server(ServerMsg::Error("no such session".into()));
}

#[test]
fn codec_reassembles_frame_split_across_reads() {
    let msg = ServerMsg::Frame(b"hello world".to_vec());
    let frame = encode(&msg).unwrap();
    let mut codec = FrameCodec::new();
    // Feed one byte at a time; only the last byte completes the frame.
    for (i, b) in frame.iter().enumerate() {
        codec.extend(&[*b]);
        let got: Option<ServerMsg> = codec.next_message().unwrap();
        if i + 1 < frame.len() {
            assert!(
                got.is_none(),
                "incomplete frame must yield None at byte {i}"
            );
        } else {
            assert_eq!(got, Some(msg.clone()));
        }
    }
}

#[test]
fn codec_yields_multiple_frames_from_one_buffer() {
    let a = ClientMsg::Input(b"a".to_vec());
    let b = ClientMsg::Input(b"bb".to_vec());
    let mut bytes = encode(&a).unwrap();
    bytes.extend(encode(&b).unwrap());
    let mut codec = FrameCodec::new();
    codec.extend(&bytes);
    let m1: ClientMsg = codec.next_message().unwrap().unwrap();
    let m2: ClientMsg = codec.next_message().unwrap().unwrap();
    assert_eq!(m1, a);
    assert_eq!(m2, b);
    let none: Option<ClientMsg> = codec.next_message().unwrap();
    assert!(none.is_none());
}

#[test]
fn codec_rejects_oversized_length_prefix() {
    let mut codec = FrameCodec::new();
    // Length prefix claims 1 GiB.
    codec.extend(&(1_000_000_000u32).to_be_bytes());
    codec.extend(&[0u8; 8]);
    assert!(matches!(codec.next_frame(), Err(FrameError::TooLarge(_))));
}

#[test]
fn handshake_accepts_same_version() {
    let h = Hello::current("wmuxd/0.1.0");
    assert!(h.check().is_ok());
    let frame = encode(&h).unwrap();
    let back: Hello = decode(&frame[4..]).unwrap();
    assert_eq!(back.protocol_version, PROTOCOL_VERSION);
}

#[test]
fn handshake_rejects_version_mismatch() {
    let mut h = Hello::current("old-client");
    h.protocol_version = PROTOCOL_VERSION + 99;
    let err = h.check().unwrap_err();
    assert_eq!(err.theirs, PROTOCOL_VERSION + 99);
    assert_eq!(err.ours, PROTOCOL_VERSION);
}

#[test]
fn wire_size_converts_to_pty_size() {
    let ws = WireSize {
        cols: 120,
        rows: 40,
    };
    let ps: crate::traits::PtySize = ws.into();
    assert_eq!(ps.cols, 120);
    assert_eq!(ps.rows, 40);
    let back: WireSize = ps.into();
    assert_eq!(back, ws);
}
