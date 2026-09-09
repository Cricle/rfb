//! The vsock listener server: one shared runtime for every connection so
//! sessions survive disconnects and a Cancel from connection B can stop a turn
//! started on connection A.

use rfb_runtime::resources::RuntimeLimits;
use std::io;

#[cfg(target_os = "linux")]
use super::connection;
#[cfg(target_os = "linux")]
use rfb_runtime::codec::FrameCodec;
#[cfg(target_os = "linux")]
use rfb_runtime::runtime_service::RuntimeService;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

#[cfg(target_os = "linux")]
pub async fn run(limits: RuntimeLimits) -> io::Result<()> {
    rfb_runtime::vsock::validate_endpoint(0, rfb_runtime::config::DEFAULT_VSOCK_PORT)?;
    let listener = rfb_runtime::vsock::bind_guest(
        rfb_runtime::config::RuntimeConfig::from_environment().vsock_port,
    )?;
    let codec = FrameCodec::from_limits(&limits);
    // One runtime shared by every connection so sessions survive disconnects
    // and a Cancel from connection B can stop a turn started on connection A.
    let shared = Arc::new(Mutex::new(RuntimeService::from_environment_with_limits(
        limits,
    )));
    loop {
        let stream = rfb_runtime::vsock::accept(&listener).await?;
        let shared = shared.clone();
        let codec = codec.clone();
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let _ = connection::serve(reader, writer, codec, shared).await;
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
