//! RFB1 wire helpers over the vsock UDS relay: CONNECT handshake and framed
//! request/response exchange using the runtime codec.

use crate::cli::error::{external, validation, CliError};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use rfb_runtime::codec::{read_frame_blocking, write_frame_blocking, FrameCodec, MessageType};
use rfb_runtime::host_vsock::VsockEndpoint;
use rfb_runtime::session::{ControlMessage, RuntimeMessage};

/// Connect to the vsock UDS relay and complete the `CONNECT <port>` handshake.
pub fn connect_vsock_uds(uds: &Path, port: u16, timeout: Duration) -> Result<UnixStream, CliError> {
    let mut endpoint = VsockEndpoint::new(uds, 3, port as u32)
        .map_err(|error| validation(format!("invalid vsock endpoint: {error}")))?;
    endpoint
        .capture_identity()
        .map_err(|error| validation(format!("vsock endpoint unavailable: {error}")))?;
    let deadline = Instant::now() + timeout;
    loop {
        let result = UnixStream::connect(uds).and_then(|mut stream| {
            stream.write_all(format!("CONNECT {port}\n").as_bytes())?;
            let mut handshake = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte)?;
                handshake.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if handshake.len() > rfb_runtime::vsock_relay::RELAY_MAX_LINE_BYTES {
                    break;
                }
            }
            // Shared parser: the reply must be `OK <host-port>` (the real
            // Firecracker relay format); bare `OK` is a rejection.
            rfb_runtime::vsock_relay::parse_relay_response(&handshake)
                .map(|_| ())
                .map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        format!("vsock handshake failed: {error}"),
                    )
                })?;
            Ok(stream)
        });
        match result {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(external(format!("vsock CONNECT failed: {error}"))),
        }
    }
}

/// Exchange one RFB1 frame (write request, read response, validate sequence /
/// payload). Uses the runtime codec so the wire format is always in sync.
pub fn exchange(
    stream: &mut UnixStream,
    codec: &FrameCodec,
    sequence: u64,
    message_type: MessageType,
    request: &ControlMessage,
    expected_type: MessageType,
    expected: &RuntimeMessage,
) -> Result<(), CliError> {
    write_frame_blocking(stream, codec, message_type, sequence, request)
        .map_err(|error| external(format!("write frame {sequence}: {error}")))?;
    let (frame, response): (_, RuntimeMessage) = read_frame_blocking(stream, codec)
        .map_err(|error| external(format!("read frame {sequence}: {error}")))?;
    if frame.sequence != sequence {
        return Err(validation(format!(
            "response sequence {} != {sequence}",
            frame.sequence
        )));
    }
    if frame.message_type != expected_type || &response != expected {
        return Err(validation(format!(
            "unexpected response at sequence {sequence}: got {:?} (type {:?}), expected {:?} (type {:?})",
            response, frame.message_type, expected, expected_type
        )));
    }
    Ok(())
}
