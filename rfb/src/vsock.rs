//! Tokio clients for Linux AF_VSOCK and Firecracker's UDS backend.
//!
//! Firecracker exposes virtio-vsock through a host Unix socket. That socket is
//! not a transparent byte stream: the client must first send `CONNECT <port>\n`
//! and consume the `OK <host-port>\n` response. After that handshake, the
//! connection carries the guest AF_VSOCK stream bytes.

use std::io;
use std::time::Duration;

use crate::protocol::{Frame, Kind};

#[cfg(any(target_os = "linux", target_os = "android"))]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(any(target_os = "linux", target_os = "android"))]
use tokio::net::UnixStream;
#[cfg(any(target_os = "linux", target_os = "android"))]
use tokio::time::timeout;

/// Connect to a Firecracker vsock UDS backend and complete its text handshake.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub async fn connect_firecracker_uds(
    uds_path: impl AsRef<std::path::Path>,
    guest_port: u32,
    handshake_timeout: Duration,
) -> io::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + handshake_timeout;
    let remaining = || deadline.saturating_duration_since(tokio::time::Instant::now());
    let mut stream = timeout(remaining(), UnixStream::connect(uds_path)).await??;
    let host_port =
        rfb_runtime::vsock_relay::perform_relay_handshake(&mut stream, guest_port, deadline.into())
            .await
            .map_err(|error| match error {
                rfb_runtime::vsock_relay::RelayHandshakeError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, error.to_string())
                }
                rfb_runtime::vsock_relay::RelayHandshakeError::Eof => io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Firecracker vsock handshake closed before response",
                ),
                rfb_runtime::vsock_relay::RelayHandshakeError::TooLong => io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Firecracker vsock handshake response too long",
                ),
                rfb_runtime::vsock_relay::RelayHandshakeError::NotUtf8 => io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Firecracker vsock handshake is not UTF-8",
                ),
                rfb_runtime::vsock_relay::RelayHandshakeError::Rejected(line) => io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("Firecracker vsock handshake failed: {line}"),
                ),
            })?;
    let _ = host_port;
    Ok(stream)
}

/// Write one typed ZBRT frame with a bounded I/O timeout.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub async fn write_frame(
    stream: &mut UnixStream,
    request_id: [u8; 16],
    kind: Kind,
    payload: Vec<u8>,
    io_timeout: Duration,
) -> io::Result<()> {
    let request = Frame {
        kind,
        flags: 0,
        request_id,
        payload,
    };
    let mut encoded = Vec::new();
    request.encode(&mut encoded)?;
    timeout(io_timeout, stream.write_all(&encoded)).await??;
    stream.flush().await
}

/// Read one complete typed ZBRT frame with a bounded I/O timeout.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub async fn read_frame(stream: &mut UnixStream, io_timeout: Duration) -> io::Result<Frame> {
    timeout(io_timeout, async {
        let mut header = vec![0u8; crate::protocol::HEADER_LEN];
        stream.read_exact(&mut header).await?;
        let length = u32::from_be_bytes(header[24..28].try_into().unwrap()) as usize;
        if length > crate::protocol::MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "payload too large",
            ));
        }
        let mut bytes = header;
        bytes.resize(crate::protocol::HEADER_LEN + length, 0);
        stream
            .read_exact(&mut bytes[crate::protocol::HEADER_LEN..])
            .await?;
        Frame::decode(&mut bytes.as_slice())
    })
    .await?
}

/// Send a typed ZBRT Execute frame and decode one response frame.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub async fn execute_frame(
    stream: &mut UnixStream,
    request_id: [u8; 16],
    payload: Vec<u8>,
    io_timeout: Duration,
) -> io::Result<Frame> {
    write_frame(stream, request_id, Kind::Execute, payload, io_timeout).await?;
    read_frame(stream, io_timeout).await
}

/// Send one complete request and read exactly the echoed response.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub async fn round_trip<S>(
    stream: &mut S,
    payload: &[u8],
    io_timeout: Duration,
) -> io::Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(io_timeout, stream.write_all(payload)).await??;
    stream.flush().await?;
    let mut response = vec![0u8; payload.len()];
    timeout(io_timeout, stream.read_exact(&mut response)).await??;
    Ok(response)
}

/// Connect directly to a Linux AF_VSOCK endpoint.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub async fn connect_vsock(
    cid: u32,
    port: u32,
    connect_timeout: Duration,
) -> io::Result<tokio_vsock::VsockStream> {
    let addr = tokio_vsock::VsockAddr::new(cid, port);
    timeout(connect_timeout, tokio_vsock::VsockStream::connect(addr)).await?
}
