//! RFB runtime guest services: the RFB1 framed codec, the session/control wire
//! contracts, the guest runtime service (identity/sequencing/lifecycle), the
//! workspace executor, the forkd NDJSON guest agent, and optional Firecracker
//! lifecycle support.
//!
//! Protocol types can be used without starting a runtime:
//! ```
//! use rfb_runtime::session::RuntimeEvent;
//!
//! let event = RuntimeEvent::Completed { sequence: 7, exit_code: Some(0) };
//! assert_eq!(event.kind_label(), "turn.completed");
//! assert!(event.is_terminal());
//! ```

/// The forkd TCP guest agent (NDJSON).
#[cfg(feature = "forkd")]
pub mod agent;
/// Guest kernel command line builder shared by every Firecracker driver.
pub mod boot_args;
/// RFB1 frame encoder/decoder with runtime limits.
pub mod codec;
/// Runtime configuration (ports, paths, timeouts — all overridable).
pub mod config;
#[cfg(feature = "guest")]
pub(crate) mod environment_loader;
/// Minimal fake runtime for tests.
pub mod fake;
/// Firecracker VM lifecycle support.
#[cfg(feature = "firecracker")]
pub mod firecracker;
/// Firecracker API/socket internals (Linux only).
#[cfg(all(feature = "firecracker", target_os = "linux"))]
pub mod firecracker_core;
/// PID-1 guest boot helpers (Linux only).
#[cfg(feature = "guest")]
pub mod guest;
#[cfg(feature = "guest")]
pub(crate) mod guest_boot;
#[cfg(all(feature = "guest", target_os = "linux"))]
pub(crate) mod guest_connection;
/// Reusable guest process entrypoint and transport runners.
#[cfg(feature = "guest")]
pub mod guest_entrypoint;
/// Guest-facing operation DTOs.
#[cfg(feature = "guest")]
pub mod guest_protocol;
#[cfg(feature = "guest")]
pub(crate) mod guest_vsock;
/// Host-side Firecracker `/vsock` UDS relay and RFB1 client.
#[cfg(feature = "host-vsock")]
pub mod host_vsock;
/// Optional embedded guest interpreters (`python3` via RustPython, `lua` via
/// mlua), dispatched multi-call from `/init` by `argv[0]`.
#[cfg(any(feature = "rustpython", feature = "mlua"))]
pub mod interpreters;
/// Path policy: workspace confinement and read-only host roots.
pub mod policy;
/// Runtime resource limits.
pub mod resources;
/// Guest runtime state machine (identity, sequencing, lifecycle).
#[cfg(feature = "guest")]
pub mod runtime_service;
/// Sandbox backend classification.
pub mod sandbox;
/// RFB1 session/control wire contracts.
pub mod session;
/// Linux guest transport over tokio-vsock.
#[cfg(all(feature = "guest", target_os = "linux"))]
pub mod vsock;
/// Firecracker vsock UDS relay handshake (single shared implementation).
#[cfg(feature = "core")]
pub mod vsock_relay;
/// Serial, shell-free workspace executor with a cancellable active child.
#[cfg(feature = "guest")]
pub mod workspace_executor;

pub use codec::{Frame, FrameCodec, MessageType};
pub use policy::{PathPolicy, PolicyError};

/// The canonical RFB1 wire protocol version. All runtime/host handshakes must
/// quote this constant; the image marker (`image/protocol-version`) and the
/// rootfs `/etc/rfb-runtime/protocol-version` file must match it.
pub const PROTOCOL_VERSION: u16 = 1;

/// The canonical ZeroBoot V1 (ZBRT) wire protocol primitives shared by host,
/// guest, and Firecracker integrations. `rfb::protocol` re-exports these items
/// so both crates share one implementation without a dependency cycle.
#[cfg(feature = "core")]
pub mod zeroboot_protocol;

/// ZeroBoot V1 guest connection: ZBRT frames driven through the RFB1
/// [`runtime_service::RuntimeService`] and
/// [`workspace_executor::WorkspaceGuestExecutor`] semantics.
#[cfg(feature = "zeroboot")]
pub mod zeroboot_connection;

/// ZeroBoot V1 guest vsock listener service.
#[cfg(feature = "zeroboot")]
pub mod zeroboot_guest;

/// Generic host-side runtime lifecycle orchestration.
#[cfg(feature = "core")]
pub mod orchestration;
