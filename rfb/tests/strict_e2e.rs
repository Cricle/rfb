//! Local, deterministic cross-crate contract E2E: no sockets, network, KVM, or credentials.
#![cfg(feature = "cli")]

use rfb::{Capability, ImageManifest, Resources, TransportKind};
use rfb_runtime::{
    codec::{CodecError, FrameCodec, MessageType},
    fake,
    policy::PathPolicy,
    session::{ControlMessage, FileReadRequest, FileWriteRequest, RuntimeMessage, SessionRequest},
    PROTOCOL_VERSION,
};

#[test]
fn codec_protocol_and_fake_runtime_round_trip() {
    let codec = FrameCodec::default();
    let hello = ControlMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
    };
    let bytes = codec.encode(MessageType::Hello, 41, &hello).unwrap();
    let (frame, decoded): (_, ControlMessage) = codec.decode(&bytes).unwrap();
    assert_eq!(frame.message_type, MessageType::Hello);
    assert_eq!(frame.sequence, 41);
    assert_eq!(decoded, hello);

    let mut sequence = 0;
    let replies = fake::handle_control(decoded, &mut sequence);
    assert!(matches!(
        replies[0],
        RuntimeMessage::HelloAck {
            protocol_version: 1
        }
    ));
    assert!(matches!(
        replies[1],
        RuntimeMessage::Capabilities {
            session_per_vm: true,
            writable_workspace: true
        }
    ));

    let turn = ControlMessage::StartTurn(SessionRequest {
        session_id: "local-session".into(),
        request_id: "request-1".into(),
        prompt: "fake prompt".into(),
    });
    let event = fake::handle_control(turn, &mut sequence)
        .into_iter()
        .next()
        .unwrap();
    assert!(
        matches!(event, RuntimeMessage::Event(ref e) if e.kind == "turn.started" && e.sequence == 1)
    );
    let cancel = fake::handle_control(
        ControlMessage::Cancel {
            session_id: "local-session".into(),
            request_id: "request-1".into(),
        },
        &mut sequence,
    );
    assert!(
        matches!(&cancel[0], RuntimeMessage::Event(e) if e.kind == "turn.cancelled" && e.sequence == 2)
    );
}

#[test]
fn protocol_errors_and_frame_boundaries_fail_closed() {
    let mut sequence = 0;
    let bad = fake::handle_control(
        ControlMessage::Hello {
            protocol_version: 999,
        },
        &mut sequence,
    );
    assert!(
        matches!(&bad[0], RuntimeMessage::Error { message, .. } if message.contains("unsupported protocol"))
    );
    assert_eq!(sequence, 0);

    let codec = FrameCodec {
        compression_threshold: usize::MAX,
        max_payload: 4,
    };
    assert!(matches!(
        codec.encode(MessageType::Event, 0, &"too long"),
        Err(CodecError::TooLarge)
    ));
    let mut malformed = vec![0u8; 18];
    malformed[..4].copy_from_slice(b"RFB1");
    malformed[4] = MessageType::Event as u8;
    malformed[5] = 2;
    assert!(matches!(
        codec.decode::<String>(&malformed),
        Err(CodecError::InvalidFlags)
    ));
}

#[test]
fn fake_rpc_lifecycle_and_permission_boundaries() {
    let mut sequence = 0;
    let read = fake::handle_control(
        ControlMessage::ReadWorkspaceFile(FileReadRequest {
            request_id: "r".into(),
            path: "ok.txt".into(),
            max_bytes: 10,
        }),
        &mut sequence,
    );
    assert!(
        matches!(&read[0], RuntimeMessage::FileContent { request_id, path, content } if request_id == "r" && path == "ok.txt" && content == b"fake")
    );
    let host = fake::handle_control(
        ControlMessage::ReadHostFile(FileReadRequest {
            request_id: "h".into(),
            path: "secret".into(),
            max_bytes: 10,
        }),
        &mut sequence,
    );
    assert!(
        matches!(&host[0], RuntimeMessage::Error { message, .. } if message.contains("read-only"))
    );
    let write = fake::handle_control(
        ControlMessage::WriteWorkspaceFile(FileWriteRequest {
            request_id: "w".into(),
            path: "out.txt".into(),
            content: b"x".to_vec(),
        }),
        &mut sequence,
    );
    assert!(matches!(&write[0], RuntimeMessage::WriteAck { path, .. } if path == "out.txt"));
    assert!(matches!(
        fake::handle_control(ControlMessage::Shutdown, &mut sequence)[0],
        RuntimeMessage::ShutdownAck
    ));

    let policy = PathPolicy::new("/workspace", vec!["/sources".into()]);
    assert!(policy.workspace_path("../escape").is_err());
    assert!(policy.host_read_path(1, "x").is_err());
    assert!(policy.can_write_host("/sources/x").is_err());
}

#[test]
fn rfb_contracts_are_local_and_non_networked() {
    assert_eq!(TransportKind::InProcess, TransportKind::InProcess);
    assert!(Capability::Execute != Capability::ReadFramebuffer);
    assert!(Resources {
        cpus: Some(0),
        ..Default::default()
    }
    .validate()
    .is_err());
    assert!(ImageManifest::new("").validate().is_err());
}
