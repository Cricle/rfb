#![cfg(feature = "guest")]

//! Guest protocol tests: decoding guest-to-host message streams with strict
//! type checking, and async control/runtime round trips
//! over an in-process duplex transport (no real guest required).

use rfb_runtime::codec::{read_frame, write_frame, FrameCodec, MessageType};
use rfb_runtime::guest_protocol::{
    decode_guest_messages, decode_guest_messages_with_sequence, read_runtime_message,
    read_runtime_message_with_sequence, write_control_message,
};
use rfb_runtime::session::{ControlMessage, RuntimeMessage, SessionEvent, SessionRequest};

/// Wire length prefix + frame helper used to build guest-direction buffers.
fn framed(
    codec: &FrameCodec,
    message_type: MessageType,
    sequence: u64,
    value: &impl serde::Serialize,
) -> Vec<u8> {
    let frame = codec.encode(message_type, sequence, value).unwrap();
    let mut wire = (frame.len() as u32).to_le_bytes().to_vec();
    wire.extend_from_slice(&frame);
    wire
}

fn event(session_id: &str, sequence: u64, kind: &str, payload: Vec<u8>) -> RuntimeMessage {
    RuntimeMessage::Event(SessionEvent {
        session_id: session_id.into(),
        sequence,
        kind: kind.into(),
        payload,
    })
}

#[test]
fn decode_empty_input_yields_no_messages() {
    let codec = FrameCodec::default();
    assert!(decode_guest_messages(&codec, &[]).unwrap().is_empty());
    assert!(decode_guest_messages_with_sequence(&codec, &[])
        .unwrap()
        .is_empty());
}

#[test]
fn decode_rejects_incomplete_frame_length() {
    let codec = FrameCodec::default();
    let error = decode_guest_messages(&codec, &[0, 0]).unwrap_err();
    assert!(error.to_string().contains("incomplete"));
}

#[test]
fn decode_rejects_incomplete_frame_payload() {
    let codec = FrameCodec::default();
    // Declares a 100-byte payload but only provides 3 bytes.
    let mut wire = 100u32.to_le_bytes().to_vec();
    wire.extend_from_slice(&[0u8; 3]);
    let error = decode_guest_messages(&codec, &wire).unwrap_err();
    assert!(error.to_string().contains("incomplete"));
}

#[test]
fn decode_rejects_input_from_the_wrong_direction() {
    let codec = FrameCodec::default();
    // StartTurn is a host-to-guest request opcode. The payload still decodes as
    // a RuntimeMessage, so the strict type check must reject the frame.
    let wire = framed(
        &codec,
        MessageType::StartTurn,
        3,
        &RuntimeMessage::ShutdownAck,
    );
    let error = decode_guest_messages(&codec, &wire).unwrap_err();
    assert!(error.to_string().contains("unexpected"));
}

#[test]
fn decode_yields_sequenced_guest_messages_in_order() {
    let codec = FrameCodec::default();
    let started = event("s1", 1, "turn.started", vec![]);
    let completed = event("s1", 2, "turn.completed", b"done".to_vec());
    let mut wire = framed(&codec, MessageType::Event, 1, &started);
    wire.extend(framed(&codec, MessageType::Event, 2, &completed));

    let messages = decode_guest_messages(&codec, &wire).unwrap();
    assert_eq!(messages, vec![started.clone(), completed.clone()]);

    let sequenced = decode_guest_messages_with_sequence(&codec, &wire).unwrap();
    assert_eq!(sequenced.len(), 2);
    assert_eq!(sequenced[0].sequence, 1);
    assert_eq!(sequenced[1].sequence, 2);
    assert_eq!(sequenced[0].message, started);
    assert_eq!(sequenced[1].message, completed);
}

#[test]
fn decode_accepts_all_guest_direction_message_types() {
    let codec = FrameCodec::default();
    let messages = [
        (
            MessageType::HelloAck,
            RuntimeMessage::HelloAck {
                protocol_version: 1,
            },
        ),
        (
            MessageType::Capabilities,
            RuntimeMessage::Capabilities {
                session_per_vm: true,
                writable_workspace: true,
            },
        ),
        (MessageType::Event, event("s", 1, "turn.started", vec![])),
        (
            MessageType::Error,
            RuntimeMessage::Error {
                request_id: "r".into(),
                message: "x".into(),
            },
        ),
        (
            MessageType::FileContent,
            RuntimeMessage::FileContent {
                request_id: "f".into(),
                path: "a.txt".into(),
                content: b"data".to_vec(),
            },
        ),
        (
            MessageType::WriteAck,
            RuntimeMessage::WriteAck {
                request_id: "w".into(),
                path: "b.txt".into(),
            },
        ),
        (MessageType::Shutdown, RuntimeMessage::ShutdownAck),
    ];
    for (i, (message_type, message)) in messages.iter().enumerate() {
        let wire = framed(&codec, *message_type, i as u64, message);
        let decoded = decode_guest_messages(&codec, &wire).unwrap();
        assert_eq!(decoded, vec![message.clone()], "{message_type:?}");
    }
}

#[tokio::test]
async fn async_read_rejects_wrong_direction_wire_types() {
    let codec = FrameCodec::default();
    let (mut host, mut guest) = tokio::io::duplex(4096);
    // Payload decodes as a RuntimeMessage but the opcode is host-direction only.
    write_frame(
        &mut guest,
        &codec,
        MessageType::StartTurn,
        1,
        &RuntimeMessage::ShutdownAck,
    )
    .await
    .unwrap();
    let error = read_runtime_message_with_sequence(&mut host, &codec)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("unexpected"));
}

#[tokio::test]
async fn async_control_and_runtime_messages_round_trip() {
    let codec = FrameCodec::default();
    let (mut host, mut guest) = tokio::io::duplex(4096);

    // Host -> guest: a control message is carried with its request opcode.
    let control = ControlMessage::StartTurn(SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: "run".into(),
    });
    write_control_message(&mut host, &codec, &control, 7)
        .await
        .unwrap();
    let (frame, decoded): (_, ControlMessage) = read_frame(&mut guest, &codec).await.unwrap();
    assert_eq!(frame.message_type, MessageType::StartTurn);
    assert_eq!(frame.sequence, 7);
    assert_eq!(decoded, control);

    // Guest -> host: a runtime message is read back with its sequence.
    let response = event("s", 8, "turn.completed", b"ok".to_vec());
    write_frame(&mut guest, &codec, MessageType::Event, 8, &response)
        .await
        .unwrap();
    let (sequence, decoded): (u64, RuntimeMessage) =
        read_runtime_message_with_sequence(&mut host, &codec)
            .await
            .unwrap();
    assert_eq!(sequence, 8);
    assert_eq!(decoded, response);

    // Closing the guest side surfaces EOF on the host reader instead of hanging.
    drop(guest);
    let error = read_runtime_message(&mut host, &codec).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}
