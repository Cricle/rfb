//! Guest-side execution seam and the events it produces.

use crate::session::SessionRequest;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Guest-side execution seam. Implementations own the guest command/event pump;
/// the runtime service owns identity, sequencing, lifecycle, and protocol safety.
pub trait GuestExecutor: Send {
    /// Start a turn and return its events (or an error).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    fn start_turn(&mut self, request: &SessionRequest) -> Result<Vec<GuestEvent>, String>;
    /// Cancel an in-flight turn for the given identity.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    fn cancel(&mut self, session_id: &str, request_id: &str) -> Result<(), String>;
    /// Shut down this executor, releasing any child processes.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    fn shutdown(&mut self) -> Result<(), String>;

    /// Read a structured workspace file. Unsupported by default.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    fn read_workspace_file(
        &mut self,
        _request: &crate::session::FileReadRequest,
    ) -> Result<Vec<u8>, String> {
        Err("structured file RPC is not supported by this executor".into())
    }
    /// Write a structured workspace file. Unsupported by default.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    fn write_workspace_file(
        &mut self,
        _request: &crate::session::FileWriteRequest,
    ) -> Result<(), String> {
        Err("structured file RPC is not supported by this executor".into())
    }

    /// Handle a typed filesystem RPC and return its JSON result.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    fn filesystem_rpc(&mut self, _op: u8, _path: &str, _data: &[u8]) -> Result<Vec<u8>, String> {
        Err("filesystem RPC is not supported by this executor".into())
    }

    /// Shared cancellation signal for the currently running turn. When `Some`,
    /// a host `Cancel` can set this flag from a concurrent reader while a
    /// blocking `start_turn` is executing, and the executor terminates the
    /// child/process group on its next poll. Defaults to `None` (fail-closed
    /// cancel). Implementors that support true in-flight cancellation override
    /// this to return their handle.
    fn cancel_handle(&self) -> Option<Arc<AtomicBool>> {
        None
    }

    /// Reset the shared cancellation flag for a fresh turn. Called by the
    /// service before handing a turn to a worker, i.e. strictly before a later
    /// Cancel frame can be processed, so an early Cancel is never swallowed by
    /// a stale flag from a previous turn.
    fn reset_cancel(&mut self) {}

    /// Attach a live event sink for streaming transports. Implementors that
    /// forward `terminal.output` chunks while a turn runs (the workspace
    /// executor under the ZeroBoot V1 connection) override this; the default
    /// keeps the buffered event contract used by RFB1.
    fn attach_event_sink(&mut self, _sink: Option<Arc<dyn Fn(GuestEvent) + Send + Sync>>) {}
}

/// An event produced by a guest executor. Sequence numbers are assigned by the
/// service, never accepted from an executor or a host request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestEvent {
    /// Stable event kind label (e.g. `turn.started`, `terminal.output`).
    pub kind: String,
    /// Event payload (typed terminal schema for `terminal.output`).
    pub payload: Vec<u8>,
}

impl GuestEvent {
    /// Construct an event from a kind label and a payload.
    pub fn new(kind: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: kind.into(),
            payload: payload.into(),
        }
    }
}

/// An executor that rejects every turn. Used when the environment does not
/// declare a real guest backend, so the runtime fails closed instead of
/// fabricating results.
pub(super) struct UnavailableExecutor(pub(super) String);
impl GuestExecutor for UnavailableExecutor {
    fn start_turn(&mut self, _request: &SessionRequest) -> Result<Vec<GuestEvent>, String> {
        Err(self.0.clone())
    }
    fn cancel(&mut self, _session_id: &str, _request_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), String> {
        Ok(())
    }
}
