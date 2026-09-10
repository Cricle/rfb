//! Optimized opcode frame encoder/decoder with the runtime limits.
//!
//! ```
//! use rfb_runtime::codec::{FrameCodec, MessageType};
//! let codec = FrameCodec::default();
//! let bytes = codec.encode(MessageType::Event, 4, &"ok").unwrap();
//! let (frame, value): (_, String) = codec.decode(&bytes).unwrap();
//! assert_eq!(frame.sequence, 4);
//! assert_eq!(value, "ok");
//! ```

use crate::resources::RuntimeLimits;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAGIC: &[u8; 4] = b"RFB1";
const HEADER_LEN: usize = 4 + 1 + 1 + 8 + 4;

/// Wire message type for a frame (opcode byte in the header).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Host initiates the RFB1 handshake.
    Hello = 1,
    /// Host advertises session/workspace capabilities.
    Capabilities = 2,
    /// Host starts a turn.
    StartTurn = 3,
    /// Host cancels an in-flight turn.
    CancelTurn = 4,
    /// Guest emits a session event.
    Event = 5,
    /// Host requests a read-only host source file.
    ReadHostFile = 6,
    /// Host requests a guest workspace file.
    ReadWorkspaceFile = 7,
    /// Host writes a guest workspace file.
    WriteWorkspaceFile = 8,
    /// Host requests orderly shutdown.
    Shutdown = 9,
    /// Guest reports a terminal error.
    Error = 10,
    /// Guest acknowledges `Hello`.
    HelloAck = 11,
    /// Guest response to `ReadHostFile` / `ReadWorkspaceFile` carrying the
    /// requested bytes. It is a guest-to-host response, never a host request.
    FileContent = 12,
    /// Guest acknowledgement to `WriteWorkspaceFile`.
    WriteAck = 13,
}

/// A decoded frame header plus its (possibly compressed) payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Wire message type.
    pub message_type: MessageType,
    /// Client-assigned sequence number echoed by the guest.
    pub sequence: u64,
    /// Whether the payload is zstd-compressed.
    pub compressed: bool,
    /// Raw payload bytes (compressed if `compressed`).
    pub payload: Vec<u8>,
}

/// Codec failure modes.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Frame magic is not `RFB1`.
    #[error("invalid frame magic")]
    Magic,
    /// Unknown opcode byte.
    #[error("unknown message type {0}")]
    MessageType(u8),
    /// Frame exceeds the configured payload limit.
    #[error("frame exceeds limit")]
    TooLarge,
    /// Frame is shorter than the header or its declared length.
    #[error("truncated frame")]
    Truncated,
    /// Postcard serialization failed.
    #[error("serialization: {0}")]
    Serialize(String),
    /// zstd compression/decompression failed.
    #[error("compression: {0}")]
    Compression(String),
    /// Header flags contain unsupported bits.
    #[error("invalid frame flags")]
    InvalidFlags,
    /// Payload has extra bytes after the frame.
    #[error("frame has trailing bytes")]
    Trailing,
    /// Decompressed payload exceeds the limit.
    #[error("decompressed payload exceeds limit")]
    DecompressedTooLarge,
}

/// stateful encoder/decoder bound by runtime limits.
#[derive(Debug, Clone)]
pub struct FrameCodec {
    /// Payloads at or above this size are zstd-compressed.
    pub compression_threshold: usize,
    /// Hard cap on a single payload (frame minus header).
    pub max_payload: usize,
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self {
            compression_threshold: 1024,
            max_payload: 16 * 1024 * 1024,
        }
    }
}

impl FrameCodec {
    /// Build a codec whose payload cap derives from the runtime frame limit.
    pub fn from_limits(limits: &RuntimeLimits) -> Self {
        Self {
            compression_threshold: 1024,
            max_payload: limits.max_frame_bytes.saturating_sub(HEADER_LEN),
        }
    }

    /// Encode a value into a complete RFB1 frame (magic, opcode, flags,
    /// sequence, length, payload).
    pub fn encode<T: Serialize>(
        &self,
        message_type: MessageType,
        sequence: u64,
        value: &T,
    ) -> Result<Vec<u8>, CodecError> {
        let raw = postcard::to_allocvec(value).map_err(|e| CodecError::Serialize(e.to_string()))?;
        if raw.len() > self.max_payload {
            return Err(CodecError::TooLarge);
        }
        let compressed = raw.len() >= self.compression_threshold;
        let payload = if compressed {
            zstd::stream::encode_all(raw.as_slice(), 1)
                .map_err(|e| CodecError::Compression(e.to_string()))?
        } else {
            raw
        };
        if payload.len() > self.max_payload {
            return Err(CodecError::TooLarge);
        }
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(MAGIC);
        out.push(message_type as u8);
        out.push(u8::from(compressed));
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Decode a frame from raw bytes, returning the header info and value.
    pub fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<(Frame, T), CodecError> {
        if bytes.len() < HEADER_LEN {
            return Err(CodecError::Truncated);
        }
        if &bytes[..4] != MAGIC {
            return Err(CodecError::Magic);
        }
        let message_type = match bytes[4] {
            1 => MessageType::Hello,
            2 => MessageType::Capabilities,
            3 => MessageType::StartTurn,
            4 => MessageType::CancelTurn,
            5 => MessageType::Event,
            6 => MessageType::ReadHostFile,
            7 => MessageType::ReadWorkspaceFile,
            8 => MessageType::WriteWorkspaceFile,
            9 => MessageType::Shutdown,
            10 => MessageType::Error,
            11 => MessageType::HelloAck,
            12 => MessageType::FileContent,
            13 => MessageType::WriteAck,
            other => return Err(CodecError::MessageType(other)),
        };
        let flags = bytes[5];
        if flags & !1 != 0 {
            return Err(CodecError::InvalidFlags);
        }
        let compressed = flags == 1;
        let sequence =
            u64::from_le_bytes(bytes[6..14].try_into().map_err(|_| CodecError::Truncated)?);
        let len = u32::from_le_bytes(
            bytes[14..18]
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ) as usize;
        if len > self.max_payload || bytes.len() < HEADER_LEN + len {
            return Err(CodecError::Truncated);
        }
        if bytes.len() != HEADER_LEN + len {
            return Err(CodecError::Trailing);
        }
        let encoded = &bytes[HEADER_LEN..HEADER_LEN + len];
        let payload = if compressed {
            let mut decoder = zstd::stream::read::Decoder::new(encoded)
                .map_err(|e| CodecError::Compression(e.to_string()))?;
            let mut payload =
                Vec::with_capacity(self.max_payload.min(encoded.len().saturating_mul(2)));
            let mut limited = (&mut decoder).take((self.max_payload + 1) as u64);
            limited
                .read_to_end(&mut payload)
                .map_err(|e| CodecError::Compression(e.to_string()))?;
            if payload.len() > self.max_payload {
                return Err(CodecError::DecompressedTooLarge);
            }
            payload
        } else {
            encoded.to_vec()
        };
        let value =
            postcard::from_bytes(&payload).map_err(|e| CodecError::Serialize(e.to_string()))?;
        Ok((
            Frame {
                message_type,
                sequence,
                compressed,
                payload,
            },
            value,
        ))
    }
}

/// Write one length-delimited frame on a blocking (e.g. vsock/Unix socket)
/// transport, the non-async sibling of [`write_frame`].
/// ```
/// use rfb_runtime::codec::{read_frame_blocking, write_frame_blocking, FrameCodec, MessageType};
/// let codec = FrameCodec::default();
/// let mut bytes = Vec::new();
/// write_frame_blocking(&mut bytes, &codec, MessageType::Hello, 9, &"ready").unwrap();
/// let (frame, value): (_, String) = read_frame_blocking(&mut bytes.as_slice(), &codec).unwrap();
/// assert_eq!(frame.message_type, MessageType::Hello);
/// assert_eq!(frame.sequence, 9);
/// assert_eq!(value, "ready");
/// ```
pub fn write_frame_blocking<W: Write, T: Serialize>(
    writer: &mut W,
    codec: &FrameCodec,
    message_type: MessageType,
    sequence: u64,
    value: &T,
) -> io::Result<()> {
    let frame = codec
        .encode(message_type, sequence, value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer.write_all(&(frame.len() as u32).to_le_bytes())?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Read one length-delimited frame on a blocking transport, the non-async
/// sibling of [`read_frame`].
pub fn read_frame_blocking<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    codec: &FrameCodec,
) -> io::Result<(Frame, T)> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > HEADER_LEN + codec.max_payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut frame = vec![0; len];
    reader.read_exact(&mut frame)?;
    codec
        .decode(&frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Write one length-delimited frame on an async transport.
pub async fn write_frame<W: tokio::io::AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    codec: &FrameCodec,
    message_type: MessageType,
    sequence: u64,
    value: &T,
) -> io::Result<()> {
    let frame = codec
        .encode(message_type, sequence, value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer.write_u32_le(frame.len() as u32).await?;
    writer.write_all(&frame).await
}

/// Read one length-delimited frame on an async transport.
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
    codec: &FrameCodec,
) -> io::Result<(Frame, T)> {
    let len = reader.read_u32_le().await? as usize;
    if len > HEADER_LEN + codec.max_payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut frame = vec![0; len];
    reader.read_exact(&mut frame).await?;
    codec
        .decode(&frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}
