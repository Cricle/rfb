//! ADK adapters for the typed RFB core guest contract.
//!
//! This module exposes structured guest operations as ADK function tools and
//! keeps capability registration aligned with the backing sandbox.
//!
//! Structured guest RPCs call the corresponding `Sandbox` method directly;
//! they are never emulated through `exec`.

mod agent;
mod closure;
mod capability;
mod execute;
mod ops;
mod tools;

pub use agent::{
    sandbox_agent, sandbox_agent_with_model, SandboxAgent, SandboxAgentConfig, SandboxAgentReply,
};
pub use capability::AdkCapability;
pub use closure::ClosureTool;
pub use execute::{execute_schema, sandbox_execute_tool, SandboxExecuteTool, SANDBOX_EXECUTE_NAME};
pub use ops::sandbox_error_to_adk;
pub use tools::{sandbox_tools, SandboxTools};

/// Stable tool names exposed by the ADK adapter.
///
/// The adapter only registers these tools when the backing sandbox advertises
/// the corresponding core capability (edit requires both read and write).
pub const TOOL_NAMES: [&str; 7] = ["read", "write", "edit", "bash", "grep", "find", "ls"];

/// Liveness classification attached to execute results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The result reflects a core-only sandbox (no live VM extras).
    CoreOnly,
}
impl Liveness {
    /// Stable machine-readable label.
    pub(crate) fn as_str(self) -> &'static str {
        "core_only"
    }
}
