//! The vsock listener server: each connection owns its runtime (converged with
//! the ZeroBoot guest), so turns from different connections execute in
//! parallel and a wedged connection can never brick the runtime.

use crate::resources::RuntimeLimits;
use std::io;

#[cfg(target_os = "linux")]
use super::guest_connection as connection;
#[cfg(target_os = "linux")]
use crate::codec::FrameCodec;
#[cfg(target_os = "linux")]
use crate::runtime_service::RuntimeService;
#[cfg(target_os = "linux")]
use crate::vsock::{
    accept_failure_action, AcceptFailure, ACCEPT_FAILURE_LIMIT, ACCEPT_RETRY_BACKOFF,
};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

/// V1 guest dispatcher seams.  The dispatcher deliberately delegates framing,
/// lifecycle, cancellation, and execution to the existing RFB1 runtime pieces.
pub mod dispatcher {
    #[cfg(target_os = "linux")]
    use super::*;

    /// Serve one already-accepted RFB1 guest connection with the caller's
    /// [`RuntimeService`]. This is the testable seam used by vsock listeners
    /// and fake transports; no second protocol or guest implementation exists.
    #[cfg(target_os = "linux")]
    pub async fn serve<R, W>(
        reader: R,
        writer: W,
        codec: FrameCodec,
        shared: Arc<Mutex<RuntimeService>>,
    ) -> io::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin,
    {
        super::connection::serve(reader, writer, codec, shared).await
    }
}

/// Serve the RFB1 framed NDJSON runtime over the guest's vsock listener
/// (pid-1 mode): bind the configured vsock port, then accept and dispatch
/// framed sessions until the listener fails. Transient accept errors back
/// off (see `vsock::accept_failure_action`); config-class errors end run().
///
/// # Errors
///
/// Returns `Err` when the endpoint is invalid, the bind fails, or the accept
/// loop exhausts its failure budget.
/// Serve the RFB1 framed NDJSON runtime over the guest's vsock listener
/// (pid-1 mode): bind the configured vsock port, then accept and dispatch
/// framed sessions until the listener fails. Transient accept errors back
/// off (see `vsock::accept_failure_action`); config-class errors end run().
///
/// # Errors
///
/// Returns `Err` when the endpoint is invalid, the bind fails, or the accept
/// loop exhausts its failure budget.
#[cfg(target_os = "linux")]
pub async fn run(limits: RuntimeLimits) -> io::Result<()> {
    crate::vsock::validate_endpoint(0, crate::config::DEFAULT_VSOCK_PORT)?;
    let listener =
        crate::vsock::bind_guest(crate::config::RuntimeConfig::from_environment().vsock_port)?;
    let codec = FrameCodec::from_limits(&limits);
    // Per-connection runtime, converged with the ZeroBoot guest: each
    // connection owns its RuntimeService + workspace executor, so turns from
    // different connections execute in parallel and one wedged connection
    // cannot brick the runtime for later connections. The workspace size
    // cache stays shared per-root (see `workspace_executor::shared_size_cache`),
    // so `max_workspace_bytes` remains a workspace-wide bound across executors.
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
                        "rfb-guest: vsock accept failed unrecoverably after \
                         {consecutive_failures} consecutive errors: {error}"
                    );
                    return Err(error);
                }
                eprintln!(
                    "rfb-guest: vsock accept failed ({consecutive_failures}/\
                     {ACCEPT_FAILURE_LIMIT}): {error}"
                );
                tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                continue;
            }
        };
        let limits = limits.clone();
        let codec = codec.clone();
        tokio::spawn(async move {
            let service = Arc::new(Mutex::new(RuntimeService::from_environment_with_limits(
                limits,
            )));
            let (reader, writer) = tokio::io::split(stream);
            let _ = dispatcher::serve(reader, writer, codec, service).await;
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn run(_limits: RuntimeLimits) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "vsock is only supported on Linux",
    ))
}
