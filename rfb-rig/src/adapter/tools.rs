//! The typed seven-tool surface over a live sandbox.

use crate::adapter::capability::RigCapability;
use crate::adapter::ops::{invoke, metadata, unsupported};
use rfb::Sandbox;
use rig_core::tool::{PortableDynamicTool, ToolExecutionError, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

/// The typed seven-tool surface over a live sandbox. Tools are only registered
/// when the backing sandbox advertises the corresponding core capability.
#[derive(Clone)]
pub struct RigPortableTools {
    sandbox: Arc<dyn Sandbox>,
    capabilities: Vec<RigCapability>,
}
impl std::fmt::Debug for RigPortableTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RigPortableTools")
            .field("capabilities", &self.capabilities)
            .finish()
    }
}
impl RigPortableTools {
    /// Build a tool surface with the given capabilities, keeping only those
    /// the sandbox advertises.
    pub fn new(
        sandbox: Arc<dyn Sandbox>,
        capabilities: impl IntoIterator<Item = RigCapability>,
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
                RigCapability::Read,
                RigCapability::Write,
                RigCapability::Edit,
                RigCapability::Bash,
                RigCapability::Grep,
                RigCapability::Find,
                RigCapability::Ls,
            ],
        )
    }
    /// The capabilities this surface actually registered.
    pub fn capabilities(&self) -> &[RigCapability] {
        &self.capabilities
    }
    /// Build one portable tool per registered capability.
    pub fn tools(&self) -> Vec<PortableDynamicTool> {
        self.capabilities
            .iter()
            .copied()
            .map(|c| self.tool(c))
            .collect()
    }
    /// Build the portable tool for one capability.
    pub fn tool(&self, c: RigCapability) -> PortableDynamicTool {
        let sb = self.sandbox.clone();
        let (n, d, s) = metadata(c);
        PortableDynamicTool::new(n, d, s, move |v| {
            let sb = sb.clone();
            Box::pin(async move { invoke(sb, c, v).await })
        })
    }
    /// Invoke a capability directly with JSON arguments.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn invoke(
        &self,
        c: RigCapability,
        v: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        if !self.capabilities.contains(&c) {
            return Err(unsupported(c));
        }
        invoke(self.sandbox.clone(), c, v).await
    }
}

/// Build the default tool surface for a shared sandbox.
pub fn rig_tools(sandbox: Arc<dyn Sandbox>) -> RigPortableTools {
    RigPortableTools::from_sandbox(sandbox)
}

/// Build the default portable tools for a shared sandbox (Rig `toolbox` input).
pub fn portable_dynamic_tools(sandbox: Arc<dyn Sandbox>) -> Vec<PortableDynamicTool> {
    rig_tools(sandbox).tools()
}
