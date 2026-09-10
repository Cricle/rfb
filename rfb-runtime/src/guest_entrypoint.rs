//! Public guest startup API shared by the runtime binary and embedders.

use crate::config::RuntimeConfig;
use crate::resources::RuntimeLimits;
use std::io;

/// Guest transport selected by an embedding process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestTransport {
    /// Listen for RFB1 connections on the configured guest vsock port.
    Vsock,
    /// Serve the forkd NDJSON agent at its configured address.
    #[cfg(feature = "forkd")]
    ForkdAgent,
    /// Serve the ZeroBoot V1 (ZBRT) guest on the ZBRT guest vsock port.
    #[cfg(feature = "zeroboot")]
    ZeroBoot,
}

/// Start the reusable guest RFB1 entrypoint.
pub async fn run(transport: GuestTransport) -> io::Result<()> {
    #[cfg(feature = "forkd")]
    if let GuestTransport::ForkdAgent = transport {
        return crate::agent::run(&RuntimeConfig::from_environment().forkd_agent_addr).await;
    }
    #[cfg(feature = "zeroboot")]
    let zeroboot = matches!(transport, GuestTransport::ZeroBoot);
    #[cfg(not(feature = "zeroboot"))]
    let zeroboot = false;
    crate::guest_boot::boot_environment(matches!(transport, GuestTransport::Vsock) || zeroboot)?;
    crate::environment_loader::load_image_environment()?;
    let limits = RuntimeLimits::default();
    limits
        .validate()
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    match transport {
        GuestTransport::Vsock => crate::guest_vsock::run(limits).await,
        #[cfg(feature = "zeroboot")]
        GuestTransport::ZeroBoot => crate::zeroboot_guest::run(limits).await,
        #[cfg(feature = "forkd")]
        GuestTransport::ForkdAgent => unreachable!("forkd agent handled above"),
    }
}

/// Load the configured image environment without starting a transport.
pub fn load_environment() -> io::Result<()> {
    crate::environment_loader::load_image_environment()
}

/// Read the effective guest configuration.
pub fn config() -> RuntimeConfig {
    RuntimeConfig::from_environment()
}
