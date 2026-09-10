use rfb_runtime::codec::{FrameCodec, MessageType};
use rfb_runtime::fake::{handle_control, PROTOCOL_VERSION};
use rfb_runtime::policy::{PathPolicy, PolicyError};
use rfb_runtime::session::{ControlMessage, RuntimeMessage, SessionRequest};

#[test]
fn codec_round_trips_through_blocking_length_delimited_api() {
    let codec = FrameCodec::default();
    let mut wire = Vec::new();
    rfb_runtime::codec::write_frame_blocking(
        &mut wire,
        &codec,
        MessageType::Capabilities,
        23,
        &"ready".to_string(),
    )
    .unwrap();
    let (frame, value): (_, String) =
        rfb_runtime::codec::read_frame_blocking(&mut wire.as_slice(), &codec).unwrap();
    assert_eq!(frame.message_type, MessageType::Capabilities);
    assert_eq!(frame.sequence, 23);
    assert_eq!(value, "ready");
}

#[test]
fn codec_round_trips_small_and_compressed_payloads() {
    let codec = FrameCodec {
        compression_threshold: 8,
        max_payload: 1024 * 1024,
    };
    let small = "hello".to_string();
    let (frame, decoded): (_, String) = codec
        .decode(&codec.encode(MessageType::Hello, 7, &small).unwrap())
        .unwrap();
    assert_eq!(frame.sequence, 7);
    assert!(!frame.compressed);
    assert_eq!(decoded, small);

    let large = "x".repeat(4096);
    let (frame, decoded): (_, String) = codec
        .decode(&codec.encode(MessageType::Event, 8, &large).unwrap())
        .unwrap();
    assert!(frame.compressed);
    assert_eq!(decoded, large);
}

#[test]
fn codec_rejects_noncanonical_legacy_frame_magic() {
    let codec = FrameCodec::default();
    let mut frame = codec
        .encode(
            MessageType::Hello,
            1,
            &ControlMessage::Hello {
                protocol_version: 1,
            },
        )
        .unwrap();
    frame[..4].copy_from_slice(b"OLD!");
    let error = codec
        .decode::<ControlMessage>(&frame)
        .expect_err("non-canonical magic must be rejected");
    assert!(matches!(error, rfb_runtime::codec::CodecError::Magic));
}

#[test]
fn fake_runtime_handles_session_lifecycle() {
    let mut sequence = 0;
    let hello = handle_control(
        ControlMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
        &mut sequence,
    );
    assert!(matches!(
        hello.first(),
        Some(RuntimeMessage::HelloAck { .. })
    ));
    assert!(matches!(
        hello.get(1),
        Some(RuntimeMessage::Capabilities {
            session_per_vm: true,
            writable_workspace: true
        })
    ));

    let events = handle_control(
        ControlMessage::StartTurn(SessionRequest {
            session_id: "s1".into(),
            request_id: "r1".into(),
            prompt: "hello".into(),
        }),
        &mut sequence,
    );
    assert!(
        matches!(events.first(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.started" && event.sequence == 1)
    );
    let cancelled = handle_control(
        ControlMessage::Cancel {
            session_id: "s1".into(),
            request_id: "r1".into(),
        },
        &mut sequence,
    );
    assert!(
        matches!(cancelled.first(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.cancelled" && event.sequence == 2)
    );
}

#[test]
fn wire_frames_preserve_control_request_and_guest_response() {
    let codec = FrameCodec::default();
    let request = ControlMessage::StartTurn(SessionRequest {
        session_id: "s-wire".into(),
        request_id: "r-wire".into(),
        prompt: "hello wire".into(),
    });
    let encoded = codec.encode(MessageType::StartTurn, 41, &request).unwrap();
    let mut wire = (encoded.len() as u32).to_le_bytes().to_vec();
    wire.extend_from_slice(&encoded);
    let frame_len = u32::from_le_bytes(wire[..4].try_into().unwrap()) as usize;
    let (header, decoded): (_, ControlMessage) = codec.decode(&wire[4..4 + frame_len]).unwrap();
    assert_eq!(header.message_type, MessageType::StartTurn);
    assert_eq!(header.sequence, 41);
    assert_eq!(decoded, request);

    let response = RuntimeMessage::Event(rfb_runtime::session::SessionEvent {
        session_id: "s-wire".into(),
        sequence: 42,
        kind: "turn.started".into(),
        payload: b"ok".to_vec(),
    });
    let encoded = codec.encode(MessageType::Event, 42, &response).unwrap();
    let mut response_wire = (encoded.len() as u32).to_le_bytes().to_vec();
    response_wire.extend_from_slice(&encoded);
    let decoded =
        rfb_runtime::guest_protocol::decode_guest_messages(&codec, &response_wire).unwrap();
    assert_eq!(decoded, vec![response]);
    let sequenced =
        rfb_runtime::guest_protocol::decode_guest_messages_with_sequence(&codec, &response_wire)
            .unwrap();
    assert_eq!(sequenced[0].sequence, 42);
}

#[test]
fn wire_frame_error_type_is_distinct_from_event() {
    let codec = FrameCodec::default();
    let error = RuntimeMessage::Error {
        request_id: "r1".into(),
        message: "execution failed".into(),
    };
    let encoded = codec.encode(MessageType::Error, 7, &error).unwrap();
    let (header, decoded): (_, RuntimeMessage) = codec.decode(&encoded).unwrap();
    assert_eq!(header.message_type, MessageType::Error);
    assert_eq!(decoded, error);

    let mut wire = (encoded.len() as u32).to_le_bytes().to_vec();
    wire.extend_from_slice(&encoded);
    // The guest protocol lets a guest surface an explicit Error frame.
    let decoded = rfb_runtime::guest_protocol::decode_guest_messages(&codec, &wire).unwrap();
    assert_eq!(decoded, vec![error]);
}

#[test]
fn codec_rejects_invalid_flags_and_trailing_bytes() {
    let codec = FrameCodec::default();
    let value = "payload".to_string();
    let mut encoded = codec.encode(MessageType::Event, 1, &value).unwrap();
    encoded[5] = 2;
    assert!(matches!(
        codec.decode::<String>(&encoded),
        Err(rfb_runtime::codec::CodecError::InvalidFlags)
    ));

    let mut encoded = codec.encode(MessageType::Event, 1, &value).unwrap();
    encoded.push(0);
    assert!(matches!(
        codec.decode::<String>(&encoded),
        Err(rfb_runtime::codec::CodecError::Trailing)
    ));
}

#[test]
fn codec_rejects_payload_above_raw_limit_even_when_compressible() {
    let codec = FrameCodec {
        compression_threshold: 1,
        max_payload: 32,
    };
    let result = codec.encode(MessageType::Event, 1, &"x".repeat(4096));
    assert!(matches!(
        result,
        Err(rfb_runtime::codec::CodecError::TooLarge)
    ));
}

#[test]
fn hello_ack_uses_its_own_wire_type() {
    let codec = FrameCodec::default();
    let response = RuntimeMessage::HelloAck {
        protocol_version: 1,
    };
    let encoded = codec.encode(MessageType::HelloAck, 3, &response).unwrap();
    let (frame, decoded): (_, RuntimeMessage) = codec.decode(&encoded).unwrap();
    assert_eq!(frame.message_type, MessageType::HelloAck);
    assert_eq!(decoded, response);
}

#[test]
fn cancel_wire_round_trip_preserves_exact_request_identity() {
    let codec = FrameCodec::default();
    let cancel = ControlMessage::Cancel {
        session_id: "session-a".into(),
        request_id: "request-7".into(),
    };
    let encoded = codec.encode(MessageType::CancelTurn, 19, &cancel).unwrap();
    let (header, decoded): (_, ControlMessage) = codec.decode(&encoded).unwrap();
    assert_eq!(header.message_type, MessageType::CancelTurn);
    assert_eq!(header.sequence, 19);
    assert_eq!(decoded, cancel);
}

#[test]
fn unknown_wire_frame_type_is_rejected() {
    let codec = FrameCodec::default();
    let error = RuntimeMessage::Error {
        request_id: "r1".into(),
        message: "boom".into(),
    };
    let mut encoded = codec.encode(MessageType::Error, 1, &error).unwrap();
    // Corrupt the type byte into a value that is not assigned.
    encoded[4] = 42;
    let result = codec.decode::<RuntimeMessage>(&encoded);
    assert!(matches!(
        result,
        Err(rfb_runtime::codec::CodecError::MessageType(42))
    ));
}

#[test]
fn policy_rejects_absolute_and_unknown_host_paths() {
    let policy = PathPolicy::new("/vm/workspace", vec!["/host/source".into()]);
    assert!(matches!(
        policy.workspace_path("/etc/passwd"),
        Err(PolicyError::Escape)
    ));
    assert!(matches!(
        policy.host_read_path(1, "README"),
        Err(PolicyError::NotAllowed)
    ));
    assert!(matches!(
        policy.host_read_path(0, "../secret"),
        Err(PolicyError::Escape)
    ));
}

#[test]
fn policy_allows_workspace_and_readonly_host_only() {
    let policy = PathPolicy::new("/vm/workspace", vec!["/host/source".into()]);
    assert_eq!(
        policy.workspace_path("src/main.rs").unwrap(),
        std::path::PathBuf::from("/vm/workspace/src/main.rs")
    );
    assert_eq!(
        policy.host_read_path(0, "README.md").unwrap(),
        std::path::PathBuf::from("/host/source/README.md")
    );
    assert!(matches!(
        policy.workspace_path("../escape"),
        Err(PolicyError::Escape)
    ));
    assert!(matches!(
        policy.can_write_host("README.md"),
        Err(PolicyError::ReadOnly)
    ));
}

#[test]
fn runtime_rejects_empty_control_identity() {
    let mut service = rfb_runtime::runtime_service::RuntimeService::from_environment();
    service.handle(ControlMessage::Hello {
        protocol_version: 1,
    });
    let responses = service.handle(ControlMessage::StartTurn(SessionRequest {
        session_id: "session-a".into(),
        request_id: "".into(),
        prompt: "hello".into(),
    }));
    assert!(
        matches!(responses.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("non-empty"))
    );
}

#[test]
fn structured_file_rpc_round_trips_and_fail_closes_without_executor_support() {
    use rfb_runtime::session::{FileReadRequest, FileWriteRequest};

    let codec = FrameCodec::default();
    let read = ControlMessage::ReadWorkspaceFile(FileReadRequest {
        request_id: "read-1".into(),
        path: "src/lib.rs".into(),
        max_bytes: 4096,
    });
    let encoded = codec
        .encode(MessageType::ReadWorkspaceFile, 7, &read)
        .unwrap();
    let (frame, decoded): (_, ControlMessage) = codec.decode(&encoded).unwrap();
    assert_eq!(frame.message_type, MessageType::ReadWorkspaceFile);
    assert_eq!(decoded, read);

    let write = ControlMessage::WriteWorkspaceFile(FileWriteRequest {
        request_id: "write-1".into(),
        path: "out.txt".into(),
        content: b"hello".to_vec(),
    });
    let mut service = rfb_runtime::runtime_service::RuntimeService::from_environment();
    service.handle(ControlMessage::Hello {
        protocol_version: rfb_runtime::PROTOCOL_VERSION,
    });
    let response = service.handle(write);
    assert!(
        matches!(response.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("not supported"))
    );
}

#[test]
fn structured_file_responses_use_dedicated_wire_types() {
    use rfb_runtime::session::RuntimeMessage;
    let codec = FrameCodec::default();
    let content = RuntimeMessage::FileContent {
        request_id: "r".into(),
        path: "a.txt".into(),
        content: b"data".to_vec(),
    };
    let bytes = codec.encode(MessageType::FileContent, 9, &content).unwrap();
    let (frame, decoded): (_, RuntimeMessage) = codec.decode(&bytes).unwrap();
    assert_eq!(frame.message_type, MessageType::FileContent);
    assert_eq!(decoded, content);
}
