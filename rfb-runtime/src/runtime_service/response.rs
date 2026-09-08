//! Terminal-response detection for the runtime service.

use crate::session::RuntimeMessage;

/// Whether a response ends a turn (completed/failed/cancelled terminal).
pub(super) fn response_completes_turn(response: &RuntimeMessage) -> bool {
    matches!(response, RuntimeMessage::Event(event) if matches!(event.kind.as_str(), "turn.completed" | "turn.failed" | "turn.cancelled"))
}
