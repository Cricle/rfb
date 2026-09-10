//! ZeroBoot V1 guest vsock listener service.
//!
//! Each accepted connection gets its own
//! [`crate::runtime_service::RuntimeService`] backed by a
//! [`crate::workspace_executor::WorkspaceGuestExecutor`], then is served by
//! [`crate::zeroboot_connection::serve`]. The ZBRT wire (magic `ZBRT`,
//! version 1) runs on the same guest vsock port (5000) as the RFB1 guest
//! transport; only one protocol serves a given VM image.

#[cfg(target_os = "linux")]
use crate::config::RuntimeConfig;
use crate::resources::RuntimeLimits;
#[cfg(target_os = "linux")]
use crate::runtime_service::RuntimeService;
#[cfg(target_os = "linux")]
use crate::workspace_executor::WorkspaceGuestExecutor;
use std::io;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

/// The ZeroBoot V1 guest vsock port (shared with the ZBRT provider contract).
pub const GUEST_PORT: u32 = 5000;

/// Run the ZeroBoot V1 guest service: bind the guest vsock port, and serve
/// every accepted connection with a fresh workspace executor (Linux only).
#[cfg(target_os = "linux")]
pub async fn run(limits: RuntimeLimits) -> io::Result<()> {
    crate::vsock::validate_endpoint(0, GUEST_PORT)?;
    let listener = crate::vsock::bind_guest(GUEST_PORT)?;
    let workspace_root = RuntimeConfig::from_environment().workspace_root;
    loop {
        let stream = crate::vsock::accept(&listener).await?;
        let limits = limits.clone();
        let root = workspace_root.clone();
        tokio::spawn(async move {
            let executor = match WorkspaceGuestExecutor::new(&root, limits.clone()) {
                Ok(executor) => executor,
                Err(_) => return,
            };
            let service = Arc::new(Mutex::new(RuntimeService::with_executor_impl(
                limits, executor,
            )));
            let (reader, writer) = tokio::io::split(stream);
            let _ = crate::zeroboot_connection::serve(reader, writer, service).await;
        });
    }
}

/// Non-Linux variant: ZeroBoot V1 is a Linux/vsock guest and never silently
/// falls back to another transport.
#[cfg(not(target_os = "linux"))]
pub async fn run(_limits: RuntimeLimits) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "zeroboot guest requires Linux vsock support",
    ))
}
