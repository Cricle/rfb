//! ADK (adk-rust) agent integration for RFB.
//!
//! Exposes an RFB sandbox as ADK function tools and assembles a ready-to-run
//! [`adk_rust::agent::LlmAgent`] over them; the application owns the model
//! credentials, prompts, and session lifecycle.

/// Adapters that expose an RFB sandbox as ADK tools and agents.
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
/// # use rfb_adk::{ExecutionTarget, GuestExecution};
/// # let target = ExecutionTarget::unsupported("not provisioned");
/// assert!(target.capabilities().is_empty());
/// ```
///
/// The implementation is provided by [`ExecutionTarget`].
pub struct GuestExecutionFacade;
