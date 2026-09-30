//! Linux guest transport for the RFB1 framed protocol.
#![cfg(target_os = "linux")]

use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;
use tokio_vsock::{VsockAddr, VsockListener, VsockStream, VMADDR_CID_ANY};

/// Default guest vsock port the RFB1 runtime listens on.
pub const DEFAULT_PORT: u32 = 5000;

/// Validate the production RFB1 guest endpoint.
///
/// The port is the published wire contract (5000) — EXCEPT under
/// `cfg(debug_assertions)` with `RFB_RUNTIME_DEV_VSOCK_PORT`, whose whole
/// purpose is a dev-machine port override; rejecting it here made the knob
/// self-defeating (set it and the runtime fails to start).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn validate_endpoint(_cid: u32, port: u32) -> io::Result<()> {
    let allowed = port == DEFAULT_PORT
        || (cfg!(debug_assertions)
            && std::env::var("RFB_RUNTIME_DEV_VSOCK_PORT")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .is_some_and(|dev| dev == port));
    if !allowed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("RFB1 vsock port must be {DEFAULT_PORT}"),
        ));
    }
    Ok(())
}

/// Bind a guest vsock listener on a port.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn bind_guest(port: u32) -> io::Result<VsockListener> {
    validate_endpoint(VMADDR_CID_ANY, port)?;
    VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))
}

/// Accept the next guest vsock connection.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn accept(listener: &VsockListener) -> io::Result<VsockStream> {
    listener.accept().await.map(|(stream, _)| stream)
}

/// Connect to a guest vsock endpoint with a bounded wait.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn connect(cid: u32, port: u32, wait: Duration) -> io::Result<VsockStream> {
    validate_endpoint(cid, port)?;
    timeout(wait, VsockStream::connect(VsockAddr::new(cid, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "vsock connect timed out"))?
}

/// Read up to `buf.len()` bytes with a bounded wait.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn read_with_timeout<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
    wait: Duration,
) -> io::Result<usize> {
    timeout(wait, tokio::io::AsyncReadExt::read(reader, buf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "vsock read timed out"))?
}

/// Write all bytes with a bounded wait.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn write_all_with_timeout<W: AsyncWrite + Unpin>(
    writer: &mut W,
    buf: &[u8],
    wait: Duration,
) -> io::Result<()> {
    timeout(wait, tokio::io::AsyncWriteExt::write_all(writer, buf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "vsock write timed out"))?
}

/// Short backoff between vsock accept retries. A transient accept failure
/// (ECONNRESET on a probe that connected and vanished, an interrupted syscall)
/// must not tear down the whole guest service.
pub const ACCEPT_RETRY_BACKOFF: Duration = Duration::from_millis(10);

/// How many consecutive accept failures are treated as unrecoverable and end
/// the guest service. A single error is transient; a persistent stream of
/// failures means the listener is broken in a way retrying cannot fix, and an
/// unbounded retry loop would instead spin (and flood stderr) forever.
#[doc(hidden)]
pub const ACCEPT_FAILURE_LIMIT: u32 = 100;

/// What the accept loop does with one failed accept.
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
