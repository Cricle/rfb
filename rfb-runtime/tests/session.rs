//! RFB1 session/control wire contract tests: `RuntimeEvent` classification and
//! postcard round trips for every session, control, and runtime type.

use rfb_runtime::session::{
    ControlMessage, FileReadRequest, FileWriteRequest, RuntimeEvent, RuntimeMessage, SessionEvent,
    SessionRequest, TerminalEvent, TerminalStream,
};

fn round_trips<T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq>(
    value: &T,
) -> T {
    let bytes = postcard::to_allocvec(value).unwrap();
    postcard::from_bytes(&bytes).unwrap()
}

#[test]
fn runtime_event_classification_is_stable() {
    let events: [(RuntimeEvent, &'static str, u64, bool); 7] = [
        (
            RuntimeEvent::Started { sequence: 1 },
            "turn.started",
            1,
            false,
        ),
        (
            RuntimeEvent::Progress {
                sequence: 2,
                message: "working".into(),
            },
            "turn.progress",
            2,
            false,
        ),
        (
            RuntimeEvent::Output {
                sequence: 3,
                terminal: TerminalEvent {
                    stream: TerminalStream::Stdout,
                    data: "hello".into(),
                },
            },
            "terminal.output",
            3,
            false,
        ),
        (
            RuntimeEvent::Completed {
                sequence: 4,
                exit_code: Some(0),
            },
            "turn.completed",
            4,
            true,
        ),
        (
            RuntimeEvent::Completed {
                sequence: 5,
                exit_code: None,
            },
            "turn.completed",
            5,
            true,
        ),
        (
            RuntimeEvent::Failed {
                sequence: 6,
                reason: "boom".into(),
            },
            "turn.failed",
            6,
            true,
        ),
        (
            RuntimeEvent::Cancelled { sequence: 7 },
            "turn.cancelled",
            7,
            true,
        ),
    ];
    for (event, kind, sequence, terminal) in events {
        assert_eq!(event.kind_label(), kind);
        assert_eq!(event.sequence(), sequence);
        assert_eq!(event.is_terminal(), terminal);
    }
}

#[test]
fn runtime_event_variants_round_trip_through_postcard() {
    let events = [
        RuntimeEvent::Started { sequence: 1 },
        RuntimeEvent::Progress {
            sequence: 2,
            message: "working".into(),
        },
        RuntimeEvent::Output {
            sequence: 3,
            terminal: TerminalEvent {
                stream: TerminalStream::Stderr,
                data: "oops".into(),
            },
        },
        RuntimeEvent::Completed {
            sequence: 4,
            exit_code: Some(7),
        },
        RuntimeEvent::Completed {
            sequence: 5,
            exit_code: None,
        },
        RuntimeEvent::Failed {
            sequence: 6,
            reason: "bad".into(),
        },
        RuntimeEvent::Cancelled { sequence: 7 },
    ];
    for event in events {
        let decoded = round_trips(&event);
        assert_eq!(decoded, event);
        assert_eq!(decoded.kind_label(), event.kind_label());
    }
}

#[test]
fn terminal_schema_distinguishes_streams_and_keeps_text_utf8() {
    let stdout = TerminalEvent {
        stream: TerminalStream::Stdout,
        data: "out".into(),
    };
    let stderr = TerminalEvent {
        stream: TerminalStream::Stderr,
        data: "err".into(),
    };
    assert_eq!(stdout.stream, TerminalStream::Stdout);
    assert_ne!(stdout, stderr);
    assert!(std::str::from_utf8(&serde_json::to_vec(&stdout).unwrap()).is_ok());
    assert_eq!(round_trips(&stdout), stdout);
    assert_eq!(round_trips(&stderr), stderr);
}

#[test]
fn session_event_preserves_identity_sequence_kind_and_payload() {
    let event = SessionEvent {
        session_id: "session-1".into(),
        sequence: 42,
        kind: "terminal.output".into(),
        payload: vec![1, 2, 3, 4],
    };
    let decoded = round_trips(&event);
    assert_eq!(decoded, event);
    assert_eq!(decoded.sequence, 42);
    assert_eq!(decoded.payload, vec![1, 2, 3, 4]);
}

#[test]
fn session_request_round_trips() {
    for request in [
        SessionRequest {
            session_id: "s".into(),
            request_id: "r".into(),
            prompt: "structured json".into(),
        },
        SessionRequest {
            session_id: "empty-prompt".into(),
            request_id: "r2".into(),
            prompt: String::new(),
        },
    ] {
        assert_eq!(round_trips(&request), request);
    }
}

#[test]
fn file_request_types_round_trip() {
    let read = FileReadRequest {
        request_id: "read-1".into(),
        path: "src/main.rs".into(),
        max_bytes: 4096,
    };
    assert_eq!(round_trips(&read), read);

    let write = FileWriteRequest {
        request_id: "write-1".into(),
        path: "out/build.bin".into(),
        content: vec![0u8, 1, 2, 255],
    };
    assert_eq!(round_trips(&write), write);
}

#[test]
fn control_messages_round_trip_through_postcard() {
    let messages = [
        ControlMessage::Hello {
            protocol_version: 1,
        },
        ControlMessage::Hello {
            protocol_version: 1_000,
        },
        ControlMessage::Capabilities {
            session_per_vm: true,
            writable_workspace: false,
        },
        ControlMessage::StartTurn(SessionRequest {
            session_id: "s".into(),
            request_id: "r".into(),
            prompt: "do-it".into(),
        }),
        ControlMessage::Cancel {
            session_id: "s".into(),
            request_id: "r".into(),
        },
        ControlMessage::ReadWorkspaceFile(FileReadRequest {
            request_id: "f".into(),
            path: "a.txt".into(),
            max_bytes: 16,
        }),
        ControlMessage::ReadHostFile(FileReadRequest {
            request_id: "g".into(),
            path: "b.txt".into(),
            max_bytes: 8,
        }),
        ControlMessage::WriteWorkspaceFile(FileWriteRequest {
            request_id: "w".into(),
            path: "c.txt".into(),
            content: b"data".to_vec(),
        }),
        ControlMessage::Shutdown,
    ];
    for message in messages {
        assert_eq!(round_trips(&message), message);
    }
}

#[test]
fn runtime_messages_round_trip_through_postcard() {
    let messages = [
        RuntimeMessage::HelloAck {
            protocol_version: 1,
        },
        RuntimeMessage::Capabilities {
            session_per_vm: false,
            writable_workspace: true,
        },
        RuntimeMessage::Event(SessionEvent {
            session_id: "s".into(),
            sequence: 1,
            kind: "turn.started".into(),
            payload: vec![0],
        }),
        RuntimeMessage::Error {
            request_id: "r".into(),
            message: "failed".into(),
        },
        RuntimeMessage::FileContent {
            request_id: "f".into(),
            path: "a.txt".into(),
            content: b"bytes".to_vec(),
        },
        RuntimeMessage::WriteAck {
            request_id: "w".into(),
            path: "c.txt".into(),
        },
        RuntimeMessage::ShutdownAck,
    ];
    for message in messages {
        assert_eq!(round_trips(&message), message);
    }
}

#[test]
fn event_and_error_are_distinct_wire_variants() {
    // A terminal Error must not be confused with a terminal Event on the wire.
    let error = RuntimeMessage::Error {
        request_id: "r".into(),
        message: "boom".into(),
    };
    let event = RuntimeMessage::Event(SessionEvent {
        session_id: "s".into(),
        sequence: 1,
        kind: "turn.failed".into(),
        payload: vec![],
    });
    assert_ne!(error, event);
    assert!(round_trips::<RuntimeMessage>(&error) != round_trips::<RuntimeMessage>(&event));
}
