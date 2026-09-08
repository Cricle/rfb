//! The vsock listener server: one shared runtime for every connection so
//! sessions survive disconnects and a Cancel from connection B can stop a turn
//! started on connection A.

use crate::resources::RuntimeLimits;
use std::io;

#[cfg(target_os = "linux")]
use super::guest_connection as connection;
#[cfg(target_os = "linux")]
use crate::codec::FrameCodec;
#[cfg(target_os = "linux")]
use crate::runtime_service::RuntimeService;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

/// V1 guest dispatcher seams.  The dispatcher deliberately delegates framing,
/// lifecycle, cancellation, and execution to the existing RFB1 runtime pieces.
pub mod dispatcher {
    #[cfg(target_os = "linux")]
    use super::*;

    /// Serve one already-accepted RFB1 guest connection with the shared
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
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        super::connection::serve(reader, writer, codec, shared).await
    }
}

#[cfg(target_os = "linux")]
pub async fn run(limits: RuntimeLimits) -> io::Result<()> {
    crate::vsock::validate_endpoint(0, crate::config::DEFAULT_VSOCK_PORT)?;
    let listener =
        crate::vsock::bind_guest(crate::config::RuntimeConfig::from_environment().vsock_port)?;
    let codec = FrameCodec::from_limits(&limits);
    // One runtime shared by every connection so sessions survive disconnects
    // and a Cancel from connection B can stop a turn started on connection A.
    let shared = Arc::new(Mutex::new(RuntimeService::from_environment_with_limits(
        limits,
    )));
    loop {
        let stream = crate::vsock::accept(&listener).await?;
        let shared = shared.clone();
        let codec = codec.clone();
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let _ = dispatcher::serve(reader, writer, codec, shared).await;
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
