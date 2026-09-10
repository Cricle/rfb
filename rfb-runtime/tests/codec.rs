//! Strict RFB1 codec tests: frame encode/decode for every wire type,
//! compression threshold boundaries, resource-limit enforcement, wire-format
//! rejection, and blocking/async length-delimited framed I/O.

use rfb_runtime::codec::{
    read_frame, read_frame_blocking, write_frame, write_frame_blocking, CodecError, Frame,
    FrameCodec, MessageType,
};
use rfb_runtime::resources::RuntimeLimits;

/// RFB1 header: magic (4) + message type (1) + flags (1) + sequence (8) + len (4).
const HEADER_LEN: usize = 18;

#[test]
fn all_message_types_round_trip_through_encode_decode() {
    let codec = FrameCodec::default();
    let cases = [
        (MessageType::Hello, "hello"),
        (MessageType::Capabilities, "cap"),
        (MessageType::StartTurn, "start"),
        (MessageType::CancelTurn, "cancel"),
        (MessageType::Event, "event"),
        (MessageType::ReadHostFile, "read-host"),
        (MessageType::ReadWorkspaceFile, "read-ws"),
        (MessageType::WriteWorkspaceFile, "write-ws"),
        (MessageType::Shutdown, "shutdown"),
        (MessageType::Error, "error"),
        (MessageType::HelloAck, "ack"),
        (MessageType::FileContent, "content"),
        (MessageType::WriteAck, "write-ack"),
    ];
    for (message_type, payload) in cases {
        let bytes = codec.encode(message_type, 9, &payload).unwrap();
        let (frame, decoded): (Frame, String) = codec.decode(&bytes).unwrap();
        assert_eq!(frame.message_type, message_type);
        assert_eq!(frame.sequence, 9);
        assert!(
            !frame.compressed,
            "{message_type:?} small payload must stay raw"
        );
        assert_eq!(frame.payload, postcard::to_allocvec(&payload).unwrap());
        assert_eq!(decoded, payload);
    }
}

#[test]
fn compression_flips_exactly_at_the_threshold_boundary() {
    // postcard encodes a string as varint length + bytes, so a payload of N
    // chars produces N+1 postcard bytes. threshold=100: byte length 99 raw,
    // byte length 100 compressed.
    let codec = FrameCodec {
        compression_threshold: 100,
        max_payload: 1024 * 1024,
    };
    let below = codec
        .encode(MessageType::Event, 1, &"a".repeat(98))
        .unwrap();
    let (frame, decoded): (Frame, String) = codec.decode(&below).unwrap();
    assert!(
        !frame.compressed,
        "payload below the threshold must stay raw"
    );
    assert_eq!(decoded.len(), 98);

    let at = codec
        .encode(MessageType::Event, 1, &"a".repeat(99))
        .unwrap();
    let (frame, decoded): (Frame, String) = codec.decode(&at).unwrap();
    assert!(frame.compressed, "payload at the threshold must compress");
    assert_eq!(decoded.len(), 99);
}

#[test]
fn decode_rejects_frames_shorter_than_the_header() {
    let codec = FrameCodec::default();
    for len in 0..HEADER_LEN {
        let bytes = vec![0u8; len];
        let error = codec.decode::<String>(&bytes).unwrap_err();
        assert!(matches!(error, CodecError::Truncated));
    }
    let error = codec.decode::<String>(&[]).unwrap_err();
    assert!(matches!(error, CodecError::Truncated));
}

#[test]
fn decode_rejects_header_declaring_more_bytes_than_present() {
    let codec = FrameCodec::default();
    let bytes = codec
        .encode(MessageType::Event, 1, &"truncate me".to_string())
        .unwrap();
    // Drop part of the payload: the declared length now exceeds the buffer.
    let truncated = &bytes[..bytes.len() - 5];
    let error = codec.decode::<String>(truncated).unwrap_err();
    assert!(matches!(error, CodecError::Truncated));
}

#[test]
fn decode_rejects_declared_length_over_max_payload() {
    let codec = FrameCodec {
        compression_threshold: 1024,
        max_payload: 64,
    };
    let mut bytes = b"RFB1".to_vec();
    bytes.push(MessageType::Event as u8);
    bytes.push(0); // flags: uncompressed
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&65u32.to_le_bytes()); // len > max_payload, no payload present
    let error = codec.decode::<String>(&bytes).unwrap_err();
    assert!(matches!(error, CodecError::Truncated));
}

#[test]
fn decode_rejects_trailing_bytes_after_a_valid_frame() {
    let codec = FrameCodec::default();
    let encoded = codec
        .encode(MessageType::Event, 1, &"payload".to_string())
        .unwrap();
    let mut with_trailing = encoded.clone();
    with_trailing.push(0);
    let error = codec.decode::<String>(&with_trailing).unwrap_err();
    assert!(matches!(error, CodecError::Trailing));

    let mut double = encoded.clone();
    double.extend_from_slice(&encoded);
    let error = codec.decode::<String>(&double).unwrap_err();
    assert!(matches!(error, CodecError::Trailing));
}

#[test]
fn decode_passes_then_rejects_unknown_opcode_bytes() {
    let codec = FrameCodec::default();
    for (byte, expected) in [
        (1u8, MessageType::Hello),
        (2, MessageType::Capabilities),
        (3, MessageType::StartTurn),
        (4, MessageType::CancelTurn),
        (5, MessageType::Event),
        (6, MessageType::ReadHostFile),
        (7, MessageType::ReadWorkspaceFile),
        (8, MessageType::WriteWorkspaceFile),
        (9, MessageType::Shutdown),
        (10, MessageType::Error),
        (11, MessageType::HelloAck),
        (12, MessageType::FileContent),
        (13, MessageType::WriteAck),
    ] {
        let mut bytes = b"RFB1".to_vec();
        bytes.push(byte);
        bytes.push(0);
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0);
        let (frame, _): (_, String) = codec.decode(&bytes).unwrap();
        assert_eq!(frame.message_type, expected);
    }

    let mut bytes = b"RFB1".to_vec();
    bytes.push(42);
    bytes.push(0);
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(0);
    let error = codec.decode::<String>(&bytes).unwrap_err();
    assert!(matches!(error, CodecError::MessageType(42)));
}

#[test]
fn decode_rejects_noncanonical_magic_and_invalid_flags() {
    let codec = FrameCodec::default();
    let mut bytes = codec
        .encode(MessageType::Event, 1, &"x".to_string())
        .unwrap();
    bytes[..4].copy_from_slice(b"RFB2");
    assert!(matches!(
        codec.decode::<String>(&bytes),
        Err(CodecError::Magic)
    ));

    let mut bytes = codec
        .encode(MessageType::Event, 1, &"x".to_string())
        .unwrap();
    bytes[5] = 2; // flags bit beyond the compression bit
    assert!(matches!(
        codec.decode::<String>(&bytes),
        Err(CodecError::InvalidFlags)
    ));
}

#[test]
fn decompression_bomb_is_bounded_by_max_payload() {
    // A highly-compressible payload encodes into a tiny frame. Decoding it
    // with a small max_payload must fail closed instead of inflating.
    let fat_codec = FrameCodec {
        compression_threshold: 1,
        max_payload: 16 * 1024 * 1024,
    };
    let bytes = fat_codec
        .encode(MessageType::Event, 1, &"x".repeat(4096))
        .unwrap();
    let small_codec = FrameCodec {
        compression_threshold: 1,
        max_payload: 1024,
    };
    let error = small_codec.decode::<String>(&bytes).unwrap_err();
    assert!(matches!(error, CodecError::DecompressedTooLarge));
}

#[test]
fn decode_rejects_undecompressible_or_garbage_payloads() {
    let codec = FrameCodec::default();
    // A good header with a payload that cannot postcard-decode as a String.
    let mut bytes = b"RFB1".to_vec();
    bytes.push(MessageType::Event as u8);
    bytes.push(0);
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(0xff); // varint with a missing continuation byte
    let error = codec.decode::<String>(&bytes).unwrap_err();
    assert!(matches!(error, CodecError::Serialize(_)));

    // A compressed flag with a payload that is not valid zstd.
    let mut bytes = b"RFB1".to_vec();
    bytes.push(MessageType::Event as u8);
    bytes.push(1);
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    let error = codec.decode::<String>(&bytes).unwrap_err();
    assert!(matches!(error, CodecError::Compression(_)));
}

#[test]
fn encode_rejects_payloads_over_max_payload_before_and_after_compression() {
    let codec = FrameCodec {
        compression_threshold: 1,
        max_payload: 32,
    };
    let error = codec
        .encode(MessageType::Event, 1, &"x".repeat(4096))
        .unwrap_err();
    assert!(matches!(error, CodecError::TooLarge));

    // The limit applies to the postcard-encoded size, so a string that stays
    // under 1024 chars but over 1024 encoded bytes is rejected before
    // compression is ever considered.
    let codec = FrameCodec {
        compression_threshold: 1024,
        max_payload: 1024,
    };
    let error = codec
        .encode(MessageType::Event, 1, &"x".repeat(1030))
        .unwrap_err();
    assert!(matches!(error, CodecError::TooLarge));
}

#[test]
fn from_limits_caps_payload_at_frame_limit_minus_header() {
    let limits = RuntimeLimits {
        max_frame_bytes: 4096,
        ..Default::default()
    };
    let codec = FrameCodec::from_limits(&limits);
    assert_eq!(codec.max_payload, 4096 - HEADER_LEN);
    assert_eq!(codec.compression_threshold, 1024);
}

#[test]
fn blocking_framed_io_round_trips_and_rejects_oversized_length() {
    let codec = FrameCodec::default();
    let mut wire = Vec::new();
    write_frame_blocking(
        &mut wire,
        &codec,
        MessageType::Hello,
        11,
        &"ping".to_string(),
    )
    .unwrap();
    let mut reader = wire.as_slice();
    let (frame, value): (_, String) = read_frame_blocking(&mut reader, &codec).unwrap();
    assert_eq!(frame.message_type, MessageType::Hello);
    assert_eq!(frame.sequence, 11);
    assert_eq!(value, "ping");

    // A corrupt length prefix beyond the cap is rejected without buffering up
    // the declared payload.
    let mut wire = (u32::MAX).to_le_bytes().to_vec();
    wire.extend_from_slice(&[0u8; 8]);
    let mut reader = wire.as_slice();
    let error = read_frame_blocking::<_, String>(&mut reader, &codec).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("too large"));
}

#[tokio::test]
async fn async_framed_io_round_trips_and_rejects_oversized_length() {
    let codec = FrameCodec::default();
    let (mut a, mut b) = tokio::io::duplex(4096);
    write_frame(&mut b, &codec, MessageType::Hello, 22, &"async".to_string())
        .await
        .unwrap();
    let (frame, value): (_, String) = read_frame(&mut a, &codec).await.unwrap();
    assert_eq!(frame.message_type, MessageType::Hello);
    assert_eq!(frame.sequence, 22);
    assert_eq!(value, "async");

    let (mut a, mut b) = tokio::io::duplex(64);
    tokio::io::AsyncWriteExt::write_all(&mut a, &u32::MAX.to_le_bytes())
        .await
        .unwrap();
    drop(a);
    let error = read_frame::<_, String>(&mut b, &codec).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("too large"));
}

#[tokio::test]
async fn async_framed_io_round_trips_compressed_payloads() {
    let codec = FrameCodec {
        compression_threshold: 1024,
        max_payload: 16 * 1024 * 1024,
    };
    let payload = "z".repeat(8192);
    let (mut a, mut b) = tokio::io::duplex(64 * 1024);
    write_frame(&mut b, &codec, MessageType::Event, 3, &payload)
        .await
        .unwrap();
    drop(b);
    let (frame, decoded): (_, String) = read_frame(&mut a, &codec).await.unwrap();
    assert!(frame.compressed);
    assert_eq!(decoded, payload);
}

#[test]
fn framed_buffer_round_trips_a_sequence_of_frames() {
    let codec = FrameCodec::default();
    let mut wire = Vec::new();
    for (i, msg) in ["first", "second", "third"].iter().enumerate() {
        write_frame_blocking(
            &mut wire,
            &codec,
            MessageType::Event,
            i as u64,
            &(*msg).to_string(),
        )
        .unwrap();
    }
    let mut reader = wire.as_slice();
    for (i, msg) in ["first", "second", "third"].iter().enumerate() {
        let (frame, value): (_, String) = read_frame_blocking(&mut reader, &codec).unwrap();
        assert_eq!(frame.sequence, i as u64);
        assert_eq!(value, *msg);
    }
}
