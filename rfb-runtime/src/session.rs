//! RFB1 session/control wire contracts shared by the runtime and hosts.
//!
//! ```
//! use rfb_runtime::session::{TerminalEvent, TerminalStream, RuntimeEvent};
//! let event = RuntimeEvent::Output { sequence: 2, terminal: TerminalEvent { stream: TerminalStream::Stdout, data: "ok".into() } };
//! assert_eq!(event.kind_label(), "terminal.output");
//! assert_eq!(event.sequence(), 2);
//! assert!(!event.is_terminal());
//! ```

use serde::{Deserialize, Serialize};

/// A turn request: identity plus the opaque prompt the executor interprets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRequest {
    /// Client-chosen session identifier.
    pub session_id: String,
    /// Client-chosen request identifier for this turn.
    pub request_id: String,
    /// Opaque prompt (structured JSON for the workspace executor).
    pub prompt: String,
}

/// An event emitted by a running turn, with the service-assigned sequence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEvent {
    /// Session that produced the event.
    pub session_id: String,
    /// Monotonic sequence assigned by the runtime service.
    pub sequence: u64,
    /// Stable event kind string (e.g. `turn.started`, `terminal.output`).
    pub kind: String,
    /// Event payload (typed terminal schema for `terminal.output`).
    pub payload: Vec<u8>,
}

/// Structured read request for a guest file (host source or workspace).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileReadRequest {
    /// Caller-chosen request identifier echoed in the response.
    pub request_id: String,
    /// Guest-relative or workspace-relative path.
    pub path: String,
    /// Maximum bytes to read; `0` is rejected by the service.
    pub max_bytes: usize,
}

/// Structured write request for a guest workspace file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileWriteRequest {
    /// Caller-chosen request identifier echoed in the response.
    pub request_id: String,
    /// Workspace-relative target path (escapes are rejected by policy).
    pub path: String,
    /// Exact bytes to write.
    pub content: Vec<u8>,
}

/// Terminal output stream selector for the typed terminal schema.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TerminalStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Typed terminal event schema. The `data` field is always UTF-8 text; the
/// runtime never mixes raw bytes into the terminal stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalEvent {
    /// Which stream produced the text.
    pub stream: TerminalStream,
    /// UTF-8 text chunk.
    pub data: String,
}

/// Strong typed Runtime event classification, replacing kind-string convention.
/// `SessionEvent` still carries a stable `kind` string for host compatibility,
/// but this enum is the canonical, wire-serializable interpretation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// The guest acknowledged the turn.
    Started {
        /// Service-assigned sequence.
        sequence: u64,
    },
    /// Non-terminal progress signal.
    Progress {
        /// Service-assigned sequence.
        sequence: u64,
        /// Human-readable progress detail.
        message: String,
    },
    /// A typed terminal output chunk.
    Output {
        /// Service-assigned sequence.
        sequence: u64,
        /// The terminal stream + text payload.
        terminal: TerminalEvent,
    },
    /// The turn finished successfully.
    Completed {
        /// Service-assigned sequence.
        sequence: u64,
        /// Process exit code when the guest reported one.
        exit_code: Option<i32>,
    },
    /// The turn failed.
    Failed {
        /// Service-assigned sequence.
        sequence: u64,
        /// Failure reason.
        reason: String,
    },
    /// The turn was cancelled.
    Cancelled {
        /// Service-assigned sequence.
        sequence: u64,
    },
}

impl RuntimeEvent {
    /// The stable kind label for this event.
    pub fn kind_label(&self) -> &'static str {
        match self {
            RuntimeEvent::Started { .. } => "turn.started",
            RuntimeEvent::Progress { .. } => "turn.progress",
            RuntimeEvent::Output { .. } => "terminal.output",
            RuntimeEvent::Completed { .. } => "turn.completed",
            RuntimeEvent::Failed { .. } => "turn.failed",
            RuntimeEvent::Cancelled { .. } => "turn.cancelled",
        }
    }

    /// The service-assigned sequence for this event.
    pub fn sequence(&self) -> u64 {
        match self {
            RuntimeEvent::Started { sequence }
            | RuntimeEvent::Progress { sequence, .. }
            | RuntimeEvent::Output { sequence, .. }
            | RuntimeEvent::Completed { sequence, .. }
            | RuntimeEvent::Failed { sequence, .. }
            | RuntimeEvent::Cancelled { sequence } => *sequence,
        }
    }

    /// Whether this event ends the turn.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RuntimeEvent::Completed { .. }
                | RuntimeEvent::Failed { .. }
                | RuntimeEvent::Cancelled { .. }
        )
    }
}

/// Control messages sent by a host to the guest runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ControlMessage {
    /// Open the RFB1 handshake with the requested protocol version.
    Hello {
        /// Must match `rfb_runtime::PROTOCOL_VERSION`.
        protocol_version: u16,
    },
    /// Advertise host-side session/workspace capabilities.
    Capabilities {
        /// One runtime serves at most one active session.
        session_per_vm: bool,
        /// The guest workspace is writable.
        writable_workspace: bool,
    },
    /// Start a turn (the executor interprets the opaque prompt).
    StartTurn(SessionRequest),
    /// Cancel an in-flight turn (idempotent).
    Cancel {
        /// Session whose active request is being cancelled.
        session_id: String,
        /// Request being cancelled. A session may process multiple turns over
        /// its lifetime, so session_id alone is not a sufficient identity.
        request_id: String,
    },
    /// Read a file from a configured guest workspace root. Fails closed when
    /// the executor does not support structured file RPC.
    ReadWorkspaceFile(FileReadRequest),
    /// Read a file from a configured read-only host source root, if one exists.
    ReadHostFile(FileReadRequest),
    /// Write a file inside the guest workspace only. Outside the workspace is
    /// always rejected by the policy.
    WriteWorkspaceFile(FileWriteRequest),
    /// Orderly shutdown: the runtime replies `ShutdownAck` and stops servicing.
    Shutdown,
}

/// Runtime responses to control messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RuntimeMessage {
    /// Reply to `Hello` with the agreed protocol version.
    HelloAck {
        /// Agreed protocol version (must equal `rfb_runtime::PROTOCOL_VERSION`).
        protocol_version: u16,
    },
    /// Reply to `Capabilities` (also returned during handshake checks).
    Capabilities {
        /// One runtime serves at most one active session.
        session_per_vm: bool,
        /// The guest workspace is writable.
        writable_workspace: bool,
    },
    /// A session event (sequence assigned by the service).
    Event(SessionEvent),
    /// A terminal error carrying the request identity.
    Error {
        /// Request whose failure this reports.
        request_id: String,
        /// Machine-readable error message.
        message: String,
    },
    /// Structured response to a file read RPC.
    FileContent {
        /// Echoes the originating request id.
        request_id: String,
        /// Path that was read.
        path: String,
        /// Read bytes (bounded by the request's `max_bytes`).
        content: Vec<u8>,
    },
    /// Structured acknowledgement of a workspace file write RPC.
    WriteAck {
        /// Echoes the originating request id.
        request_id: String,
        /// Path that was written.
        path: String,
    },
    /// Reply to `Shutdown`.
    ShutdownAck,
}
