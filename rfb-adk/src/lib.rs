//! ADK (adk-rust) agent integration for RFB.
//!
//! One tool surface, two ways to run an agent over it (same `Llm` clients,
//! same `Tool` impls, interchangeable):
//!
//! - [`adapter::sandbox_agent`]: the full adk assembly (LlmAgent + Runner +
//!   session service) for session management and multi-agent machinery.
//! - [`loop_agent`]: a lightweight loop on the same adk components — ideas
//!   borrowed from pi (the loop IS the agent; steering/abort/errors are all
//!   plain data), not a port and not a Runner replacement.

/// Adapters that expose an RFB sandbox as ADK tools and agents.
pub mod adapter;
pub use adapter::*;

/// 轻量 agent 循环 on adk 组件（pi 思想，adk 生态）：动态 [`loop_agent::
/// Toolset`]、转向、中止、输出截断。
pub mod loop_agent;
pub use loop_agent::{
    agent_loop, AbortFlag, LoopEvent, LoopOptions, LoopOutcome, SteeringInbox, Toolset,
};

/// 按需沙箱会话：默认零沙箱，agent 调 `sandbox_start` 才引导并解锁工具。
pub mod session;
pub use session::{
    install_session_tools, SandboxSetup, SandboxState, SANDBOX_START_NAME, SANDBOX_STOP_NAME,
};

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
