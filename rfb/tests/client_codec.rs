#![cfg(feature = "zeroboot")]
//! Golden wire vectors loaded from the shared conformance file
//! `sdk/shared/conformance/zbrt_vectors.json` (schema and loader conventions:
//! `sdk/shared/conformance/README.md`): encode AND decode for all eight
//! frames, plus strict-decode rejection cases driven by the file's `rejects`
//! entries. The codecs are the crate's `rfb::protocol` re-exports, exercised
//! exactly as the SDK uses them.
//!
//! Do NOT hardcode vector bytes here — edit the JSON (and `sdk/PROTOCOL.md`
//! §4) instead.

use std::path::PathBuf;

use rfb::protocol::{
    Cancel, Error as ZbrtError, Execute, Exit, Frame, Hello, HelloAck, Kind, Output, MAX_PAYLOAD,
};

const VECTORS_PATH: &str = "../sdk/shared/conformance/zbrt_vectors.json";

struct GoldenFrame {
    name: String,
    kind: Kind,
    hex: String,
}

struct GoldenReject {
    name: String,
    byte_offset: usize,
    byte_value: u8,
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(VECTORS_PATH)
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "hex string has even length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn load_vectors() -> ([u8; 16], Vec<GoldenFrame>, Vec<GoldenReject>) {
    let path = vectors_path();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read golden vectors at {}: {e}", path.display()));
    let doc: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("valid vectors JSON: {e}"));
    let request_id: [u8; 16] = hex_to_bytes(
        doc.get("request_id_hex")
            .and_then(serde_json::Value::as_str)
            .expect("request_id_hex"),
    )
    .try_into()
    .expect("request_id_hex is 16 bytes");
    let frames = doc
        .get("frames")
        .and_then(serde_json::Value::as_array)
        .expect("frames array")
        .iter()
        .map(|f| {
            let kind = u8::try_from(
                f.get("kind")
                    .and_then(serde_json::Value::as_u64)
                    .expect("frame kind"),
            )
            .expect("kind fits in a byte");
            GoldenFrame {
                name: f
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("frame name")
                    .to_owned(),
                kind: Kind::parse(kind).expect("known frame kind"),
                hex: f
                    .get("hex")
                    .and_then(serde_json::Value::as_str)
                    .expect("frame hex")
                    .to_owned(),
            }
        })
        .collect();
    let rejects = doc
        .get("rejects")
        .and_then(serde_json::Value::as_array)
        .expect("rejects array")
        .iter()
        .map(|r| GoldenReject {
            name: r
                .get("name")
                .and_then(serde_json::Value::as_str)
                .expect("reject name")
                .to_owned(),
            byte_offset: usize::try_from(
                r.get("byte_offset")
                    .and_then(serde_json::Value::as_u64)
                    .expect("byte_offset"),
            )
            .expect("offset fits in usize"),
            byte_value: u8::try_from(
                r.get("byte_value")
                    .and_then(serde_json::Value::as_u64)
                    .expect("byte_value"),
            )
            .expect("byte fits in u8"),
        })
        .collect();
    (request_id, frames, rejects)
}

fn golden<'a>(frames: &'a [GoldenFrame], name: &str) -> &'a GoldenFrame {
    frames
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("golden vector `{name}` missing from the conformance file"))
}

fn frame_bytes(request_id: [u8; 16], kind: Kind, payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = Vec::new();
    Frame {
        kind,
        flags: 0,
        request_id,
        payload,
    }
    .encode(&mut bytes)
    .expect("frame encode");
    bytes
}

fn assert_encode(request_id: [u8; 16], vector: &GoldenFrame, kind: Kind, payload: Vec<u8>) {
    let encoded = frame_bytes(request_id, kind, payload);
    assert_eq!(
        bytes_to_hex(&encoded),
        vector.hex,
        "encode mismatch for golden vector `{}`",
        vector.name
    );
}

fn decode_frame(request_id: [u8; 16], vector: &GoldenFrame) -> Frame {
    let decoded = Frame::decode(&mut hex_to_bytes(&vector.hex).as_slice())
        .unwrap_or_else(|e| panic!("decode `{}` failed: {e}", vector.name));
    assert_eq!(
        decoded.kind, vector.kind,
        "kind mismatch for `{}`",
        vector.name
    );
    assert_eq!(decoded.flags, 0, "flags mismatch for `{}`", vector.name);
    assert_eq!(
        decoded.request_id, request_id,
        "request id mismatch for `{}`",
        vector.name
    );
    decoded
}

fn sdk_test_hello() -> Hello {
    Hello {
        client: "sdk-test".to_owned(),
        capabilities: vec!["execute".to_owned(), "stream".to_owned()],
    }
}

#[test]
fn golden_file_lists_all_eight_vectors() {
    let (_, frames, rejects) = load_vectors();
    let names: Vec<&str> = frames.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "hello",
            "helloack",
            "execute",
            "output",
            "exit",
            "cancel",
            "cancel_legacy",
            "error"
        ],
        "PROTOCOL.md §4 vector set drifted from the conformance file"
    );
    assert!(!rejects.is_empty(), "conformance file carries rejects");
}

#[test]
fn golden_hello_encode_decode() {
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "hello");
    let payload = sdk_test_hello().encode().unwrap();
    assert_encode(rid, vector, Kind::Hello, payload);
    let decoded = decode_frame(rid, vector);
    let hello = Hello::decode(&decoded.payload).unwrap();
    assert_eq!(hello.client, "sdk-test");
    assert_eq!(hello.capabilities, vec!["execute", "stream"]);
}

#[test]
fn golden_helloack_encode_decode() {
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "helloack");
    let payload = HelloAck {
        server: "rfb-zeroboot-guest".to_owned(),
        capabilities: rfb::protocol::ZBRT_V1_CAPABILITIES
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    }
    .encode()
    .unwrap();
    assert_encode(rid, vector, Kind::HelloAck, payload);
    let decoded = decode_frame(rid, vector);
    let ack = HelloAck::decode(&decoded.payload).unwrap();
    assert_eq!(ack.server, "rfb-zeroboot-guest");
    assert_eq!(ack.capabilities.len(), 6);
    assert_eq!(ack.capabilities[0], "execute");
    assert_eq!(ack.capabilities[5], "filesystem");
}

#[test]
fn golden_execute_encode_decode() {
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "execute");
    let payload = Execute {
        argv: vec!["echo".to_owned(), "hi".to_owned()],
        cwd: Some("/workspace".to_owned()),
        stdin: b"abc".to_vec(),
        timeout_ms: 1500,
    }
    .encode()
    .unwrap();
    assert_encode(rid, vector, Kind::Execute, payload);
    let decoded = decode_frame(rid, vector);
    let exec = Execute::decode(&decoded.payload).unwrap();
    assert_eq!(exec.argv, vec!["echo", "hi"]);
    assert_eq!(exec.cwd.as_deref(), Some("/workspace"));
    assert_eq!(exec.stdin, b"abc");
    assert_eq!(exec.timeout_ms, 1500);
}

#[test]
fn golden_output_encode_decode() {
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "output");
    let payload = Output {
        stream: 1,
        data: b"err line\n".to_vec(),
    }
    .encode()
    .unwrap();
    assert_encode(rid, vector, Kind::Output, payload);
    let decoded = decode_frame(rid, vector);
    let output = Output::decode(&decoded.payload).unwrap();
    assert_eq!(output.stream, 1);
    assert_eq!(output.data, b"err line\n");
}

#[test]
fn golden_exit_encode_decode() {
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "exit");
    let payload = Exit {
        code: 0,
        signal: None,
    }
    .encode()
    .unwrap();
    assert_encode(rid, vector, Kind::Exit, payload);
    let decoded = decode_frame(rid, vector);
    let exit = Exit::decode(&decoded.payload).unwrap();
    assert_eq!(exit.code, 0);
    assert_eq!(exit.signal, None);
}

#[test]
fn golden_cancel_encode_decode() {
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "cancel");
    // The modern form carries the explicit no-target flag byte.
    let payload = Cancel {
        reason: Some("user".to_owned()),
        target: None,
    }
    .encode()
    .unwrap();
    assert_encode(rid, vector, Kind::Cancel, payload);
    let decoded = decode_frame(rid, vector);
    let cancel = Cancel::decode(&decoded.payload).unwrap();
    assert_eq!(cancel.reason.as_deref(), Some("user"));
    assert_eq!(cancel.target, None);
}

#[test]
fn golden_cancel_legacy_decode() {
    // Legacy payloads end right after the reason; target decodes to None.
    // (Decode-only: no encodable structure exists for the legacy form.)
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "cancel_legacy");
    let decoded = decode_frame(rid, vector);
    let cancel = Cancel::decode(&decoded.payload).unwrap();
    assert_eq!(cancel.reason.as_deref(), Some("user"));
    assert_eq!(cancel.target, None);
}

#[test]
fn golden_error_encode_decode() {
    let (rid, frames, _) = load_vectors();
    let vector = golden(&frames, "error");
    let payload = ZbrtError {
        code: 1,
        message: "argv is empty".to_owned(),
    }
    .encode()
    .unwrap();
    assert_encode(rid, vector, Kind::Error, payload);
    let decoded = decode_frame(rid, vector);
    let err = ZbrtError::decode(&decoded.payload).unwrap();
    assert_eq!(err.code, 1);
    assert_eq!(err.message, "argv is empty");
}

/// Every `rejects` entry flips one byte of the hello vector; strict decode
/// must refuse the frame (byte_offset/byte_value from the conformance file).
#[test]
fn rejects_from_conformance_file() {
    let (rid, _frames, rejects) = load_vectors();
    let good = frame_bytes(rid, Kind::Hello, sdk_test_hello().encode().unwrap());
    for reject in &rejects {
        let mut bytes = good.clone();
        assert!(
            reject.byte_offset < bytes.len(),
            "reject `{}` offset out of range",
            reject.name
        );
        bytes[reject.byte_offset] = reject.byte_value;
        assert!(
            Frame::decode(&mut bytes.as_slice()).is_err(),
            "reject `{}` must fail strict decode",
            reject.name
        );
    }
}

// Behavioral strict-decode rejections below are built programmatically (no
// hardcoded vector bytes): header-level rules that a single flipped byte
// cannot express (flags, truncation, trailing payload, oversize).

fn good_frame_bytes(rid: [u8; 16]) -> Vec<u8> {
    frame_bytes(rid, Kind::Hello, sdk_test_hello().encode().unwrap())
}

#[test]
fn rejects_nonzero_flags() {
    let (rid, _, _) = load_vectors();
    let mut bytes = good_frame_bytes(rid);
    bytes[6] = 0;
    bytes[7] = 1;
    assert!(Frame::decode(&mut bytes.as_slice()).is_err());
}

#[test]
fn rejects_truncated_header() {
    let (rid, _, _) = load_vectors();
    let bytes = good_frame_bytes(rid);
    assert!(Frame::decode(&mut &bytes[..20]).is_err());
}

#[test]
fn rejects_truncated_payload() {
    let (rid, _, _) = load_vectors();
    let bytes = good_frame_bytes(rid);
    assert!(Frame::decode(&mut &bytes[..40]).is_err());
}

#[test]
fn rejects_trailing_payload_bytes() {
    // One extra byte inside the declared payload: the strict Hello codec
    // must reject the trailing byte.
    let (rid, _, _) = load_vectors();
    let mut bytes = good_frame_bytes(rid);
    let mut payload = bytes.split_off(28);
    payload.push(0xff);
    assert!(Hello::decode(&payload).is_err());
}

#[test]
fn rejects_oversize_payload() {
    let (rid, _, _) = load_vectors();
    let mut bytes = vec![b'Z', b'B', b'R', b'T', 1, 1, 0, 0];
    bytes.extend_from_slice(&rid);
    let len = (MAX_PAYLOAD + 1) as u32;
    bytes.extend_from_slice(&len.to_be_bytes());
    assert!(Frame::decode(&mut bytes.as_slice()).is_err());
}
