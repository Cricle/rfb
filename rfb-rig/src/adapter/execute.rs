//! The legacy single-screen `sandbox_execute` Rig tool.

use crate::adapter::ops::{decode, invalid, map_sandbox_error};
use crate::adapter::Liveness;
use rfb::{ExecSpec, Sandbox};
use rig_core::tool::{PortableDynamicTool, ToolExecutionError, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Stable name of the legacy execute tool.
pub const SANDBOX_EXECUTE_NAME: &str = "sandbox_execute";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteArgs {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    stdin: Option<Vec<u8>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// JSON schema for the legacy execute tool.
///
/// ```
/// let schema = rfb_rig::execute_schema();
/// assert_eq!(schema["type"], "object");
/// assert!(schema["required"].as_array().unwrap().iter().any(|v| v == "command"));
/// ```
pub fn execute_schema() -> Value {
    json!({"type":"object","properties":{"command":{"type":"string","minLength":1},"args":{"type":"array","items":{"type":"string"}},"stdin":{"type":"array","items":{"type":"integer","minimum":0,"maximum":255}},"cwd":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}},"required":["command"],"additionalProperties":false})
}

/// Adapter exposing the legacy `sandbox_execute` Rig tool over a live
/// [`rfb::Sandbox`].
#[derive(Clone)]
pub struct SandboxExecuteAdapter {
    sandbox: Arc<dyn Sandbox>,
}
impl std::fmt::Debug for SandboxExecuteAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxExecuteAdapter")
            .finish_non_exhaustive()
    }
}
impl SandboxExecuteAdapter {
    /// Wrap a concrete sandbox in the adapter.
    pub fn new(sandbox: impl Sandbox + 'static) -> Self {
        Self {
            sandbox: Arc::new(sandbox),
        }
    }
    /// Wrap an already-shared sandbox without an extra `Arc` allocation.
    pub fn from_arc(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
    /// Build the legacy execute tool with a shared sandbox.
    pub fn new_with_context(sandbox: Arc<dyn Sandbox>) -> PortableDynamicTool {
        Self { sandbox }.tool()
    }
    /// Build the portable tool from this adapter.
    pub fn tool(&self) -> PortableDynamicTool {
        let a = self.clone();
        PortableDynamicTool::new(
            SANDBOX_EXECUTE_NAME,
            "Execute a command in the live RFB sandbox.",
            execute_schema(),
            move |v| {
                let a = a.clone();
                Box::pin(async move { a.execute(v).await })
            },
        )
    }
    /// Execute an `ExecuteArgs` JSON value against the sandbox.
    pub async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolExecutionError> {
        let a: ExecuteArgs = decode(arguments)?;
        // The bash/execute tool speaks a shell-string contract: the command is
        // run through the guest shell (`/bin/sh -c`), so pipelines, `&&`, and
        // redirections work as the model expects. Structured `args` are
        // appended as positional arguments for direct-exec callers.
        let mut s = ExecSpec::new("/bin/sh");
        s.args = vec!["-c".into(), a.command.clone()];
        s.args.extend(a.args);
        s.stdin = a.stdin;
        s.cwd = a.cwd;
        s.timeout = a.timeout_ms.map(Duration::from_millis);
        s.validate().map_err(|e| invalid(e.to_string()))?;
        let r = self.sandbox.exec(s).await.map_err(map_sandbox_error)?;
        if r.timed_out {
            return Err(ToolExecutionError::timeout("command execution timed out")
                .with_code("execution_timeout")
                .redact_model_feedback());
        }
        Ok(ToolOutput::json(
            json!({"status":r.status,"stdout":String::from_utf8_lossy(&r.stdout),"stderr":String::from_utf8_lossy(&r.stderr),"timed_out":false,"liveness":Liveness::CoreOnly.as_str()}),
        ))
    }
}

/// Build the legacy `sandbox_execute` tool for a shared sandbox.
pub fn sandbox_execute_tool(sandbox: Arc<dyn Sandbox>) -> PortableDynamicTool {
    SandboxExecuteAdapter::new_with_context(sandbox)
}
