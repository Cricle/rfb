#![cfg(feature = "core")]

// ZBRT V1 wire contract tests, moved out of `src/zeroboot_protocol.rs` to keep
// the crate free of `#[cfg(test)]` modules (rfb/scripts/check-tests-folder.sh).

use rfb_runtime::zeroboot_protocol::*;

fn frame(kind: Kind, request_id: [u8; 16], payload: Vec<u8>) -> Frame {
    Frame {
        kind,
        flags: 0,
        request_id,
        payload,
    }
}

#[test]
fn frame_round_trips_header_and_payload() {
    let expected = frame(
        Kind::Execute,
        [7; 16],
        Execute {
            argv: vec!["echo".into(), "hi".into()],
            cwd: Some(".".into()),
            stdin: b"in".to_vec(),
            timeout_ms: 42,
        }
        .encode()
        .unwrap(),
    );
    let mut wire = Vec::new();
    expected.encode(&mut wire).unwrap();
    let decoded = Frame::decode(&mut wire.as_slice()).unwrap();
    assert_eq!(decoded, expected);
}

#[test]
fn frame_rejects_flags_and_oversized_payloads() {
    let mut wire = Vec::new();
    frame(Kind::Hello, [0; 16], Vec::new())
        .encode(&mut wire)
        .unwrap();
    wire[6] = 1; // set flags byte 0
    assert!(Frame::decode(&mut wire.as_slice()).is_err());

    // oversized payload fails at encode time
    let big = frame(Kind::Hello, [0; 16], vec![0u8; MAX_PAYLOAD + 1]);
    let mut out = Vec::new();
    assert!(big.encode(&mut out).is_err());
}

#[test]
fn codecs_reject_truncation_and_trailing_bytes() {
    let hello = Hello {
        client: "t".into(),
        capabilities: vec!["execute".into()],
    };
    let encoded = hello.encode().unwrap();
    assert!(Hello::decode(&encoded[..encoded.len() - 1]).is_err());
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(Hello::decode(&trailing).is_err());
    assert!(Hello::decode(&encoded).is_ok());
}

#[test]
fn execute_codec_round_trips_all_fields() {
    let expected = Execute {
        argv: vec!["sh".into(), "-c".into(), "echo $PWD".into()],
        cwd: Some("/workspace/sub".into()),
        stdin: b"\x00\x01\xff".to_vec(),
        timeout_ms: u32::MAX,
    };
    let decoded = Execute::decode(&expected.encode().unwrap()).unwrap();
    assert_eq!(decoded, expected);
}

#[test]
fn health_and_error_codecs_round_trip() {
    let health = Health {
        healthy: true,
        message: Some("ready".into()),
    };
    assert_eq!(Health::decode(&health.encode().unwrap()).unwrap(), health);
    let error = Error {
        code: 95,
        message: "not implemented".into(),
    };
    assert_eq!(Error::decode(&error.encode().unwrap()).unwrap(), error);
    let exit = Exit {
        code: -1,
        signal: Some(9),
    };
    assert_eq!(Exit::decode(&exit.encode().unwrap()).unwrap(), exit);
}

#[test]
fn cancel_codec_round_trips_target_and_accepts_legacy_payloads() {
    // Explicit target form used by stream.stop.
    let targeted = Cancel {
        reason: Some("user stopped the stream".into()),
        target: Some([9; 16]),
    };
    assert_eq!(
        Cancel::decode(&targeted.encode().unwrap()).unwrap(),
        targeted
    );

    // Null target is still encoded with an explicit trailing flag.
    let untargeted = Cancel {
        reason: None,
        target: None,
    };
    assert_eq!(
        Cancel::decode(&untargeted.encode().unwrap()).unwrap(),
        untargeted
    );

    // Legacy V1 payload (reason only, no target byte) must decode with
    // target=None instead of failing: old hosts cancel the active request.
    // Wire form: reason flag=1, u32 len=2, "r1" (no trailing target flag).
    let legacy = vec![1u8, 0, 0, 0, 2, b'r', b'1'];
    let decoded = Cancel::decode(&legacy).unwrap();
    assert_eq!(decoded.reason.as_deref(), Some("r1"));
    assert_eq!(decoded.target, None);
}

#[test]
fn host_session_negotiates_and_submits() {
    let mut session = HostSession::new();
    let ack = session
        .negotiate_capabilities(
            Hello {
                client: "test".into(),
                capabilities: vec!["execute".into(), "filesystem".into(), "v2".into()],
            },
            &["execute"],
        )
        .unwrap();
    assert_eq!(ack.capabilities, vec!["execute"]);
    assert!(session
        .submit(
            [1; 16],
            Execute {
                argv: vec!["true".into()],
                cwd: None,
                stdin: Vec::new(),
                timeout_ms: 0,
            }
        )
        .is_ok());
    assert!(session
        .submit(
            [1; 16],
            Execute {
                argv: vec!["true".into()],
                cwd: None,
                stdin: Vec::new(),
                timeout_ms: 0,
            }
        )
        .is_err());
    assert!(session.cancel(&[1; 16]));
    assert!(!session.cancel(&[1; 16]));
}
