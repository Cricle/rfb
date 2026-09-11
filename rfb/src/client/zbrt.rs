//! Internal ZBRT v1 TCP adapter for the unified facade. Reuses the strict
//! frame codecs from `rfb::protocol` (canonical implementation in
//! `rfb-runtime::zeroboot_protocol`) over a plain tokio `TcpStream`.
//!
//! Session semantics follow `sdk/PROTOCOL.md` §3.4: one Execute per
//! connection, 0..n `Output` frames strictly before exactly one terminal
//! `Exit`/`Error`, idempotent `Cancel` → `CancelAck`, `Health` → `HealthAck`,
//! fresh 128-bit request id per request. No wire Hello is sent (it is
//! optional and the guest auto-readies un-helloed connections).

use std::io;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use super::error::{transport_timeout, RfbError};
use super::types::{ExecResult, StreamEvent, StreamEventKind};
use crate::protocol::{
    read_frame_async, write_frame_async, Cancel, Error as ZbrtErrorFrame, Execute, Exit, Frame, Fs,
    Health, Kind, Output,
};

pub(super) struct ZbrtGuest {
    pub(super) address: String,
    pub(super) timeout: Duration,
}

fn io_error(message: &'static str) -> RfbError {
    RfbError::Transport(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn decode_error(message: impl Into<String>) -> RfbError {
    RfbError::Decode(message.into())
}

/// Map a ZBRT `Error` frame payload to a [`RfbError::Remote`].
fn error_frame(payload: &[u8]) -> RfbError {
    match ZbrtErrorFrame::decode(payload) {
        Ok(err) => RfbError::Remote(format!("{} (code {})", err.message, err.code)),
        Err(_) => decode_error("invalid Error payload"),
    }
}

impl ZbrtGuest {
    async fn connect(&self) -> Result<TcpStream, RfbError> {
        tokio::time::timeout(self.timeout, TcpStream::connect(&self.address))
            .await
            .map_err(|_| transport_timeout("zbrt connect timeout"))?
            .map_err(RfbError::Transport)
    }

    /// Fresh 128-bit request id per request (UUID v4 bytes).
    fn request_id() -> [u8; 16] {
        *uuid::Uuid::new_v4().as_bytes()
    }

    async fn write_frame<W: AsyncWrite + Unpin>(
        writer: &mut W,
        frame: &Frame,
    ) -> Result<(), RfbError> {
        write_frame_async(writer, frame)
            .await
            .map_err(RfbError::Transport)
    }

    async fn read_frame<R: AsyncRead + Unpin>(
        reader: &mut R,
        timeout: Duration,
    ) -> Result<Frame, RfbError> {
        tokio::time::timeout(timeout, read_frame_async(reader))
            .await
            .map_err(|_| transport_timeout("zbrt read timeout"))?
            .map_err(RfbError::Transport)
    }

    fn check_id(frame: &Frame, request_id: [u8; 16]) -> Result<(), RfbError> {
        if frame.request_id != request_id {
            return Err(decode_error("zbrt request id mismatch"));
        }
        Ok(())
    }

    /// One exec turn: `Execute` → `Output`* → `Exit`|`Error`.
    pub(super) async fn exec(
        &self,
        argv: Vec<String>,
        cwd: Option<String>,
        stdin: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<ExecResult, RfbError> {
        let request_id = Self::request_id();
        let payload = Execute {
            argv,
            cwd,
            stdin,
            timeout_ms,
        }
        .encode()
        .map_err(|_| io_error("failed to encode Execute payload"))?;
        let request = Frame {
            kind: Kind::Execute,
            flags: 0,
            request_id,
            payload,
        };
        let mut stream = self.connect().await?;
        Self::write_frame(&mut stream, &request).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let frame = Self::read_frame(&mut stream, self.timeout).await?;
            Self::check_id(&frame, request_id)?;
            match frame.kind {
                Kind::Output => {
                    let output = Output::decode(&frame.payload)
                        .map_err(|_| decode_error("invalid Output payload"))?;
                    match output.stream {
                        0 => stdout.extend(output.data),
                        1 => stderr.extend(output.data),
                        other => {
                            return Err(decode_error(format!("unknown output stream {other}")))
                        }
                    }
                }
                Kind::Exit => {
                    let exit = Exit::decode(&frame.payload)
                        .map_err(|_| decode_error("invalid Exit payload"))?;
                    return Ok(ExecResult {
                        exit_code: exit.code,
                        stdout,
                        stderr,
                        timed_out: false,
                    });
                }
                Kind::Error => return Err(error_frame(&frame.payload)),
                _ => {
                    return Err(decode_error(format!(
                        "unexpected ZBRT frame kind {:?} during exec",
                        frame.kind
                    )))
                }
            }
        }
    }

    /// One structured filesystem RPC: `Fs` → `FsResult`|`Error`.
    pub(super) async fn fs(&self, op: u8, path: &str, data: Value) -> Result<Value, RfbError> {
        let request_id = Self::request_id();
        let payload = Fs {
            op,
            path: path.to_owned(),
            data: serde_json::to_vec(&data).map_err(|e| decode_error(e.to_string()))?,
        }
        .encode()
        .map_err(|_| io_error("failed to encode Fs payload"))?;
        let request = Frame {
            kind: Kind::Fs,
            flags: 0,
            request_id,
            payload,
        };
        let mut stream = self.connect().await?;
        Self::write_frame(&mut stream, &request).await?;
        let frame = Self::read_frame(&mut stream, self.timeout).await?;
        Self::check_id(&frame, request_id)?;
        match frame.kind {
            Kind::FsResult => serde_json::from_slice(&frame.payload)
                .map_err(|e| decode_error(format!("invalid FsResult payload: {e}"))),
            Kind::Error => Err(error_frame(&frame.payload)),
            _ => Err(decode_error(format!(
                "unexpected ZBRT frame kind {:?} during fs",
                frame.kind
            ))),
        }
    }

    /// Health probe: `Health` → `HealthAck`.
    pub(super) async fn health(&self) -> Result<bool, RfbError> {
        let request_id = Self::request_id();
        let payload = Health {
            healthy: true,
            message: None,
        }
        .encode()
        .map_err(|_| io_error("failed to encode Health payload"))?;
        let request = Frame {
            kind: Kind::Health,
            flags: 0,
            request_id,
            payload,
        };
        let mut stream = self.connect().await?;
        Self::write_frame(&mut stream, &request).await?;
        let frame = Self::read_frame(&mut stream, self.timeout).await?;
        Self::check_id(&frame, request_id)?;
        match frame.kind {
            Kind::HealthAck => {
                let ack = Health::decode(&frame.payload)
                    .map_err(|_| decode_error("invalid HealthAck payload"))?;
                Ok(ack.healthy)
            }
            Kind::Error => Err(error_frame(&frame.payload)),
            _ => Err(decode_error(format!(
                "unexpected ZBRT frame kind {:?} during health",
                frame.kind
            ))),
        }
    }

    /// Start a stream session: send `Execute` and keep the connection open.
    pub(super) async fn start(
        guest: &ZbrtGuest,
        argv: Vec<String>,
        cwd: Option<String>,
    ) -> Result<ZbrtStream, RfbError> {
        let request_id = Self::request_id();
        let payload = Execute {
            argv,
            cwd,
            stdin: Vec::new(),
            timeout_ms: 0,
        }
        .encode()
        .map_err(|_| io_error("failed to encode Execute payload"))?;
        let request = Frame {
            kind: Kind::Execute,
            flags: 0,
            request_id,
            payload,
        };
        let mut stream = guest.connect().await?;
        Self::write_frame(&mut stream, &request).await?;
        Ok(ZbrtStream {
            stream,
            request_id,
            timeout: guest.timeout,
            started: false,
            terminal: false,
            cancel_sent: false,
        })
    }
}

/// Live ZBRT stream session on one TCP connection.
pub(super) struct ZbrtStream {
    stream: TcpStream,
    request_id: [u8; 16],
    timeout: Duration,
    started: bool,
    terminal: bool,
    cancel_sent: bool,
}

impl ZbrtStream {
    /// Next stream event. The first poll synthesizes the `Started` event (ZBRT
    /// has no started frame); `CancelAck` frames are skipped; peer close is a
    /// clean end.
    pub(super) async fn next_event(&mut self) -> Result<Option<StreamEvent>, RfbError> {
        if self.terminal {
            return Ok(None);
        }
        if !self.started {
            self.started = true;
            return Ok(Some(StreamEvent {
                kind: StreamEventKind::Started,
                data: Vec::new(),
                code: None,
            }));
        }
        loop {
            let frame = match ZbrtGuest::read_frame(&mut self.stream, self.timeout).await {
                Ok(frame) => frame,
                Err(RfbError::Transport(err)) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None)
                }
                Err(err) => return Err(err),
            };
            ZbrtGuest::check_id(&frame, self.request_id)?;
            match frame.kind {
                Kind::Output => {
                    let output = Output::decode(&frame.payload)
                        .map_err(|_| decode_error("invalid Output payload"))?;
                    let kind = match output.stream {
                        0 => StreamEventKind::Stdout,
                        1 => StreamEventKind::Stderr,
                        other => {
                            return Err(decode_error(format!("unknown output stream {other}")))
                        }
                    };
                    return Ok(Some(StreamEvent {
                        kind,
                        data: output.data,
                        code: None,
                    }));
                }
                Kind::Exit => {
                    let exit = Exit::decode(&frame.payload)
                        .map_err(|_| decode_error("invalid Exit payload"))?;
                    self.terminal = true;
                    return Ok(Some(StreamEvent {
                        kind: StreamEventKind::Exit,
                        data: Vec::new(),
                        code: Some(exit.code),
                    }));
                }
                Kind::Error => return Err(error_frame(&frame.payload)),
                Kind::CancelAck => continue,
                _ => {
                    return Err(decode_error(format!(
                        "unexpected ZBRT frame kind {:?} during stream",
                        frame.kind
                    )))
                }
            }
        }
    }

    // ZBRT v1 carries stdin only inside `Execute`; injection is unsupported.
    // (Not `async`: no await on this path — callers still await the Result.)
    pub(super) fn send_input(&self) -> Result<(), RfbError> {
        if self.terminal {
            return Err(RfbError::Remote(
                "guest stream is no longer running".to_owned(),
            ));
        }
        Err(RfbError::Remote(
            "stdin injection is not supported over the ZBRT transport".to_owned(),
        ))
    }

    /// Idempotent stop: send `Cancel` targeting this request; the eventual
    /// `CancelAck` is skipped by [`next_event`](Self::next_event).
    pub(super) async fn stop(&mut self) -> Result<(), RfbError> {
        if self.terminal || self.cancel_sent {
            return Ok(());
        }
        let payload = Cancel {
            reason: Some("stop".to_owned()),
            target: Some(self.request_id),
        }
        .encode()
        .map_err(|_| io_error("failed to encode Cancel payload"))?;
        let frame = Frame {
            kind: Kind::Cancel,
            flags: 0,
            request_id: self.request_id,
            payload,
        };
        ZbrtGuest::write_frame(&mut self.stream, &frame).await?;
        self.cancel_sent = true;
        Ok(())
    }
}
