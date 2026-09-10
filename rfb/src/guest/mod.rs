//! Guest-facing operations for a sandboxed runtime.
//!
//! These are typed, platform-neutral DTOs and traits that mirror the operations
//! a forkd guest exposes over its newline-delimited JSON protocol: `ping`,
//! `stream`, and the structured, read-only filesystem RPCs (`ls`/`find`/`grep`)
//! plus `read`/`write`/`eval`/`cancel`.
//!
//! Every request type carries a [`validate`](crate::ContractError) method and
//! enforces guest-side limits. Paths are always interpreted inside the guest
//! and are never converted to host paths. The public API never leaks
//! `serde_json::Value`; all payloads are typed DTOs.
//!
//! A backend that does not implement an operation keeps the corresponding
//! [`Sandbox`](crate::Sandbox) default, which returns
//! [`SandboxError::UnsupportedCapability`](crate::SandboxError).

mod cancel;
mod eval;
mod filesystem;
mod health;
mod io;
mod limits;
mod operations;
mod stream;

pub use cancel::{CancelRequest, CancelResult};
pub use eval::{EvalRequest, EvalResult};
pub use filesystem::{
    DirEntry, FindRequest, FindResult, GrepMatch, GrepRequest, GrepResult, LsRequest, LsResult,
};
pub use health::Health;
pub use io::{ReadRequest, ReadResult, WriteRequest, WriteResult};
pub use limits::{
    MAX_GUEST_CODE_BYTES, MAX_GUEST_PATH_BYTES, MAX_GUEST_PATTERN_BYTES, MAX_GUEST_RESULTS,
    MAX_GUEST_RESULT_BYTES,
};
pub use operations::{GuestOperations, OperationsConfig};
pub use stream::{GuestStream, StreamEvent, StreamSpec};
