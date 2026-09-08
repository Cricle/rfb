//! Identity validation helpers shared by the runtime service.

use crate::session::ControlMessage;
use crate::session::RuntimeMessage;

/// Validate that a session/request identity pair is non-empty.
pub(super) fn validate_identity(session_id: &str, request_id: &str) -> Option<RuntimeMessage> {
    if session_id.trim().is_empty() || request_id.trim().is_empty() {
        Some(RuntimeMessage::Error {
            request_id: request_id.into(),
            message: "session_id and request_id must be non-empty".into(),
        })
    } else {
        None
    }
}

/// The request id of a control message, or empty for messages without one.
pub(super) fn request_id(request: &ControlMessage) -> String {
    match request {
        ControlMessage::StartTurn(turn) => turn.request_id.clone(),
        _ => String::new(),
    }
}
