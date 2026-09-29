//! Internal ZBRT v1 TCP adapter for the unified facade. Reuses the strict
//! frame codecs from `rfb::protocol` (canonical implementation in
//! `rfb-runtime::zeroboot_protocol`) over a plain tokio `TcpStream`.
//!
//! Session semantics follow `sdk/PROTOCOL.md` §3.4: **every connection starts
//! with a mandatory `Hello`/`HelloAck` handshake** (client `rfb-sdk`, the full
//! `ZBRT_V1_CAPABILITIES` set) — a guest refuses un-helloed frames with
//! `Error(code=1, "protocol handshake required")`. After the handshake: one
//! Execute per connection, 0..n `Output` frames strictly before exactly one
//! terminal `Exit`/`Error`, idempotent `Cancel` → `CancelAck`, `Health` →
//! `HealthAck`, fresh 128-bit request id per request.
//!
//! Connection topology: `exec`/`stream` keep the one-turn-per-connection
//! contract and open a fresh (Hello-ed) TCP connection per turn; `health` and
//! the structured fs RPCs (`Fs`/`FsResult`) share one long-lived *control
//! connection* behind an internal async mutex. A failed exchange on the
//! control connection drops the cached socket, reconnects once, and retries
//! the request once (stale-connection semantics). The control connection is
//! opened lazily and closed when the owning `GuestSandbox` facade (and its
//! clones) is dropped.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use super::error::{transport_timeout, RfbError};
use super::types::{GuestExecResult, StreamEvent, StreamEventKind};
use crate::protocol::{
    read_frame_async, write_frame_async, Cancel, Error as ZbrtErrorFrame, Execute, Exit, Frame, Fs,
    Health, Hello, HelloAck, Kind, Output, ZBRT_V1_CAPABILITIES,
};

/// Client identity advertised in the mandatory connection Hello
/// (`sdk/PROTOCOL.md` §3.4).
const HELLO_CLIENT: &str = "rfb-sdk";

/// ZBRT v1 `Execute` carries `argc` in a single byte.
const MAX_ARGC: usize = u8::MAX as usize;

/// Aggregate cap on one exec turn's captured output (mirrors the NDJSON
/// response cap so a chatty guest cannot grow host memory without bound).
const MAX_EXEC_BYTES: usize = crate::core::MAX_GUEST_PAYLOAD_BYTES;

/// Long-lived ZBRT guest adapter owned by a [`GuestSandbox`](super::GuestSandbox)
/// facade; clones share the same control connection.
#[derive(Clone)]
pub(super) struct ZbrtGuest {
    address: String,
    timeout: Duration,
    /// Reusable control connection for `health`/fs RPCs. `None` = (re)connect
    /// lazily on next use; the socket is closed when the last clone of this
    /// adapter (i.e. of the owning facade) is dropped.
    control: Arc<Mutex<Option<TcpStream>>>,
}

fn io_error(message: &'static str) -> RfbError {
    RfbError::Transport(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn decode_error(message: impl Into<String>) -> RfbError {
    RfbError::decode(message)
}

/// Map a ZBRT `Error` frame payload to a [`RfbError::Remote`].
fn error_frame(payload: &[u8]) -> RfbError {
    match ZbrtErrorFrame::decode(payload) {
        Ok(err) => RfbError::Remote(format!("{} (code {})", err.message, err.code)),
        Err(_) => decode_error("invalid Error payload"),
    }
}

impl ZbrtGuest {
    pub(super) fn new(address: String, timeout: Duration) -> Self {
        Self {
            address,
            timeout,
            control: Arc::new(Mutex::new(None)),
        }
    }

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

    /// Classify a frame codec/I-O failure: codec faults (bad
    /// magic/version/flags, unknown kind, oversize payload, trailing bytes) are
    /// decode failures (UNIFIED_API.md §7); genuine I/O faults (EOF mid-frame,
    /// reset) stay transport failures.
    fn frame_error(error: io::Error) -> RfbError {
        if error.kind() == io::ErrorKind::InvalidData {
            decode_error(format!("zbrt frame rejected: {error}"))
        } else {
            RfbError::Transport(error)
        }
    }

    async fn write_frame<W: AsyncWrite + Unpin>(
        writer: &mut W,
        frame: &Frame,
    ) -> Result<(), RfbError> {
        write_frame_async(writer, frame)
            .await
            .map_err(Self::frame_error)
    }

    async fn read_frame<R: AsyncRead + Unpin>(
        reader: &mut R,
        timeout: Duration,
    ) -> Result<Frame, RfbError> {
        tokio::time::timeout(timeout, read_frame_async(reader))
            .await
            .map_err(|_| transport_timeout("zbrt read timeout"))?
            .map_err(Self::frame_error)
    }

    fn check_id(frame: &Frame, request_id: [u8; 16]) -> Result<(), RfbError> {
        if frame.request_id != request_id {
            return Err(decode_error("zbrt request id mismatch"));
        }
        Ok(())
    }

    /// Mandatory connection handshake (`sdk/PROTOCOL.md` §3.4): send `Hello`
    /// and require a matching `HelloAck` before any business frame. Failure to
    /// establish the session is a transport failure.
    async fn handshake(&self, stream: &mut TcpStream) -> Result<(), RfbError> {
        match self.try_handshake(stream).await {
            Ok(()) => Ok(()),
            // Real I/O faults keep their error kind; protocol-level handshake
            // rejections are reclassified as transport failures.
            Err(RfbError::Transport(err)) => Err(RfbError::Transport(err)),
            Err(err) => Err(RfbError::Transport(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("zbrt hello handshake failed: {err}"),
            ))),
        }
    }

    async fn try_handshake(&self, stream: &mut TcpStream) -> Result<(), RfbError> {
        let request_id = Self::request_id();
        let payload = Hello {
            client: HELLO_CLIENT.to_owned(),
            capabilities: ZBRT_V1_CAPABILITIES
                .iter()
                .map(|cap| (*cap).to_owned())
                .collect(),
        }
        .encode()
        .map_err(|_| io_error("failed to encode Hello payload"))?;
        let hello = Frame {
            kind: Kind::Hello,
            flags: 0,
            request_id,
            payload,
        };
        Self::write_frame(stream, &hello).await?;
        let ack = Self::read_frame(stream, self.timeout).await?;
        Self::check_id(&ack, request_id)?;
        match ack.kind {
            Kind::HelloAck => {
                HelloAck::decode(&ack.payload)
                    .map_err(|_| decode_error("invalid HelloAck payload"))?;
                Ok(())
            }
            Kind::Error => Err(error_frame(&ack.payload)),
            _ => Err(decode_error(format!(
                "expected HelloAck, got {:?} during handshake",
                ack.kind
            ))),
        }
    }

    /// ZBRT v1 `Execute` carries argc in one byte: reject oversize argv
    /// locally (Validation, zero frames — not even a TCP connection) instead
    /// of surfacing the codec failure as a transport error.
    fn validate_argv(argv: &[String]) -> Result<(), RfbError> {
        if argv.len() > MAX_ARGC {
            return Err(RfbError::Validation(format!(
                "argv exceeds the ZBRT limit of {MAX_ARGC} arguments"
            )));
        }
        Ok(())
    }

    /// Write one request and read its reply on `stream` (request id checked).
    async fn exchange_on(
        stream: &mut TcpStream,
        request: &Frame,
        timeout: Duration,
    ) -> Result<Frame, RfbError> {
        Self::write_frame(stream, request).await?;
        let frame = Self::read_frame(stream, timeout).await?;
        Self::check_id(&frame, request.request_id)?;
        Ok(frame)
    }

    /// Run one request/response exchange on the shared control connection,
    /// (re)connecting and Hello-ing as needed. A failed exchange drops the
    /// cached connection and retries the request exactly once on a fresh
    /// connection (stale-connection semantics); a guest `Error` frame is a
    /// definitive answer for the request id and is returned as-is (no retry).
    async fn control_exchange(&self, request: Frame) -> Result<Frame, RfbError> {
        let mut guard = self.control.lock().await;
        // At most two attempts: the cached connection, then exactly one
        // reconnect + retry.
        for attempt in 0..2 {
            if guard.is_none() {
                let mut stream = self.connect().await?;
                self.handshake(&mut stream).await?;
                *guard = Some(stream);
            }
            let stream = guard.as_mut().expect("control connection established");
            match Self::exchange_on(stream, &request, self.timeout).await {
                Ok(frame) => return Ok(frame),
                Err(err) => {
                    // Stale or broken control connection: forget it; the next
                    // iteration reconnects (Hello included) and resends once.
                    *guard = None;
                    if attempt == 1 {
                        return Err(err);
                    }
                }
            }
        }
        unreachable!("the loop returns within two attempts")
    }

    /// One exec turn: `Execute` → `Output`* → `Exit`|`Error`, on a fresh
    /// (Hello-ed) connection.
    pub(super) async fn exec(
        &self,
        argv: Vec<String>,
        cwd: Option<String>,
        stdin: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<GuestExecResult, RfbError> {
        Self::validate_argv(&argv)?;
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
        self.handshake(&mut stream).await?;
        Self::write_frame(&mut stream, &request).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut total = 0usize;
        loop {
            let frame = Self::read_frame(&mut stream, self.timeout).await?;
            Self::check_id(&frame, request_id)?;
            match frame.kind {
                Kind::Output => {
                    let output = Output::decode(&frame.payload)
                        .map_err(|_| decode_error("invalid Output payload"))?;
                    total = total.saturating_add(output.data.len());
                    if total > MAX_EXEC_BYTES {
                        return Err(RfbError::Remote(
                            "guest output exceeded the 16 MiB limit".to_owned(),
                        ));
                    }
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
                    return Ok(GuestExecResult {
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

    /// One structured filesystem RPC on the shared control connection:
    /// `Fs` → `FsResult`|`Error`.
    pub(super) async fn fs(&self, op: u8, path: &str, data: Value) -> Result<Value, RfbError> {
        let request_id = Self::request_id();
        let payload = Fs {
            op,
            path: path.to_owned(),
            data: serde_json::to_vec(&data).map_err(|e| decode_error(e.to_string()))?,
        }
        .encode()
        .map_err(|_| io_error("failed to encode Fs payload"))?;
        let frame = self
            .control_exchange(Frame {
                kind: Kind::Fs,
                flags: 0,
                request_id,
                payload,
            })
            .await?;
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

    /// Health probe on the shared control connection: `Health` → `HealthAck`.
    pub(super) async fn health(&self) -> Result<bool, RfbError> {
        let request_id = Self::request_id();
        let payload = Health {
            healthy: true,
            message: None,
        }
        .encode()
        .map_err(|_| io_error("failed to encode Health payload"))?;
        let frame = self
            .control_exchange(Frame {
                kind: Kind::Health,
                flags: 0,
                request_id,
                payload,
            })
            .await?;
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

    /// Start a stream session: send `Execute` on a fresh (Hello-ed) connection
    /// and keep it open.
    pub(super) async fn start(
        guest: &ZbrtGuest,
        argv: Vec<String>,
        cwd: Option<String>,
    ) -> Result<ZbrtStream, RfbError> {
        Self::validate_argv(&argv)?;
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
        guest.handshake(&mut stream).await?;
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
