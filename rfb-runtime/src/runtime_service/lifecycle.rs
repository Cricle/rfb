//! Runtime construction: backend selection from the environment and the
//! fail-closed executor wiring.

use super::executor::GuestExecutor;
use super::executor::UnavailableExecutor;
use super::RuntimeService;
use crate::resources::RuntimeLimits;
use std::collections::HashMap;

/// Declared product backend for the runtime service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeBackend {
    /// forkd owns Firecracker lifecycle; the runtime is guest-only.
    Forkd,
    /// An unsupported backend name was declared.
    Unsupported(String),
}

/// Parse the `RFB_RUNTIME_BACKEND` value into a [`RuntimeBackend`].
pub fn parse_runtime_backend(value: Option<&str>) -> RuntimeBackend {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    match value.map(|value| value.to_ascii_lowercase()).as_deref() {
        None | Some("forkd") => RuntimeBackend::Forkd,
        Some(value) => RuntimeBackend::Unsupported(value.to_string()),
    }
}

impl RuntimeService {
    /// Construct a service from the environment with default limits.
    pub fn from_environment() -> Self {
        Self::from_environment_with_limits(RuntimeLimits::default())
    }

    /// Construct a service from the environment with explicit limits.
    pub fn from_environment_with_limits(limits: RuntimeLimits) -> Self {
        // The RFB1 workspace image declares `RFB_RUNTIME_EXECUTOR=workspace`
        // and `RFB_RUNTIME_WORKSPACE=/workspace` in /etc/rfb-runtime/environment
        // and runs as `init=/sbin/rfb-runtime vsock`. That is the only path
        // that constructs a real guest executor from the environment; anything
        // else fails closed (the forkd TCP controller owns VM lifecycle).
        if std::env::var("RFB_RUNTIME_EXECUTOR").ok().as_deref() == Some("workspace") {
            let root = crate::config::RuntimeConfig::from_environment().workspace_root;
            match crate::workspace_executor::WorkspaceGuestExecutor::new(root, limits.clone()) {
                Ok(executor) => {
                    return Self::with_executor(limits, Box::new(executor));
                }
                Err(message) => {
                    return Self::with_executor(
                        limits,
                        Box::new(UnavailableExecutor(format!(
                            "workspace executor init failed: {message}"
                        ))),
                    );
                }
            }
        }
        let backend = parse_runtime_backend(
            std::env::var_os("RFB_RUNTIME_BACKEND")
                .as_deref()
                .and_then(|v| v.to_str()),
        );
        let reason = match backend {
            RuntimeBackend::Forkd => "forkd owns Firecracker lifecycle; rfb-runtime is guest-only and must not start a local backend".to_string(),
            RuntimeBackend::Unsupported(backend) => format!("unsupported backend: {backend}; use forkd"),
        };
        Self::with_executor(
            limits,
            Box::new(UnavailableExecutor(format!(
                "runtime backend unavailable: {reason}"
            ))),
        )
    }

    /// Construct a service with a real or test guest executor. No process cwd,
    /// local VM, or implicit command fallback is introduced by this seam.
    pub fn with_executor(limits: RuntimeLimits, executor: Box<dyn GuestExecutor>) -> Self {
        Self {
            sequence: 0,
            protocol_ready: false,
            completed_requests: HashMap::new(),
            completed_order: std::collections::VecDeque::new(),
            evicted_requests: std::collections::VecDeque::new(),
            active_sessions: HashMap::new(),
            cancelled_sessions: HashMap::new(),
            cancelled_order: std::collections::VecDeque::new(),
            shutdown: false,
            limits,
            executor: Some(executor),
            turn_cancel: None,
        }
    }

    /// Construct a service with a concrete test/embedded executor type.
    pub fn with_executor_impl<E: GuestExecutor + 'static>(
        limits: RuntimeLimits,
        executor: E,
    ) -> Self {
        Self::with_executor(limits, Box::new(executor))
    }
}
