//! Service-neutral guest execution targets and error mapping.
//!
//! [`ExecutionTarget`] is the unified guest surface used by service adapters:
//! it forwards `exec`, `eval`, and structured tool RPCs to a provisioned
//! [`rfb::Sandbox`], or fails closed when no backend policy is selected.
//!
//! ```
//! use rfb_rig::{ExecutionError, ExecutionTarget, GuestExecution};
//!
//! // A target with no backing policy is always fail-closed.
//! let target = ExecutionTarget::unsupported("not provisioned");
//! let error = futures::executor::block_on(target.exec(vec!["id".into()], None)).unwrap_err();
//! assert!(matches!(error, ExecutionError::Unsupported(reason) if reason == "not provisioned"));
//! ```

use rfb::{guest, ExecSpec, Sandbox, SandboxError};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Errors returned by the guest execution facade.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    /// The requested operation is not available.
    #[error("unsupported guest operation: {0}")]
    Unsupported(String),
    /// The request failed validation.
    #[error("invalid guest request: {0}")]
    Invalid(String),
    /// The guest reported an execution failure.
    #[error("guest execution failed: {0}")]
    Guest(String),
    /// The guest operation exceeded its timeout.
    #[error("guest operation timed out")]
    Timeout,
}

impl From<SandboxError> for ExecutionError {
    fn from(value: SandboxError) -> Self {
        match value {
            SandboxError::Timeout => Self::Timeout,
            SandboxError::UnsupportedCapability(c) => Self::Unsupported(format!("{c:?}")),
            SandboxError::NotReady => Self::Guest("sandbox is not ready".into()),
            SandboxError::Transport(s) | SandboxError::Execution(s) => Self::Guest(s),
            SandboxError::InvalidSpec(e) => Self::Invalid(e.to_string()),
        }
    }
}

/// A service-neutral target for guest command, eval, and structured RPCs.
#[derive(Clone)]
pub enum ExecutionTarget {
    /// A provisioned RFB sandbox. Guest paths remain opaque strings.
    Guest {
        /// The sandbox backing this target.
        sandbox: Arc<dyn Sandbox>,
        /// Opaque guest working directory prefilled on every request.
        guest_cwd: String,
    },
    /// Explicitly unavailable target, useful for service-owned policies.
    Unsupported {
        /// Explanation returned when an operation is attempted.
        reason: String,
    },
}

impl std::fmt::Debug for ExecutionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Guest { guest_cwd, .. } => f
                .debug_struct("Guest")
                .field("guest_cwd", guest_cwd)
                .finish_non_exhaustive(),
            Self::Unsupported { reason } => f
                .debug_struct("Unsupported")
                .field("reason", reason)
                .finish(),
        }
    }
}

impl ExecutionTarget {
    /// Construct a guest target with an opaque guest working directory.
    pub fn guest(sandbox: Arc<dyn Sandbox>, guest_cwd: impl Into<String>) -> Self {
        Self::Guest {
            sandbox,
            guest_cwd: guest_cwd.into(),
        }
    }
    /// Construct a fail-closed target without selecting a backend policy.
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }
    /// Advertised capabilities of this target.
    pub fn capabilities(&self) -> &[rfb::Capability] {
        match self {
            Self::Guest { sandbox, .. } => sandbox.capabilities(),
            Self::Unsupported { .. } => &[],
        }
    }
}

/// Unified execution operations used by service adapters.
pub trait GuestExecution {
    /// Execute a command in the guest.
    ///
    /// `args` must be non-empty. In flight the [`Sandbox::exec`] result is
    /// mapped into a JSON `{status, stdout, stderr, timed_out}` object; a
    /// sandbox-reported timeout becomes [`ExecutionError::Timeout`].
    fn exec<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Option<Duration>,
    ) -> rfb::BoxFuture<'a, Result<Value, ExecutionError>>;
    /// Evaluate code in the guest.
    fn eval<'a>(
        &'a self,
        code: String,
        timeout: Option<Duration>,
    ) -> rfb::BoxFuture<'a, Result<Value, ExecutionError>>;
    /// Execute a structured guest tool through the shared RFB bridge.
    ///
    /// Supported tools are the runtime `read`/`write`/`grep`/`find`/`ls`/`eval`/
    /// `bash`/`execute` names; any other tool is rejected as
    /// [`ExecutionError::Unsupported`] before reaching the sandbox.
    fn structured<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
    ) -> rfb::BoxFuture<'a, Result<Value, ExecutionError>>;
}

impl GuestExecution for ExecutionTarget {
    fn exec<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Option<Duration>,
    ) -> rfb::BoxFuture<'a, Result<Value, ExecutionError>> {
        Box::pin(async move {
            let (command, rest) = args
                .split_first()
                .ok_or_else(|| ExecutionError::Invalid("empty command".into()))?;
            match self {
                Self::Guest { sandbox, guest_cwd } => {
                    let mut spec = ExecSpec::new(command);
                    spec.args = rest.to_vec();
                    spec.cwd = Some(guest_cwd.clone());
                    spec.timeout = timeout;
                    spec.validate()
                        .map_err(|e| ExecutionError::Invalid(e.to_string()))?;
                    let r = sandbox.exec(spec).await?;
                    if r.timed_out {
                        return Err(ExecutionError::Timeout);
                    }
                    Ok(
                        serde_json::json!({"status": r.status, "stdout": String::from_utf8_lossy(&r.stdout), "stderr": String::from_utf8_lossy(&r.stderr), "timed_out": false}),
                    )
                }
                Self::Unsupported { reason } => Err(ExecutionError::Unsupported(reason.clone())),
            }
        })
    }
    fn eval<'a>(
        &'a self,
        code: String,
        timeout: Option<Duration>,
    ) -> rfb::BoxFuture<'a, Result<Value, ExecutionError>> {
        Box::pin(async move {
            match self {
                Self::Guest { sandbox, guest_cwd } => {
                    let mut r = guest::EvalRequest::new(code);
                    r.cwd = Some(guest_cwd.clone());
                    r.timeout = timeout;
                    r.validate()
                        .map_err(|e| ExecutionError::Invalid(e.to_string()))?;
                    Ok(serde_json::to_value(sandbox.eval(r).await?).unwrap())
                }
                Self::Unsupported { reason } => Err(ExecutionError::Unsupported(reason.clone())),
            }
        })
    }
    fn structured<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
    ) -> rfb::BoxFuture<'a, Result<Value, ExecutionError>> {
        Box::pin(async move {
            match self {
                Self::Guest { sandbox, .. } => {
                    let bridge = crate::RigPortableTools::from_sandbox(sandbox.clone());
                    Ok(bridge
                        .invoke(
                            match tool {
                                "read" => crate::RigCapability::Read,
                                "write" => crate::RigCapability::Write,
                                "grep" => crate::RigCapability::Grep,
                                "find" => crate::RigCapability::Find,
                                "ls" => crate::RigCapability::Ls,
                                "eval" => crate::RigCapability::Eval,
                                "bash" | "execute" => crate::RigCapability::Bash,
                                x => return Err(ExecutionError::Unsupported(x.into())),
                            },
                            args,
                        )
                        .await
                        .map_err(|e| ExecutionError::Guest(e.to_string()))?
                        .as_json()
                        .cloned()
                        .unwrap_or(Value::Null))
                }
                Self::Unsupported { reason } => Err(ExecutionError::Unsupported(reason.clone())),
            }
        })
    }
}
