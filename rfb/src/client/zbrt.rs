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
//! control connection drops the cached socket and retries the request exactly
//! once on a fresh connection — but only when the failure proves the request
//! was never processed (write failure, connection-kind read failure); a read
//! timeout returns the error without resending (the guest may still be
//! executing the request). The control connection is opened lazily and closed
//! when the owning `GuestSandbox` facade (and its clones) is dropped.

use std::collections::VecDeque;
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
    Health, HelloAck, Kind, Output,
};

/// Client identity advertised in the mandatory connection Hello
/// (`sdk/PROTOCOL.md` §3.4).
const HELLO_CLIENT: &str = "rfb-sdk";

/// ZBRT v1 `Execute` carries `argc` in a single byte.
const MAX_ARGC: usize = u8::MAX as usize;

/// Aggregate cap on one exec turn's captured output (mirrors the NDJSON
/// response cap so a chatty guest cannot grow host memory without bound).
const MAX_EXEC_BYTES: usize = crate::core::MAX_GUEST_PAYLOAD_BYTES;

/// ZBRT v1 frame payload cap (`UNIFIED_API.md` §8: payload ≤ 16 MiB).
const MAX_PAYLOAD_BYTES: usize = crate::core::MAX_GUEST_PAYLOAD_BYTES;

/// Frame-count cap on one exec turn (mirrors the NDJSON response-row cap,
/// `MAX_RESPONSE_LINES`): a guest that drips empty `Output` frames forever
/// must fail closed instead of looping on the connection indefinitely.
const MAX_EXEC_FRAMES: usize = 65_536;

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
        let stream = tokio::time::timeout(self.timeout, TcpStream::connect(&self.address))
            .await
            .map_err(|_| transport_timeout("zbrt connect timeout"))?
            .map_err(RfbError::Transport)?;
        // 无 NODELAY 时，背靠背的小帧会被 Nagle 拖住等对端 ACK（并发下变成
        // 每请求 ~5ms 的停顿）——与其它四语言客户端对齐。
        stream.set_nodelay(true).map_err(RfbError::Transport)?;
        Ok(stream)
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
        let hello = crate::protocol::hello_frame(HELLO_CLIENT, request_id)
            .map_err(|_| io_error("failed to encode Hello payload"))?;
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

    /// ZBRT v1 payloads are u32-bounded at 16 MiB (`UNIFIED_API.md` §8):
    /// reject an oversize exec payload locally (Validation, before the TCP
    /// connection is opened) instead of failing at encode time as Transport.
    fn validate_payload(len: usize) -> Result<(), RfbError> {
        if len > MAX_PAYLOAD_BYTES {
            return Err(RfbError::Validation(format!(
                "exec payload exceeds the ZBRT limit of {MAX_PAYLOAD_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// Run one request/response exchange on the shared control connection,
    /// (re)connecting and Hello-ing as needed. Only failures that prove the
    /// request was never processed are retried on a fresh connection
    /// (stale-connection semantics): a **write** failure means the frame never
    /// went out, and a connection-kind read failure (reset/broken pipe/EOF)
    /// means the connection died before a functioning guest could answer. A
    /// **read timeout does NOT retry** (P1-5): the request was delivered and
    /// may still execute on the guest — a retried non-idempotent Fs op (e.g.
    /// an `append` write) would run twice — so a timeout returns the error
    /// after dropping the (now unusable) cached connection. A guest `Error`
    /// frame is a definitive answer for the request id and is returned as-is.
    async fn control_exchange(&self, request: Frame) -> Result<Frame, RfbError> {
        let mut guard = self.control.lock().await;
        // At most two attempts: the cached connection, then exactly one
        // reconnect + retry (only when the failure proves non-delivery).
        for attempt in 0..2 {
            if guard.is_none() {
                let mut stream = self.connect().await?;
                self.handshake(&mut stream).await?;
                *guard = Some(stream);
            }
            let stream = guard.as_mut().expect("control connection established");
            // Stage 1 — write. Any failure here means the request was never
            // delivered, so a retry is always safe.
            if let Err(err) = Self::write_frame(stream, &request).await {
                *guard = None;
                if attempt == 1 {
                    return Err(err);
                }
                continue;
            }
            // Stage 2 — read + id check.
            let frame = match Self::read_frame(stream, self.timeout).await {
                Ok(frame) => match Self::check_id(&frame, request.request_id) {
                    Ok(()) => frame,
                    Err(err) => {
                        // The connection carried a frame this request id
                        // cannot claim (leftover from an earlier failed
                        // exchange): unusable — drop it and surface the error.
                        *guard = None;
                        return Err(err);
                    }
                },
                Err(err) => {
                    // A timed-out read leaves the connection in an unknown
                    // framing state, so it is always dropped; whether the
                    // request is retried depends on the error kind.
                    *guard = None;
                    if attempt == 1 || !Self::retryable_read_failure(&err) {
                        return Err(err);
                    }
                    // Connection died before a functioning guest could
                    // answer: reconnect (next iteration) and resend once.
                    continue;
                }
            };
            return Ok(frame);
        }
        unreachable!("the loop returns within two attempts")
    }

    /// Whether a read failure after a successful write may be retried on a
    /// fresh connection. Only connection-death kinds qualify (the request was
    /// written but the peer went away before answering). A `TimedOut` read is
    /// excluded (P1-5): delivery succeeded and the guest may still be
    /// executing the request.
    fn retryable_read_failure(error: &RfbError) -> bool {
        match error {
            RfbError::Transport(err) => matches!(
                err.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::NotConnected
            ),
            _ => false,
        }
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
        Self::validate_payload(stdin.len())?;
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
        let mut frames = 0usize;
        loop {
            frames += 1;
            if frames > MAX_EXEC_FRAMES {
                return Err(RfbError::Remote(format!(
                    "guest output exceeded {MAX_EXEC_FRAMES} frames"
                )));
            }
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
                Kind::Error => {
                    // Decision (P2): a guest deadline miss surfaces as an
                    // Error frame ("command timed out", code 1) and raises
                    // Remote here, while the NDJSON path answers
                    // `timed_out: true` — ZBRT v1 has no timed-out wire flag,
                    // so mapping the message text would fork the transport
                    // semantics (every other SDK also raises on Error
                    // frames). Documented divergence, not a defect.
                    return Err(error_frame(&frame.payload));
                }
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
            pending: VecDeque::new(),
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
    /// Events buffered while draining the post-Cancel window in
    /// [`stop`](Self::stop); delivered by [`next_event`](Self::next_event)
    /// before any new frame is read (`UNIFIED_API.md` §5: 期间到的 Output 先缓冲).
    pending: VecDeque<StreamEvent>,
}

impl ZbrtStream {
    /// Next stream event. The first poll synthesizes the `Started` event (ZBRT
    /// has no started frame); events buffered during `stop` are delivered
    /// first; `CancelAck` frames are skipped; peer close is a clean end.
    pub(super) async fn next_event(&mut self) -> Result<Option<StreamEvent>, RfbError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
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

    /// Idempotent stop: send `Cancel` targeting this request and **wait for
    /// the empty `CancelAck`** (`UNIFIED_API.md` §5), buffering any `Output`
    /// frames that arrive before the ack for the next
    /// [`next_event`](Self::next_event) call. An `Exit` arriving inside the
    /// drain window is buffered as the terminal event; a guest `Error` frame
    /// raises Remote; peer close ends the stream cleanly.
    pub(super) async fn stop(&mut self) -> Result<(), RfbError> {
        if self.terminal || self.cancel_sent {
            return Ok(());
        }
        self.cancel_sent = true;
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
        loop {
            let frame = match ZbrtGuest::read_frame(&mut self.stream, self.timeout).await {
                Ok(frame) => frame,
                Err(RfbError::Transport(err)) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    self.terminal = true;
                    return Ok(());
                }
                Err(err) => return Err(err),
            };
            ZbrtGuest::check_id(&frame, self.request_id)?;
            match frame.kind {
                Kind::CancelAck => return Ok(()),
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
                    self.pending.push_back(StreamEvent {
                        kind,
                        data: output.data,
                        code: None,
                    });
                }
                Kind::Exit => {
                    let exit = Exit::decode(&frame.payload)
                        .map_err(|_| decode_error("invalid Exit payload"))?;
                    self.terminal = true;
                    self.pending.push_back(StreamEvent {
                        kind: StreamEventKind::Exit,
                        data: Vec::new(),
                        code: Some(exit.code),
                    });
                    return Ok(());
                }
                Kind::Error => {
                    self.terminal = true;
                    return Err(error_frame(&frame.payload));
                }
                _ => {
                    return Err(decode_error(format!(
                        "unexpected ZBRT frame kind {:?} during cancel",
                        frame.kind
                    )))
                }
            }
        }
    }
}
