//! Test-only re-export stub for the `include!` paste seam in
//! `tests/firecracker_controller.rs`: the pasted driver source declares
//! `mod socket;`, which resolves to this file. Re-expose the real
//! `socket.rs` under the same names (re-export widens the `pub(crate)`
//! helpers so the pasted module and its nested tests can reach them).

#[path = "../../src/firecracker_core/firecracker/socket.rs"]
pub mod socket;

pub use super::FirecrackerError;
pub use socket::{
    endpoint_identity, parse_api_response, parse_response_head, read_http_response, read_response,
    readiness_error, remove_owned_socket, remove_snapshot_file, remove_stale_socket,
    snapshot_file_ready, EndpointIdentity, FC_SOCKET_TIMEOUT,
};
