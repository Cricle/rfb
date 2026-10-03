//! The typed seven-tool surface over a live sandbox, as ADK function tools.

use super::capability::AdkCapability;
use super::ops::{description, invoke, name, schema, unsupported};
use rfb::Sandbox;
use serde_json::Value;
use std::sync::Arc;

/// One sandbox capability exposed as an [`adk_rust::Tool`]. Thin on purpose:
/// all semantics live in `ops::invoke`, which is directly testable without an
/// ADK context.
pub struct SandboxTool {
    kind: AdkCapability,
    sandbox: Arc<dyn Sandbox>,
}

#[adk_rust::async_trait]
impl adk_rust::Tool for SandboxTool {
    fn name(&self) -> &str {
        name(self.kind)
    }

    fn description(&self) -> &str {
        description(self.kind)
    }

    fn parameters_schema(&self) -> Option<Value> {
        Some(schema(self.kind))
    }

    async fn execute(
        &self,
        _ctx: Arc<dyn adk_rust::ToolContext>,
        args: Value,
    ) -> adk_rust::Result<Value> {
        invoke(self.sandbox.clone(), self.kind, args).await
    }
}

/// The typed tool surface over a live sandbox. Tools are only registered when
/// the backing sandbox advertises the corresponding core capability.
#[derive(Clone)]
pub struct SandboxTools {
    sandbox: Arc<dyn Sandbox>,
    capabilities: Vec<AdkCapability>,
}
impl std::fmt::Debug for SandboxTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxTools")
            .field("capabilities", &self.capabilities)
            .finish()
    }
}
impl SandboxTools {
    /// Build a tool surface with the given capabilities, keeping only those
    /// the sandbox advertises.
    pub fn new(
        sandbox: Arc<dyn Sandbox>,
        capabilities: impl IntoIterator<Item = AdkCapability>,
    ) -> Self {
        Self {
            capabilities: capabilities
                .into_iter()
                .filter(|c| c.available(sandbox.as_ref()))
                .collect(),
            sandbox,
        }
    }
    /// Build the default seven-tool surface (read/write/edit/bash/grep/find/ls).
    pub fn from_sandbox(sandbox: Arc<dyn Sandbox>) -> Self {
        Self::new(
            sandbox,
            [
                AdkCapability::Read,
                AdkCapability::Write,
                AdkCapability::Edit,
                AdkCapability::Bash,
                AdkCapability::Grep,
                AdkCapability::Find,
                AdkCapability::Ls,
            ],
        )
    }
    /// The capabilities this surface actually registered.
    pub fn capabilities(&self) -> &[AdkCapability] {
        &self.capabilities
    }
    /// Build one ADK tool per registered capability.
    pub fn tools(&self) -> Vec<Arc<dyn adk_rust::Tool>> {
        self.capabilities
            .iter()
            .copied()
            .map(|c| self.tool(c))
            .collect()
    }
    /// Build the ADK tool for one capability.
    pub fn tool(&self, c: AdkCapability) -> Arc<dyn adk_rust::Tool> {
        Arc::new(SandboxTool {
            kind: c,
            sandbox: self.sandbox.clone(),
        })
    }
    /// Invoke a capability directly with JSON arguments (bypasses the ADK
    /// tool dispatch; the same semantics the tool's `execute` runs).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn invoke(&self, c: AdkCapability, v: Value) -> Result<Value, adk_rust::AdkError> {
        if !self.capabilities.contains(&c) {
            return Err(unsupported(c));
        }
        invoke(self.sandbox.clone(), c, v).await
    }
}

/// Build the default seven-tool surface for a shared sandbox.
pub fn sandbox_tools(sandbox: Arc<dyn Sandbox>) -> SandboxTools {
    SandboxTools::from_sandbox(sandbox)
}
