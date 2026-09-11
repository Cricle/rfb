//! Firecracker virtio-vsock UDS relay handshake — the single implementation.
//!
//! Firecracker exposes guest virtio-vsock through a host Unix socket that is
//! NOT a transparent byte stream: the client writes `CONNECT <guest-port>\n`
//! and the relay answers `OK <host-port>\n` (or `ERR <code>\n`). After the
//! handshake the connection carries guest AF_VSOCK stream bytes.
//!
//! Every driver (zeroboot provider, RFB1 CLI, `host_vsock`) must parse the
//! reply through `parse_relay_response` so the acceptance rules cannot drift.
//! Async callers may use `perform_relay_handshake`, which applies a single
//! absolute deadline to every I/O step (a peer that drips bytes slowly cannot
//! stretch the handshake past the deadline).

use std::time::Instant;

/// Maximum accepted handshake response line, including the trailing `\n`.
pub const RELAY_MAX_LINE_BYTES: usize = 256;

/// Handshake failure modes.
#[derive(Debug)]
pub enum RelayHandshakeError {
    /// The single absolute deadline elapsed.
    Timeout,
    /// The relay closed the connection before completing the reply.
    Eof,
    /// The reply exceeded [`RELAY_MAX_LINE_BYTES`] without a newline.
    TooLong,
    /// The reply was not valid UTF-8.
    NotUtf8,
    /// The relay refused or malformed the reply; carries the trimmed line.
    Rejected(String),
}

impl std::fmt::Display for RelayHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "relay handshake timed out"),
            Self::Eof => write!(
                f,
                "relay closed the connection before the response completed"
            ),
            Self::TooLong => write!(f, "relay handshake response too long"),
            Self::NotUtf8 => write!(f, "relay handshake response is not UTF-8"),
            Self::Rejected(line) => write!(f, "relay handshake failed: {line}"),
        }
    }
}

/// Validate one relay response line and return the assigned host port.
///
/// The line is the raw bytes up to and including the `\n`. It must be UTF-8,
/// start with `OK `, and carry the relay-assigned host port as a decimal
/// number — the format real Firecracker emits. Anything else (including a
/// bare `OK\n`) is a rejection; older handshakes that accepted it were never
/// verified against a real Firecracker relay.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn parse_relay_response(line: &[u8]) -> Result<u32, RelayHandshakeError> {
    if line.len() > RELAY_MAX_LINE_BYTES {
        return Err(RelayHandshakeError::TooLong);
    }
    let text = std::str::from_utf8(line).map_err(|_| RelayHandshakeError::NotUtf8)?;
    let trimmed = text.trim_end_matches('\n');
    let port = trimmed
        .strip_prefix("OK ")
        .and_then(|rest| rest.trim().parse::<u32>().ok())
        .ok_or_else(|| RelayHandshakeError::Rejected(trimmed.to_owned()))?;
    Ok(port)
}

/// Write `CONNECT <guest_port>\n`, read the reply through
/// [`parse_relay_response`], and return the relay-assigned host port.
///
/// Every I/O step is bounded by the same absolute `deadline`, so a slow-drip
/// peer cannot extend the handshake indefinitely.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn perform_relay_handshake<S>(
    stream: &mut S,
    guest_port: u32,
    deadline: Instant,
) -> Result<u32, RelayHandshakeError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Bound one I/O future by the remaining time on the absolute deadline.
    async fn bounded<R>(
        deadline: Instant,
        fut: impl std::future::Future<Output = std::io::Result<R>>,
    ) -> Result<R, RelayHandshakeError> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, fut).await {
            Ok(result) => result.map_err(|_| RelayHandshakeError::Timeout),
            Err(_) => Err(RelayHandshakeError::Timeout),
        }
    }

    let command = format!("CONNECT {guest_port}\n");
    bounded(deadline, stream.write_all(command.as_bytes())).await?;
    bounded(deadline, stream.flush()).await?;

    let mut line: Vec<u8> = Vec::with_capacity(16);
    loop {
        let mut byte = [0u8; 1];
        let n = bounded(deadline, stream.read(&mut byte)).await?;
        if n == 0 {
            return Err(RelayHandshakeError::Eof);
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if line.len() > RELAY_MAX_LINE_BYTES {
            return Err(RelayHandshakeError::TooLong);
        }
    }
    parse_relay_response(&line)
}
