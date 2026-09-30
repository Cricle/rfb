//! Minimal fake runtime used in tests to prove the wire round trip without a
//! real guest.

use crate::session::{ControlMessage, RuntimeMessage, SessionEvent};

pub use crate::PROTOCOL_VERSION;

/// Handle a single control message against the fake guest state machine.
pub fn handle_control(message: ControlMessage, sequence: &mut u64) -> Vec<RuntimeMessage> {
    match message {
        ControlMessage::Hello { protocol_version } => {
            if protocol_version != PROTOCOL_VERSION {
                return vec![RuntimeMessage::Error {
                    request_id: String::new(),
                    message: format!("unsupported protocol version {protocol_version}"),
                }];
            }
            vec![
                RuntimeMessage::HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                },
                RuntimeMessage::Capabilities {
                    session_per_vm: true,
                    writable_workspace: true,
                },
            ]
        }
        ControlMessage::StartTurn(request) => {
            *sequence += 1;
            vec![RuntimeMessage::Event(SessionEvent {
                session_id: request.session_id,
                sequence: *sequence,
                kind: "turn.started".to_string(),
                payload: request.prompt.into_bytes(),
            })]
        }
        ControlMessage::Cancel { session_id, .. } => {
            *sequence += 1;
            vec![RuntimeMessage::Event(SessionEvent {
                session_id,
                sequence: *sequence,
                kind: "turn.cancelled".to_string(),
                payload: Vec::new(),
            })]
        }
        ControlMessage::ReadWorkspaceFile(request) => {
            // Fake runtime returns a canned response to prove the round trip.
            vec![RuntimeMessage::FileContent {
                request_id: request.request_id,
                path: request.path,
                content: b"fake".to_vec(),
            }]
        }
        ControlMessage::ReadHostFile(request) => vec![RuntimeMessage::Error {
            request_id: request.request_id,
            message: "read-only host source RPC is not supported".to_string(),
        }],
        ControlMessage::WriteWorkspaceFile(request) => vec![RuntimeMessage::WriteAck {
            request_id: request.request_id,
            path: request.path,
        }],
        ControlMessage::Shutdown => vec![RuntimeMessage::ShutdownAck],
        ControlMessage::Capabilities { .. } => vec![RuntimeMessage::Error {
            request_id: String::new(),
            message: "capabilities is runtime-owned".to_string(),
        }],
    }
}
