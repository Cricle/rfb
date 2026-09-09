#![cfg(feature = "zeroboot")]
//! Golden wire vectors from `sdk/PROTOCOL.md` §4 (encode AND decode for all
//! eight) plus strict-decode rejection cases. The codecs are the crate's
//! `rfb::protocol` re-exports, exercised exactly as the SDK uses them.

use rfb::protocol::{
    Cancel, Error as ZbrtError, Execute, Exit, Frame, Hello, HelloAck, Kind, Output, MAX_PAYLOAD,
};

const RID: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

const HELLO_HEX: &str = "5a42525401010000000102030405060708090a0b0c0d0e0f000000220000000873646b2d746573740200000007657865637574650000000673747265616d";
const HELLOACK_HEX: &str = "5a42525401020000000102030405060708090a0b0c0d0e0f0000005a000000127266622d7a65726f626f6f742d67756573740600000007657865637574650000000673747265616d00000008646561646c696e65000000066865616c74680000000663616e63656c0000000a66696c6573797374656d";
const EXECUTE_HEX: &str = "5a42525401030000000102030405060708090a0b0c0d0e0f0000002902000000046563686f000000026869010000000a2f776f726b737061636500000003616263000005dc";
const OUTPUT_HEX: &str =
    "5a42525401040000000102030405060708090a0b0c0d0e0f0000000e0100000009657272206c696e650a";
const EXIT_HEX: &str = "5a42525401050000000102030405060708090a0b0c0d0e0f000000050000000000";
const CANCEL_HEX: &str =
    "5a42525401060000000102030405060708090a0b0c0d0e0f0000000a01000000047573657200";
const CANCEL_LEGACY_HEX: &str =
    "5a42525401060000000102030405060708090a0b0c0d0e0f00000009010000000475736572";
const ERROR_HEX: &str = "5a425254010c0000000102030405060708090a0b0c0d0e0f00000015000000010000000d6172677620697320656d707479";

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn frame(kind: Kind, payload: Vec<u8>) -> Frame {
    Frame {
        kind,
        flags: 0,
        request_id: RID,
        payload,
    }
}

fn assert_roundtrip(kind: Kind, golden: &str, build_payload: fn() -> Vec<u8>) -> Frame {
    let mut encoded = Vec::new();
    frame(kind, build_payload())
        .encode(&mut encoded)
        .expect("encode");
    assert_eq!(bytes_to_hex(&encoded), golden, "encode must match golden");
    let decoded = Frame::decode(&mut hex_to_bytes(golden).as_slice()).expect("decode");
    assert_eq!(decoded.kind, kind);
    assert_eq!(decoded.flags, 0);
    assert_eq!(decoded.request_id, RID);
    decoded
}

#[test]
fn golden_hello_encode_decode() {
    let decoded = assert_roundtrip(Kind::Hello, HELLO_HEX, || {
        Hello {
            client: "sdk-test".to_owned(),
            capabilities: vec!["execute".to_owned(), "stream".to_owned()],
        }
        .encode()
        .unwrap()
    });
    let hello = Hello::decode(&decoded.payload).unwrap();
    assert_eq!(hello.client, "sdk-test");
    assert_eq!(hello.capabilities, vec!["execute", "stream"]);
}

#[test]
fn golden_helloack_encode_decode() {
    let decoded = assert_roundtrip(Kind::HelloAck, HELLOACK_HEX, || {
        HelloAck {
            server: "rfb-zeroboot-guest".to_owned(),
            capabilities: rfb::protocol::ZBRT_V1_CAPABILITIES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
        .encode()
        .unwrap()
    });
    let ack = HelloAck::decode(&decoded.payload).unwrap();
    assert_eq!(ack.server, "rfb-zeroboot-guest");
    assert_eq!(ack.capabilities.len(), 6);
    assert_eq!(ack.capabilities[0], "execute");
    assert_eq!(ack.capabilities[5], "filesystem");
}

#[test]
fn golden_execute_encode_decode() {
    let decoded = assert_roundtrip(Kind::Execute, EXECUTE_HEX, || {
        Execute {
            argv: vec!["echo".to_owned(), "hi".to_owned()],
            cwd: Some("/workspace".to_owned()),
            stdin: b"abc".to_vec(),
            timeout_ms: 1500,
        }
        .encode()
        .unwrap()
    });
    let exec = Execute::decode(&decoded.payload).unwrap();
    assert_eq!(exec.argv, vec!["echo", "hi"]);
    assert_eq!(exec.cwd.as_deref(), Some("/workspace"));
    assert_eq!(exec.stdin, b"abc");
    assert_eq!(exec.timeout_ms, 1500);
}

#[test]
fn golden_output_encode_decode() {
    let decoded = assert_roundtrip(Kind::Output, OUTPUT_HEX, || {
        Output {
            stream: 1,
            data: b"err line\n".to_vec(),
        }
        .encode()
        .unwrap()
    });
    let output = Output::decode(&decoded.payload).unwrap();
    assert_eq!(output.stream, 1);
    assert_eq!(output.data, b"err line\n");
}

#[test]
fn golden_exit_encode_decode() {
    let decoded = assert_roundtrip(Kind::Exit, EXIT_HEX, || {
        Exit {
            code: 0,
            signal: None,
        }
        .encode()
        .unwrap()
    });
    let exit = Exit::decode(&decoded.payload).unwrap();
    assert_eq!(exit.code, 0);
    assert_eq!(exit.signal, None);
}

#[test]
fn golden_cancel_encode_decode() {
    // The modern form carries the explicit no-target flag byte.
    let decoded = assert_roundtrip(Kind::Cancel, CANCEL_HEX, || {
        Cancel {
            reason: Some("user".to_owned()),
            target: None,
        }
        .encode()
        .unwrap()
    });
    let cancel = Cancel::decode(&decoded.payload).unwrap();
    assert_eq!(cancel.reason.as_deref(), Some("user"));
    assert_eq!(cancel.target, None);
}

#[test]
fn golden_cancel_legacy_decode() {
    // Legacy payloads end right after the reason; target decodes to None.
    let decoded = Frame::decode(&mut hex_to_bytes(CANCEL_LEGACY_HEX).as_slice()).unwrap();
    assert_eq!(decoded.kind, Kind::Cancel);
    let cancel = Cancel::decode(&decoded.payload).unwrap();
    assert_eq!(cancel.reason.as_deref(), Some("user"));
    assert_eq!(cancel.target, None);
}

#[test]
fn golden_error_encode_decode() {
    let decoded = assert_roundtrip(Kind::Error, ERROR_HEX, || {
        ZbrtError {
            code: 1,
            message: "argv is empty".to_owned(),
        }
        .encode()
        .unwrap()
    });
    let err = ZbrtError::decode(&decoded.payload).unwrap();
    assert_eq!(err.code, 1);
    assert_eq!(err.message, "argv is empty");
}

fn good_frame_bytes() -> Vec<u8> {
    hex_to_bytes(HELLO_HEX)
}

#[test]
fn rejects_bad_magic() {
    let mut bytes = good_frame_bytes();
    bytes[0] = b'X';
    assert!(Frame::decode(&mut bytes.as_slice()).is_err());
}

#[test]
fn rejects_wrong_version() {
    let mut bytes = good_frame_bytes();
    bytes[4] = 2;
    assert!(Frame::decode(&mut bytes.as_slice()).is_err());
}

#[test]
fn rejects_nonzero_flags() {
    let mut bytes = good_frame_bytes();
    bytes[6] = 0;
    bytes[7] = 1;
    assert!(Frame::decode(&mut bytes.as_slice()).is_err());
}

#[test]
fn rejects_unknown_kind() {
    let mut bytes = good_frame_bytes();
    bytes[5] = 0x7f;
    assert!(Frame::decode(&mut bytes.as_slice()).is_err());
}

#[test]
fn rejects_truncated_header() {
    let bytes = good_frame_bytes();
    assert!(Frame::decode(&mut &bytes[..20]).is_err());
}

#[test]
fn rejects_truncated_payload() {
    let bytes = good_frame_bytes();
    assert!(Frame::decode(&mut &bytes[..40]).is_err());
}

#[test]
fn rejects_trailing_payload_bytes() {
    // One extra byte inside the declared payload: the strict Hello codec
    // must reject the trailing byte.
    let mut bytes = good_frame_bytes();
    let mut payload = bytes.split_off(28);
    payload.push(0xff);
    assert!(Hello::decode(&payload).is_err());
}

#[test]
fn rejects_oversize_payload() {
    let mut bytes = vec![b'Z', b'B', b'R', b'T', 1, 1, 0, 0];
    bytes.extend_from_slice(&RID);
    let len = (MAX_PAYLOAD + 1) as u32;
    bytes.extend_from_slice(&len.to_be_bytes());
    assert!(Frame::decode(&mut bytes.as_slice()).is_err());
}
