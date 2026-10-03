//! The legacy single-screen `sandbox_execute` ADK tool.

use super::capability::AdkCapability;
use super::ops::{description, invoke, ExecuteArgs};
use rfb::Sandbox;
use serde_json::{json, Value};
use std::sync::Arc;

/// Stable name of the legacy execute tool.
pub const SANDBOX_EXECUTE_NAME: &str = "sandbox_execute";

/// JSON schema for the legacy execute tool.
///
/// ```
/// let schema = rfb_adk::execute_schema();
/// assert_eq!(schema["type"], "object");
/// assert!(schema["required"].as_array().unwrap().iter().any(|v| v == "command"));
/// ```
pub fn execute_schema() -> Value {
    json!({"type":"object","properties":{"command":{"type":"string","minLength":1},"args":{"type":"array","items":{"type":"string"}},"stdin":{"type":"array","items":{"type":"integer","minimum":0,"maximum":255}},"cwd":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}},"required":["command"],"additionalProperties":false})
}

/// The legacy `sandbox_execute` ADK tool over a live [`rfb::Sandbox`]. Same
/// dispatch as the `bash` tool under a stable, single-screen name.
pub struct SandboxExecuteTool {
    sandbox: Arc<dyn Sandbox>,
}

impl SandboxExecuteTool {
    /// Wrap a concrete sandbox in the tool.
    pub fn from_sandbox(sandbox: impl Sandbox + 'static) -> Self {
        Self {
            sandbox: Arc::new(sandbox),
        }
    }
    /// Wrap an already-shared sandbox without an extra `Arc` allocation.
    pub fn from_arc(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
}

#[adk_rust::async_trait]
impl adk_rust::Tool for SandboxExecuteTool {
    fn name(&self) -> &str {
        SANDBOX_EXECUTE_NAME
    }

    fn description(&self) -> &str {
        description(AdkCapability::Execute)
    }

    fn parameters_schema(&self) -> Option<Value> {
        Some(execute_schema())
    }

    async fn execute(
        &self,
        _ctx: Arc<dyn adk_rust::ToolContext>,
        args: Value,
    ) -> adk_rust::Result<Value> {
        // Shape-check the args first so a legacy caller gets the same
        // validation the schema promises; the dispatch itself is shared.
        let _: ExecuteArgs = serde_json::from_value(args.clone())
            .map_err(|e| super::ops::invalid(format!("invalid execute args: {e}")))?;
        invoke(self.sandbox.clone(), AdkCapability::Execute, args).await
    }
}

/// Build the legacy `sandbox_execute` tool for a shared sandbox.
pub fn sandbox_execute_tool(sandbox: Arc<dyn Sandbox>) -> Arc<dyn adk_rust::Tool> {
    Arc::new(SandboxExecuteTool { sandbox })
}
