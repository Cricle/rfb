//! Test-only re-export stub for the `include!` paste seam in
//! `tests/firecracker_controller.rs`: the pasted driver source declares
//! `mod snapshot;`, which resolves to this file. Re-expose the real
//! `snapshot.rs` along with the sibling items its `use super::` paths
//! resolve against.

#[path = "../../src/firecracker_core/firecracker/snapshot.rs"]
pub mod snapshot;

pub use super::{FirecrackerError, FirecrackerVm};
