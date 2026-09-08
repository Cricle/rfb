//! Runtime configuration: every port, path, and timeout the runtime uses is
//! parameterized here with defaults equal to the historical values, so an
//! operator can override any of them via environment variables without
//! changing code. The image-declared `/etc/rfb-runtime/environment` file and
//! the process environment both feed these fields (process wins).

use std::path::PathBuf;
use std::time::Duration;

/// The fixed vsock guest port the production RFB1 runtime listens on.
pub const DEFAULT_VSOCK_PORT: u32 = 5000;

/// Resolve the RFB1 guest port. Release builds deliberately ignore the
/// environment so production cannot drift from the image contract. Debug
/// builds may override it for local development with an explicit variable.
fn configured_vsock_port() -> u32 {
    #[cfg(debug_assertions)]
    {
        env_u32("RFB_RUNTIME_DEV_VSOCK_PORT", DEFAULT_VSOCK_PORT)
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_VSOCK_PORT
    }
}
/// The TCP address the forkd guest agent binds
/// (`FORKD_AGENT_ADDR`, default `0.0.0.0:8888`).
pub const DEFAULT_FORKD_AGENT_ADDR: &str = "0.0.0.0:8888";
/// The opaque guest workspace root (`RFB_RUNTIME_WORKSPACE`, default `/workspace`).
pub const DEFAULT_WORKSPACE: &str = "/workspace";
/// The image-declared environment file (`RFB_RUNTIME_ENVIRONMENT`,
/// default `/etc/rfb-runtime/environment`).
pub const DEFAULT_ENVIRONMENT_PATH: &str = "/etc/rfb-runtime/environment";

/// Comprehensive runtime tuning knobs. Every field has a default that equals
/// the historical hard-coded value; none are mandatory.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// vsock port for the RFB1 guest runtime.
    pub vsock_port: u32,
    /// TCP bind address for the forkd guest agent.
    pub forkd_agent_addr: String,
    /// Opaque guest workspace root (paths stay confined beneath it).
    pub workspace_root: PathBuf,
    /// Image-declared environment file loaded at boot.
    pub environment_path: PathBuf,
    /// Per-connection socket/read timeouts.
    pub socket_timeout: Duration,
    /// Startup readiness deadline for the vsock listener.
    pub listen_wait: Duration,
    /// Default exec timeout applied when a request carries none.
    pub exec_default_timeout: Duration,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            vsock_port: DEFAULT_VSOCK_PORT,
            forkd_agent_addr: DEFAULT_FORKD_AGENT_ADDR.to_owned(),
            workspace_root: PathBuf::from(DEFAULT_WORKSPACE),
            environment_path: PathBuf::from(DEFAULT_ENVIRONMENT_PATH),
            socket_timeout: Duration::from_secs(10),
            listen_wait: Duration::from_secs(5),
            exec_default_timeout: Duration::from_secs(1800),
        }
    }
}

#[cfg(debug_assertions)]
fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

fn env_secs(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

impl RuntimeConfig {
    /// Build a config from the current environment, applying process
    /// overrides on top of the defaults. The image-declared file has already
    /// been merged into the process environment by the time this runs, so
    /// process variables win by construction.
    pub fn from_environment() -> Self {
        Self {
            vsock_port: configured_vsock_port(),
            forkd_agent_addr: std::env::var("FORKD_AGENT_ADDR")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_FORKD_AGENT_ADDR.to_owned()),
            workspace_root: std::env::var("RFB_RUNTIME_WORKSPACE")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_WORKSPACE)),
            environment_path: std::env::var("RFB_RUNTIME_ENVIRONMENT")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_ENVIRONMENT_PATH)),
            socket_timeout: env_secs("RFB_RUNTIME_SOCKET_TIMEOUT", Duration::from_secs(10)),
            listen_wait: env_secs("RFB_RUNTIME_LISTEN_WAIT", Duration::from_secs(5)),
            exec_default_timeout: env_secs(
                "RFB_RUNTIME_EXEC_DEFAULT_TIMEOUT",
                Duration::from_secs(1800),
            ),
        }
    }
}
