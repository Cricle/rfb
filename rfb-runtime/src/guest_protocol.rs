use crate::codec::{read_frame, write_frame, FrameCodec, MessageType};
#[cfg(target_os = "linux")]
use crate::codec::{read_frame_blocking, write_frame_blocking};
use crate::session::{ControlMessage, RuntimeMessage};
use std::io;
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
use tokio::io::{AsyncRead, AsyncWrite};

/// Write one host-to-guest control message as a length-delimited frame.
pub async fn write_control_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    codec: &FrameCodec,
    message: &ControlMessage,
    sequence: u64,
) -> io::Result<()> {
    let message_type = match message {
        ControlMessage::Hello { .. } => MessageType::Hello,
        ControlMessage::Capabilities { .. } => MessageType::Capabilities,
        ControlMessage::StartTurn(_) => MessageType::StartTurn,
        ControlMessage::Cancel { .. } => MessageType::CancelTurn,
        ControlMessage::ReadWorkspaceFile(_) => MessageType::ReadWorkspaceFile,
        ControlMessage::ReadHostFile(_) => MessageType::ReadHostFile,
        ControlMessage::WriteWorkspaceFile(_) => MessageType::WriteWorkspaceFile,
        ControlMessage::Shutdown => MessageType::Shutdown,
    };
    write_frame(writer, codec, message_type, sequence, message).await
}

/// Write one control message over a blocking transport (e.g. vsock or Unix
/// stream). This is the synchronous counterpart of [`write_control_message`].
#[cfg(target_os = "linux")]
pub fn write_control_message_blocking<W: Write>(
    writer: &mut W,
    codec: &FrameCodec,
    message: &ControlMessage,
    sequence: u64,
) -> io::Result<()> {
    let message_type = match message {
        ControlMessage::Hello { .. } => MessageType::Hello,
        ControlMessage::Capabilities { .. } => MessageType::Capabilities,
        ControlMessage::StartTurn(_) => MessageType::StartTurn,
        ControlMessage::Cancel { .. } => MessageType::CancelTurn,
        ControlMessage::ReadWorkspaceFile(_) => MessageType::ReadWorkspaceFile,
        ControlMessage::ReadHostFile(_) => MessageType::ReadHostFile,
        ControlMessage::WriteWorkspaceFile(_) => MessageType::WriteWorkspaceFile,
        ControlMessage::Shutdown => MessageType::Shutdown,
    };
    write_frame_blocking(writer, codec, message_type, sequence, message)
}

/// Read one runtime message over a blocking transport (e.g. vsock or Unix
/// stream). This is the synchronous counterpart of [`read_runtime_message`].
#[cfg(target_os = "linux")]
pub fn read_runtime_message_blocking_with_sequence<R: Read>(
    reader: &mut R,
    codec: &FrameCodec,
) -> io::Result<(u64, RuntimeMessage)> {
    let (header, message): (_, RuntimeMessage) = read_frame_blocking(reader, codec)?;
    match header.message_type {
        MessageType::HelloAck
        | MessageType::Capabilities
        | MessageType::Event
        | MessageType::Shutdown
        | MessageType::Error
        | MessageType::FileContent
        | MessageType::WriteAck => Ok((header.sequence, message)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected guest response message type",
        )),
    }
}

/// Read one guest-to-host runtime message over a blocking transport (Linux
/// vsock / Unix stream) and return just the message.
#[cfg(target_os = "linux")]
pub fn read_runtime_message_blocking<R: Read>(
    reader: &mut R,
    codec: &FrameCodec,
) -> io::Result<RuntimeMessage> {
    Ok(read_runtime_message_blocking_with_sequence(reader, codec)?.1)
}

/// Read and validate one guest-to-host runtime message frame.
pub async fn read_runtime_message_with_sequence<R: AsyncRead + Unpin>(
    reader: &mut R,
    codec: &FrameCodec,
) -> io::Result<(u64, RuntimeMessage)> {
    let (header, message): (_, RuntimeMessage) = read_frame(reader, codec).await?;
    match header.message_type {
        MessageType::HelloAck
        | MessageType::Capabilities
        | MessageType::Event
        | MessageType::Shutdown
        | MessageType::Error
        | MessageType::FileContent
        | MessageType::WriteAck => Ok((header.sequence, message)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected guest response message type",
        )),
    }
}

/// Read one guest-to-host runtime message over an async transport and return
/// just the message (discarding the sequence).
pub async fn read_runtime_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    codec: &FrameCodec,
) -> io::Result<RuntimeMessage> {
    Ok(read_runtime_message_with_sequence(reader, codec).await?.1)
}

/// A decoded runtime message paired with its wire sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencedRuntimeMessage {
    /// Wire sequence number from the frame header.
    pub sequence: u64,
    /// The decoded runtime message.
    pub message: RuntimeMessage,
}

/// Decode failures for length-delimited guest frames.
#[derive(Debug, thiserror::Error)]
pub enum GuestProtocolError {
    /// A frame length prefix or payload is truncated.
    #[error("incomplete guest frame {0}")]
    Incomplete(&'static str),
    /// The frame codec rejected a frame.
    #[error("guest frame decode failed: {0}")]
    Decode(#[from] crate::codec::CodecError),
    /// The peer sent a message type this decoder does not accept.
    #[error("unexpected guest response message type")]
    UnexpectedMessage,
}

/// Decode a buffer of length-delimited guest frames into sequenced messages,
/// rejecting unexpected message types.
pub fn decode_guest_messages_with_sequence(
    codec: &FrameCodec,
    bytes: &[u8],
) -> Result<Vec<SequencedRuntimeMessage>, GuestProtocolError> {
    let mut cursor = 0usize;
    let mut messages = Vec::new();
    while cursor < bytes.len() {
        if bytes.len() - cursor < 4 {
            return Err(GuestProtocolError::Incomplete("length"));
        }
        let len = u32::from_le_bytes(
            bytes[cursor..cursor + 4]
                .try_into()
                .map_err(|_| GuestProtocolError::Incomplete("length"))?,
        ) as usize;
        cursor += 4;
        if bytes.len() - cursor < len {
            return Err(GuestProtocolError::Incomplete("payload"));
        }
        let frame = &bytes[cursor..cursor + len];
        cursor += len;
        let (header, message): (_, RuntimeMessage) = codec.decode(frame)?;
        if header.message_type != MessageType::HelloAck
            && header.message_type != MessageType::Event
            && header.message_type != MessageType::Capabilities
            && header.message_type != MessageType::Shutdown
            && header.message_type != MessageType::Error
            && header.message_type != MessageType::FileContent
            && header.message_type != MessageType::WriteAck
        {
            return Err(GuestProtocolError::UnexpectedMessage);
        }
        messages.push(SequencedRuntimeMessage {
            sequence: header.sequence,
            message,
        });
    }
    Ok(messages)
}

/// Decode a buffer of guest frames, discarding sequence numbers.
pub fn decode_guest_messages(
    codec: &FrameCodec,
    bytes: &[u8],
) -> Result<Vec<RuntimeMessage>, GuestProtocolError> {
    Ok(decode_guest_messages_with_sequence(codec, bytes)?
        .into_iter()
        .map(|item| item.message)
        .collect())
}
