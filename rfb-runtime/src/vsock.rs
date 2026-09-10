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
pub fn validate_endpoint(_cid: u32, port: u32) -> io::Result<()> {
    if port != DEFAULT_PORT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("RFB1 vsock port must be {DEFAULT_PORT}"),
        ));
    }
    Ok(())
}

/// Bind a guest vsock listener on a port.
pub fn bind_guest(port: u32) -> io::Result<VsockListener> {
    validate_endpoint(VMADDR_CID_ANY, port)?;
    VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))
}

/// Accept the next guest vsock connection.
pub async fn accept(listener: &VsockListener) -> io::Result<VsockStream> {
    listener.accept().await.map(|(stream, _)| stream)
}

/// Connect to a guest vsock endpoint with a bounded wait.
pub async fn connect(cid: u32, port: u32, wait: Duration) -> io::Result<VsockStream> {
    validate_endpoint(cid, port)?;
    timeout(wait, VsockStream::connect(VsockAddr::new(cid, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "vsock connect timed out"))?
}

/// Read up to `buf.len()` bytes with a bounded wait.
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
pub async fn write_all_with_timeout<W: AsyncWrite + Unpin>(
    writer: &mut W,
    buf: &[u8],
    wait: Duration,
) -> io::Result<()> {
    timeout(wait, tokio::io::AsyncWriteExt::write_all(writer, buf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "vsock write timed out"))?
}
