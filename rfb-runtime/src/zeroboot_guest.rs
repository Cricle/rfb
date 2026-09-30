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

/// Short backoff between vsock accept retries. A transient accept failure
/// (ECONNRESET on a probe that connected and vanished, an interrupted syscall)
/// must not tear down the whole guest service.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const ACCEPT_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);

/// How many consecutive accept failures are treated as unrecoverable and end
/// the guest service. A single error is transient; a persistent stream of
/// failures means the listener is broken in a way retrying cannot fix, and an
/// unbounded retry loop would instead spin (and flood stderr) forever.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[doc(hidden)]
pub const ACCEPT_FAILURE_LIMIT: u32 = 100;

/// What the accept loop does with one failed accept.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[doc(hidden)]
pub enum AcceptFailure {
    /// Transient (probe reset, interrupted syscall, ...): back off and retry.
    Retry,
    /// Unrecoverable: give up and surface the error to the caller.
    GiveUp,
}

/// Classify one failed vsock accept.
///
/// Config-class errors (`InvalidInput`, `Unsupported`) are programming or
/// image-contract mistakes — retrying can never fix them, so they give up
/// immediately. Everything else is treated as transient and retried with a
/// short backoff; only a long consecutive failure streak gives up, because a
/// listener that fails unboundedly would otherwise spin (and flood stderr)
/// forever while the pid-1 guest looks alive but accepts nothing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[doc(hidden)]
pub fn accept_failure_action(error: &io::Error, consecutive_failures: u32) -> AcceptFailure {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
    ) {
        return AcceptFailure::GiveUp;
    }
    if consecutive_failures >= ACCEPT_FAILURE_LIMIT {
        return AcceptFailure::GiveUp;
    }
    AcceptFailure::Retry
}

/// Run the ZeroBoot V1 guest service: bind the guest vsock port, and serve
/// every accepted connection with a fresh workspace executor (Linux only).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
#[cfg(target_os = "linux")]
pub async fn run(limits: RuntimeLimits) -> io::Result<()> {
    crate::vsock::validate_endpoint(0, GUEST_PORT)?;
    let listener = crate::vsock::bind_guest(GUEST_PORT)?;
    let workspace_root = RuntimeConfig::from_environment().workspace_root;
    // A transient accept error must not terminate the pid-1 guest service:
    // there is no supervisor to restart it, so one bad accept would leave the
    // VM permanently unreachable. Retry with a short backoff instead, and only
    // give up on a persistent failure streak (or a non-retryable config error).
    let mut consecutive_failures: u32 = 0;
    loop {
        let stream = match crate::vsock::accept(&listener).await {
            Ok(stream) => {
                consecutive_failures = 0;
                stream
            }
            Err(error) => {
                consecutive_failures += 1;
                if let AcceptFailure::GiveUp = accept_failure_action(&error, consecutive_failures) {
                    eprintln!(
                        "rfb-zeroboot-guest: vsock accept failed unrecoverably after \
                         {consecutive_failures} consecutive errors: {error}"
                    );
                    return Err(error);
                }
                eprintln!(
                    "rfb-zeroboot-guest: vsock accept failed ({consecutive_failures}/\
                     {ACCEPT_FAILURE_LIMIT}): {error}"
                );
                tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                continue;
            }
        };
        let limits = limits.clone();
        let root = workspace_root.clone();
        tokio::spawn(async move {
            let executor = match WorkspaceGuestExecutor::new(&root, limits.clone()) {
                Ok(executor) => executor,
                Err(error) => {
                    // A misconfigured workspace (unwritable root, bad limits)
                    // must never look like a healthy-but-silent guest: report
                    // it, then drop the connection.
                    eprintln!("rfb-zeroboot-guest: executor init failed for {root:?}: {error}");
                    return;
                }
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
