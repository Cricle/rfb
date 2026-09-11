//! Rig adapters for RFB.

/// Adapters that expose an RFB sandbox as Rig tools.
pub mod adapter;
pub use adapter::*;

mod execution;
pub use execution::{ExecutionError, ExecutionTarget, GuestExecution};

/// Public guest execution facade shared by services and RFB tool bridges.
///
/// The target owns guest metadata as opaque strings; it never converts a guest
/// path into a host [`std::path::Path`]. `LocalForTests` and `ZeroBoot` policy
/// decisions intentionally remain outside this crate.
///
/// ```
/// # use rfb_rig::{ExecutionTarget, GuestExecution};
/// # let target = ExecutionTarget::unsupported("not provisioned");
/// assert!(target.capabilities().is_empty());
/// ```
///
/// The implementation is provided by [`ExecutionTarget`].
pub struct GuestExecutionFacade;
