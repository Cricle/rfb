//! Unified SDK facade for RFB — the reference implementation behind the single
//! public client type [`RfbClient`].
//!
//! The three protocol implementations (forkd controller HTTP, forkd guest
//! NDJSON, ZBRT v1 binary frames) are internal adapters that reuse the crate's
//! existing `ForkdClient` / `ForkdGuestClient` and `rfb::protocol` codecs.
//! They never appear in the public API; see `sdk/UNIFIED_API.md` for the
//! surface contract and `sdk/PROTOCOL.md` for the wire contract.
//!
//! The module requires the `forkd` feature; the ZBRT transport additionally
//! requires the `zeroboot` feature.
#![cfg(feature = "forkd")]

mod error;
mod facade;
mod ndjson;
mod types;
mod validation;
#[cfg(feature = "zeroboot")]
mod zbrt;

pub use error::RfbError;
pub use facade::{GuestStream, RfbClient, Sandbox};
pub use types::{CreateOptions, ExecResult, GuestTransport, StreamEvent, StreamEventKind};

// Controller DTOs keep their `sdk/PROTOCOL.md` §1.3 serde shape; `Snapshot` is
// the unified public name for the forkd snapshot record.
pub use crate::controller::{SandboxInfo, SnapshotInfo as Snapshot};

// Guest result records reuse the shared guest contract types verbatim so the
// facade cannot drift from the wire shapes in `sdk/PROTOCOL.md` §2.4.
pub use crate::guest::ReadResult as FileRead;
pub use crate::guest::{DirEntry, GrepMatch};
