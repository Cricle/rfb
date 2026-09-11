// Linux-native ZeroBoot RFB provider over Firecracker virtio-vsock.
//
// Each sandbox owns one Firecracker VM and one reusable ZBRT vsock session.
// The VM is booted once at create time; every exec runs over that negotiated
// session, so a command never triggers a cold boot. Only capabilities the
// ZeroBoot V1 guest actually implements end-to-end are advertised: Health and
// Execute today. Filesystem and Cancel frames are routed through the session
// only after the guest's HelloAck proves support; otherwise they fail closed
// at the sandbox boundary.

use crate::{
    BackendKind, BoxFuture, Capability, ExecResult, ExecSpec, ProviderError, Sandbox, SandboxError,
    SandboxProvider, SandboxSpec, TransportKind,
};
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use crate::guest::{GuestStream, StreamEvent, StreamSpec};
use crate::protocol::{Cancel, Execute, Fs, Health};
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use std::collections::HashMap;
use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use crate::protocol::{write_frame_async, Error as ProtocolError, Exit, Frame, Hello, HelloAck, Kind, Output};
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use crate::vsock::connect_firecracker_uds;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use tokio::io::AsyncReadExt;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use tokio::sync::mpsc;

/// ZBRT V1 capability names understood by the host/provider contract.
///
/// Canonical definition lives in `rfb-runtime::zeroboot_protocol` and is
/// shared with the guest HelloAck, the rootfs marker, and `rfb-cli zeroboot
/// verify`. Only the operations implemented end-to-end by this provider are
/// offered in the Hello negotiation. The sandbox advertises a capability only
/// when the guest's HelloAck actually proves support; unnegotiated operations
/// fail closed at the sandbox boundary instead of being a false positive.
pub use rfb_runtime::zeroboot_protocol::ZBRT_V1_CAPABILITIES;

/// Convert an RFB capability to its stable ZBRT V1 negotiation name.
pub const fn capability_name(capability: Capability) -> Option<&'static str> {
    match capability {
        Capability::Execute => Some("execute"),
        Capability::Health => Some("health"),
        Capability::Stream => Some("stream"),
        Capability::Ls | Capability::Find | Capability::Grep | Capability::ReadFile | Capability::WriteFile => Some("filesystem"),
        Capability::Cancel => Some("cancel"),
        _ => None,
    }
}

/// Build the typed V1 Execute payload from the public sandbox contract.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn execute_request(spec: &ExecSpec, timeout: Duration) -> Result<Execute> {
    spec.validate().map_err(|e| Error::Backend(e.to_string()))?;
    let mut argv = Vec::with_capacity(spec.args.len() + 1);
    argv.push(spec.command.clone());
    argv.extend(spec.args.iter().cloned());
    // The wire deadline field is u32 milliseconds; longer contract timeouts
    // saturate at ~49.7 days rather than wrapping around.
    Ok(Execute { argv, cwd: spec.cwd.clone(), stdin: spec.stdin.clone().unwrap_or_default(), timeout_ms: spec.timeout.unwrap_or(timeout).as_millis().min(u32::MAX as u128) as u32 })
}

/// Build typed V1 filesystem and cancellation payloads for host transports.
pub fn filesystem_request(op: u8, path: impl Into<String>, data: Vec<u8>) -> Fs { Fs { op, path: path.into(), data } }
/// Build a typed V1 cancellation payload.
pub fn cancel_request(reason: Option<String>) -> Cancel { Cancel { reason, target: None } }
/// Build a typed V1 cancellation payload targeting a specific request id.
pub fn cancel_target_request(reason: Option<String>, target: [u8; 16]) -> Cancel {
    Cancel { reason, target: Some(target) }
}
/// Build a typed V1 health request payload.
pub fn health_request() -> Health { Health { healthy: true, message: None } }

/// Default ZeroBoot guest vsock port.
pub const GUEST_PORT: u32 = 5000;

/// Host-defined ZBRT V1 `Fs` opcodes used when routing the typed sandbox
/// filesystem RPCs. The current ZeroBoot V1 guest does not implement Fs, so
/// these encode the host-side routing contract only and stay gated behind the
/// guest advertising `filesystem` in its HelloAck.
pub mod fs_op {
    /// List a guest directory.
    pub const LS: u8 = 1;
    /// Find files by name pattern.
    pub const FIND: u8 = 2;
    /// Grep file contents.
    pub const GREP: u8 = 3;
    /// Read a file.
    pub const READ: u8 = 4;
    /// Write a file.
    pub const WRITE: u8 = 5;
}

#[allow(dead_code)]
fn new_request_id() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

/// Configuration for the ZeroBoot Firecracker provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Optional kernel image path.
    pub kernel: Option<PathBuf>,
    /// Optional root filesystem path.
    pub rootfs: Option<PathBuf>,
    /// Optional Firecracker executable path.
    pub firecracker: Option<PathBuf>,
    /// Guest vsock port.
    pub guest_port: u32,
    /// Execution timeout.
    pub timeout: Duration,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            kernel: None,
            rootfs: None,
            firecracker: None,
            guest_port: GUEST_PORT,
            timeout: Duration::from_secs(30),
        }
    }
}
impl Config {
    #[allow(dead_code)]
    fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.kernel.is_none() {
            return Err("kernel path is required");
        }
        if self.rootfs.is_none() {
            return Err("rootfs path is required");
        }
        if self.firecracker.is_none() {
            return Err("Firecracker path is required");
        }
        if self.guest_port == 0 {
            return Err("guest port must be non-zero");
        }
        if self.timeout.is_zero() {
            return Err("timeout must be non-zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Errors returned by the ZeroBoot provider.
pub enum Error {
    /// A requested capability is not supported by this provider.
    Unsupported(Capability),
    /// The provider configuration is invalid.
    InvalidConfiguration(&'static str),
    /// The backing Firecracker or vsock operation failed.
    Backend(String),
}
impl Error {
    /// Return a stable machine-readable category for this error.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "unsupported",
            Self::InvalidConfiguration(_) => "invalid_configuration",
            Self::Backend(_) => "backend",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(capability) => {
                write!(f, "ZeroBoot unsupported capability: {capability:?}")
            }
            Self::InvalidConfiguration(s) => write!(f, "invalid ZeroBoot configuration: {s}"),
            Self::Backend(s) => write!(f, "ZeroBoot backend error: {s}"),
        }
    }
}
impl std::error::Error for Error {}
/// Result type for ZeroBoot operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A reusable ZBRT V1 vsock session against one ZeroBoot guest.
///
/// A session owns the guest relay connection and the capability set the guest
/// advertised in its HelloAck. The provider opens exactly one session per
/// Firecracker VM at create time and reuses it for every exec/health/fs/cancel
/// exchange, so commands never trigger a cold boot.
///
/// A background reader task demultiplexes inbound frames by `request_id` into
/// per-request channels (the request map), so an `exec`/`stream` and a
/// concurrent `cancel` never consume each other's frames.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
pub struct ZeroBootSession {
    /// Write half of the relay stream, guarded for short write bursts.
    writer: tokio::sync::Mutex<tokio::io::WriteHalf<tokio::net::UnixStream>>,
    /// Per-request response channels, keyed by the 128-bit wire request id.
    inflight: Arc<tokio::sync::Mutex<HashMap<[u8; 16], mpsc::UnboundedSender<Frame>>>>,
    /// Serializes turn-occupying operations (`exec`/`stream`): the ZeroBoot
    /// V1 guest runs a single turn per connection, so concurrent occupants
    /// would fail closed with "a turn is already active". Queuing here turns
    /// that into sequential execution. `cancel`, `health`, and `fs`
    /// intentionally bypass the lock.
    turn_lock: Arc<tokio::sync::Mutex<()>>,
    /// Capabilities the guest advertised in its HelloAck.
    negotiated: Vec<String>,
    /// Background frame reader/demultiplexer.
    _reader: Option<tokio::task::JoinHandle<()>>,
    /// VM work directory kept alive for the session's lifetime.
    _work: Option<tempfile::TempDir>,
    /// Backing Firecracker VM kept alive for the session's lifetime.
    _vm: Option<crate::firecracker::FirecrackerVm>,
}

/// Errors returned while driving a reusable ZeroBoot session.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
#[derive(Debug)]
pub enum SessionError {
    /// Transport-level I/O failure on the vsock relay.
    Io(std::io::Error),
    /// The guest answered with a ZBRT Error frame.
    Remote {
        /// Stable guest error code.
        code: u32,
        /// Guest-provided diagnostic message.
        message: String,
    },
    /// The guest violated the ZBRT wire contract.
    Protocol(String),
    /// The exchange exceeded its I/O deadline.
    Timeout,
}
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "ZeroBoot session I/O error: {error}"),
            Self::Remote { code, message } => write!(f, "ZeroBoot guest error {code}: {message}"),
            Self::Protocol(message) => write!(f, "ZeroBoot protocol error: {message}"),
            Self::Timeout => write!(f, "ZeroBoot session timed out"),
        }
    }
}
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl std::error::Error for SessionError {}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl ZeroBootSession {
    /// Open a session against a Firecracker vsock UDS relay.
    ///
    /// Firecracker publishes the relay socket before the guest binds its vsock
    /// listener, so this retries transient EOF/refusal until `connect_timeout`
    /// elapses, then negotiates capabilities with a Hello exchange.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn open(
        uds_path: impl AsRef<std::path::Path>,
        port: u32,
        connect_timeout: Duration,
    ) -> std::result::Result<Self, SessionError> {
        let deadline = tokio::time::Instant::now() + connect_timeout;
        let stream = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(SessionError::Timeout);
            }
            match connect_firecracker_uds(uds_path.as_ref(), port, remaining).await {
                Ok(stream) => break stream,
                Err(error) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(SessionError::Io(error));
                    }
                    // 10 ms retry granularity: the guest binds its vsock
                    // listener late in boot, and a coarse poll interval used
                    // to waste up to 100 ms per sandbox on cold start.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };
        let mut session = Self::with_stream(stream);
        session.negotiated = session.hello_exchange(SESSION_IO_TIMEOUT).await?.capabilities;
        Ok(session)
    }

    /// Wrap an already-connected relay stream and negotiate capabilities.
    /// Used by the provider after booting a VM and by mock-guest tests.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn from_stream(
        stream: tokio::net::UnixStream,
        io_timeout: Duration,
    ) -> std::result::Result<Self, SessionError> {
        let mut session = Self::with_stream(stream);
        session.negotiated = session.hello_exchange(io_timeout).await?.capabilities;
        Ok(session)
    }

    /// Construct a session over an already-connected stream: split it, start
    /// the demultiplexing reader, and prepare the request map.
    fn with_stream(stream: tokio::net::UnixStream) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        let inflight: Arc<tokio::sync::Mutex<HashMap<[u8; 16], mpsc::UnboundedSender<Frame>>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let reader_task = spawn_reader(reader, inflight.clone());
        Self {
            writer: tokio::sync::Mutex::new(writer),
            inflight,
            turn_lock: Arc::new(tokio::sync::Mutex::new(())),
            negotiated: Vec::new(),
            _reader: Some(reader_task),
            _work: None,
            _vm: None,
        }
    }

    /// Perform the ZBRT Hello exchange and return the guest's HelloAck. The
    /// negotiated capability set is the source of truth for what the sandbox
    /// may route: unsupported operations fail closed on the host.
    async fn hello_exchange(&self, io_timeout: Duration) -> std::result::Result<HelloAck, SessionError> {
        let hello = Hello {
            client: "rfb-host".to_owned(),
            capabilities: ZBRT_V1_CAPABILITIES.iter().map(|s| (*s).to_owned()).collect(),
        };
        let frame = self
            .exchange(
                Kind::Hello,
                hello.encode().map_err(protocol_error)?,
                io_timeout,
                None,
            )
            .await?;
        match frame.kind {
            Kind::HelloAck => HelloAck::decode(&frame.payload).map_err(protocol_error),
            Kind::Error => Err(remote_error(&frame.payload)),
            other => Err(SessionError::Protocol(format!(
                "unexpected Hello response {other:?}"
            ))),
        }
    }

    /// Whether the guest advertised a given ZBRT V1 capability in its HelloAck.
    pub fn supports(&self, capability: &str) -> bool {
        self.negotiated.iter().any(|c| c == capability)
    }

    /// Test-only: OS process id of the backing Firecracker child, when a VM is
    /// attached. Lets tests assert teardown deterministically instead of
    /// counting global firecracker processes.
    #[doc(hidden)]
    pub fn firecracker_pid(&self) -> Option<u32> {
        self._vm.as_ref().map(|vm| vm.id())
    }

    /// The capability set the guest advertised in its HelloAck.
    pub fn negotiated(&self) -> &[String] {
        &self.negotiated
    }

    /// Run one command, consuming Output frames until Exit and returning the
    /// captured streams. A legacy single-frame `Result` payload (used by older
    /// ZeroBoot rootfs images) is accepted for backward compatibility.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn exec(&self, request: Execute) -> std::result::Result<ExecResult, SessionError> {
        // Hold the turn lock for the whole command: a concurrent exec or
        // stream queues here instead of failing against the guest's
        // single-turn contract.
        let _turn = self.turn_lock.lock().await;
        let request_id = new_request_id();
        let payload = request.encode().map_err(protocol_error)?;
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(request.timeout_ms))
            + EXEC_DEADLINE_MARGIN;
        let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
        self.inflight.lock().await.insert(request_id, tx);
        {
            let mut writer = self.writer.lock().await;
            write_frame_async(
                &mut *writer,
                &Frame {
                    kind: Kind::Execute,
                    flags: 0,
                    request_id,
                    payload,
                },
            )
            .await
            .map_err(map_io_error)?;
        }
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = loop {
            let frame = match await_frame(&mut rx, deadline).await {
                Ok(frame) => frame,
                Err(error) => break Err(error),
            };
            if frame.request_id != request_id {
                break Err(SessionError::Protocol("request_id mismatch".into()));
            }
            match frame.kind {
                Kind::Output => {
                    let output = Output::decode(&frame.payload).map_err(protocol_error)?;
                    if output.stream == 0 {
                        stdout.extend_from_slice(&output.data);
                    } else {
                        stderr.extend_from_slice(&output.data);
                    }
                }
                Kind::Exit => {
                    let exit = Exit::decode(&frame.payload).map_err(protocol_error)?;
                    break Ok(ExecResult {
                        status: Some(exit.code),
                        stdout,
                        stderr,
                        timed_out: false,
                    });
                }
                Kind::Result => {
                    let (code, out, err) = parse_legacy_result(&frame.payload)?;
                    break Ok(ExecResult {
                        status: Some(code),
                        stdout: out,
                        stderr: err,
                        timed_out: false,
                    });
                }
                Kind::Error => break Err(remote_error(&frame.payload)),
                other => {
                    break Err(SessionError::Protocol(format!(
                        "unexpected frame while awaiting exit: {other:?}"
                    )))
                }
            }
        };
        self.inflight.lock().await.remove(&request_id);
        result
    }

    /// Round-trip a ZBRT Health request and return the guest's report.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn health(&self) -> std::result::Result<crate::guest::Health, SessionError> {
        let frame = self
            .exchange(
                Kind::Health,
                health_request().encode().map_err(protocol_error)?,
                SESSION_IO_TIMEOUT,
                None,
            )
            .await?;
        match frame.kind {
            Kind::HealthAck => {
                let health = Health::decode(&frame.payload).map_err(protocol_error)?;
                Ok(crate::guest::Health {
                    healthy: health.healthy,
                    latency: None,
                    message: health.message,
                })
            }
            Kind::Error => Err(remote_error(&frame.payload)),
            other => Err(SessionError::Protocol(format!(
                "unexpected health response {other:?}"
            ))),
        }
    }

    /// Request cancellation of an in-flight request. Fails closed unless the
    /// guest advertises cancellation and answers `CancelAck`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn cancel(&self, reason: Option<String>) -> std::result::Result<(), SessionError> {
        let frame = self
            .exchange(
                Kind::Cancel,
                cancel_request(reason).encode().map_err(protocol_error)?,
                SESSION_IO_TIMEOUT,
                None,
            )
            .await?;
        match frame.kind {
            Kind::CancelAck => Ok(()),
            Kind::Error => Err(remote_error(&frame.payload)),
            other => Err(SessionError::Protocol(format!(
                "unexpected cancel response {other:?}"
            ))),
        }
    }

    /// Route one filesystem RPC. `op` is a host-defined ZBRT Fs opcode; the
    /// returned bytes are the guest's `FsResult` payload.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn fs(
        &self,
        op: u8,
        path: &str,
        data: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, SessionError> {
        let frame = self
            .exchange(
                Kind::Fs,
                filesystem_request(op, path, data)
                    .encode()
                    .map_err(protocol_error)?,
                SESSION_IO_TIMEOUT,
                None,
            )
            .await?;
        match frame.kind {
            Kind::FsResult => Ok(frame.payload),
            Kind::Error => Err(remote_error(&frame.payload)),
            other => Err(SessionError::Protocol(format!(
                "unexpected fs response {other:?}"
            ))),
        }
    }

    /// Open an interactive command stream against the guest. The stream emits
    /// `Output` frames as they arrive and terminates on a single `Exit` frame;
    /// `stop` sends a targeted `Cancel` and drains until the terminal frame.
    /// Every frame wait (output, terminal, and `stop` drain) is bounded by a
    /// single absolute deadline derived from the request's timeout, so a stuck
    /// guest can never hang a stream consumer forever.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn stream(self: &Arc<Self>, request: Execute) -> std::result::Result<ZeroBootStream, SessionError> {
        if !self.supports("stream") {
            return Err(SessionError::Protocol(
                "guest does not advertise the stream capability".into(),
            ));
        }
        let request_id = new_request_id();
        let payload = request.encode().map_err(protocol_error)?;
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(request.timeout_ms))
            + EXEC_DEADLINE_MARGIN;
        // The stream occupies the guest's single turn until its terminal
        // frame; the guard is stored in the returned ZeroBootStream and
        // released when the stream terminates (or is dropped).
        let turn_guard = self.turn_lock.clone().lock_owned().await;
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();
        self.inflight.lock().await.insert(request_id, tx);
        {
            let mut writer = self.writer.lock().await;
            write_frame_async(
                &mut *writer,
                &Frame {
                    kind: Kind::Execute,
                    flags: 0,
                    request_id,
                    payload,
                },
            )
            .await
            .map_err(map_io_error)?;
        }
        Ok(ZeroBootStream {
            session: self.clone(),
            request_id,
            rx,
            deadline,
            terminated: false,
            _turn_guard: Some(turn_guard),
        })
    }

    /// Write a `Cancel` frame that explicitly targets a specific request id
    /// via the V1 compatible target encoding inside the `Cancel` payload. The
    /// frame header reuses the target id so the guest's `CancelAck` routes to
    /// that request's own channel; callers that drain the target's stream (the
    /// `stop` path) consume it there. The guest verifies the target matches
    /// its single active request and fails closed otherwise, and acknowledges
    /// cancellations of already-discharged requests idempotently.
    async fn send_cancel_target(
        &self,
        target: [u8; 16],
        reason: Option<String>,
    ) -> std::result::Result<(), SessionError> {
        let payload = cancel_target_request(reason, target)
            .encode()
            .map_err(protocol_error)?;
        let mut writer = self.writer.lock().await;
        write_frame_async(
            &mut *writer,
            &Frame {
                kind: Kind::Cancel,
                flags: 0,
                request_id: target,
                payload,
            },
        )
        .await
        .map_err(map_io_error)
    }

    /// Register a fresh request id and perform one request/response exchange.
    /// `deadline` bounds each frame wait when `Some` (used by exec); otherwise
    /// each wait is bounded by `io_timeout`.
    async fn exchange(
        &self,
        kind: Kind,
        payload: Vec<u8>,
        io_timeout: Duration,
        deadline: Option<tokio::time::Instant>,
    ) -> std::result::Result<Frame, SessionError> {
        let request_id = new_request_id();
        let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
        self.inflight.lock().await.insert(request_id, tx);
        let write_result = {
            let mut writer = self.writer.lock().await;
            write_frame_async(
                &mut *writer,
                &Frame {
                    kind,
                    flags: 0,
                    request_id,
                    payload,
                },
            )
            .await
            .map_err(map_io_error)
        };
        let result = match write_result {
            Err(error) => Err(error),
            Ok(()) => {
                let mut limit = deadline;
                match await_frame_bounded(&mut rx, &mut limit, io_timeout).await {
                    Ok(frame) if frame.request_id != request_id => {
                        Err(SessionError::Protocol("request_id mismatch".into()))
                    }
                    Ok(frame) => Ok(frame),
                    Err(error) => Err(error),
                }
            }
        };
        self.inflight.lock().await.remove(&request_id);
        result
    }
}

/// Background demultiplexer: reads every inbound ZBRT frame and routes it to
/// the channel registered for its `request_id`. Frames for unregistered ids
/// (e.g. racing a completed request) are dropped.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn spawn_reader(
    mut reader: tokio::io::ReadHalf<tokio::net::UnixStream>,
    inflight: Arc<tokio::sync::Mutex<HashMap<[u8; 16], mpsc::UnboundedSender<Frame>>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(frame) = read_frame_unbounded(&mut reader).await {
            let sender = inflight.lock().await.get(&frame.request_id).cloned();
            if let Some(sender) = sender {
                let _ = sender.send(frame);
            } else {
                // Late or unknown frames (e.g. racing a completed request) are
                // dropped by design — but they are also the classic symptom of
                // a routing bug, so leave a trace.
                eprintln!(
                    "rfb zeroboot: dropped frame kind={:?} id={} (no in-flight request)",
                    frame.kind,
                    frame
                        .request_id
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                );
            }
        }
        // The connection is gone: close every pending request channel so all
        // waiters observe `None` and fail instead of hanging.
        let senders: Vec<_> = inflight.lock().await.drain().map(|(_, tx)| tx).collect();
        drop(senders);
    })
}

/// Read one complete ZBRT frame without an idle timeout. The reader task stays
/// alive across quiet periods and long-running commands; a closed connection
/// surfaces as an error that ends the demultiplexer.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn read_frame_unbounded<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::result::Result<Frame, SessionError> {
    use crate::protocol::HEADER_LEN;
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).await.map_err(map_io_error)?;
    if header[..4] != crate::protocol::MAGIC || header[4] != crate::protocol::VERSION {
        return Err(SessionError::Protocol("invalid ZBRT magic or version".into()));
    }
    let length = u32::from_be_bytes(header[24..28].try_into().unwrap()) as usize;
    if length > crate::protocol::MAX_PAYLOAD {
        return Err(SessionError::Protocol("ZBRT payload too large".into()));
    }
    let mut bytes = header.to_vec();
    bytes.resize(HEADER_LEN + length, 0);
    reader
        .read_exact(&mut bytes[HEADER_LEN..])
        .await
        .map_err(map_io_error)?;
    Frame::decode(&mut bytes.as_slice()).map_err(protocol_error)
}

/// Wait for the next frame of a request, bounded by the remaining exec
/// deadline (or the fixed I/O timeout when no deadline is set).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn await_frame_bounded(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    deadline: &mut Option<tokio::time::Instant>,
    io_timeout: Duration,
) -> std::result::Result<Frame, SessionError> {
    let wait = match deadline {
        Some(instant) => {
            let remaining = instant.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(SessionError::Timeout);
            }
            remaining
        }
        None => io_timeout,
    };
    await_frame(rx, tokio::time::Instant::now() + wait).await
}

/// Wait for the next frame of a request before `deadline`.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn await_frame(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    deadline: tokio::time::Instant,
) -> std::result::Result<Frame, SessionError> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(SessionError::Timeout);
    }
    match tokio::time::timeout(remaining, rx.recv()).await {
        Ok(Some(frame)) => Ok(frame),
        Ok(None) => Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "ZeroBoot guest connection closed",
        ))),
        Err(_) => Err(SessionError::Timeout),
    }
}

/// Live ZBRT V1 stream adapter: consumes `Output` frames for one Execute
/// request and emits typed [`StreamEvent`]s, terminating exactly once on
/// `Exit`. `stop` sends a targeted `Cancel` for this stream's request and
/// drains (bounded by the stream's absolute deadline) until the terminal
/// frame.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
pub struct ZeroBootStream {
    session: Arc<ZeroBootSession>,
    request_id: [u8; 16],
    rx: mpsc::UnboundedReceiver<Frame>,
    /// Absolute deadline for every frame wait on this stream (output reads
    /// and the `stop` drain). Derived from the Execute request's timeout.
    deadline: tokio::time::Instant,
    terminated: bool,
    /// Held while this stream occupies the guest's single turn. Released as
    /// soon as the terminal frame discharges the turn (or on drop, which
    /// frees the queue but leaves the guest turn active until the next
    /// request fails closed — the pre-existing abandoned-stream behavior).
    _turn_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl ZeroBootStream {
    fn unregister(&self) {
        let session = self.session.clone();
        let request_id = self.request_id;
        tokio::spawn(async move {
            session.inflight.lock().await.remove(&request_id);
        });
    }

    /// Mark the stream terminated and release its turn so a queued exec or
    /// stream can start.
    fn discharge(&mut self) {
        self.terminated = true;
        self._turn_guard = None;
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl GuestStream for ZeroBootStream {
    fn next_event<'a>(
        &'a mut self,
    ) -> BoxFuture<'a, std::result::Result<Option<StreamEvent>, SandboxError>> {
        Box::pin(async move {
            if self.terminated {
                return Ok(None);
            }
            // Every frame wait is bounded by the stream's absolute deadline,
            // so a guest that never sends its terminal cannot hang this call.
            let frame = match await_frame(&mut self.rx, self.deadline).await {
                Ok(frame) => frame,
                Err(SessionError::Io(_)) => {
                    // The guest connection closed: the session demultiplexer
                    // drains the inflight channels, so a missing frame means
                    // the session is gone. Surface as a clean stream end.
                    self.discharge();
                    self.unregister();
                    return Ok(None);
                }
                Err(error) => {
                    self.discharge();
                    self.unregister();
                    return Err(map_session_error(error));
                }
            };
            if frame.request_id != self.request_id {
                return Err(SandboxError::Transport(
                    "request_id mismatch on stream frame".into(),
                ));
            }
            match frame.kind {
                Kind::Output => {
                    let output = Output::decode(&frame.payload).map_err(|e| {
                        SandboxError::Transport(format!("invalid Output frame: {e}"))
                    })?;
                    let event = if output.stream == 0 {
                        StreamEvent::Stdout { data: output.data }
                    } else {
                        StreamEvent::Stderr { data: output.data }
                    };
                    Ok(Some(event))
                }
                Kind::Exit => {
                    let exit = Exit::decode(&frame.payload).map_err(|e| {
                        SandboxError::Transport(format!("invalid Exit frame: {e}"))
                    })?;
                    self.discharge();
                    self.unregister();
                    Ok(Some(StreamEvent::Exit {
                        code: Some(exit.code),
                    }))
                }
                Kind::Result => {
                    let (code, out, err) = parse_legacy_result(&frame.payload)
                        .map_err(|e| SandboxError::Transport(e.to_string()))?;
                    self.unregister();
                    // Emit the captured streams first (stdout has wire
                    // priority), then the terminal Exit on the next call.
                    if !out.is_empty() {
                        return Ok(Some(StreamEvent::Stdout { data: out }));
                    }
                    if !err.is_empty() {
                        return Ok(Some(StreamEvent::Stderr { data: err }));
                    }
                    self.discharge();
                    Ok(Some(StreamEvent::Exit { code: Some(code) }))
                }
                Kind::Error => {
                    let error = remote_error(&frame.payload);
                    self.discharge();
                    self.unregister();
                    Err(match error {
                        SessionError::Remote { message, .. } => {
                            SandboxError::Execution(message)
                        }
                        other => SandboxError::Transport(other.to_string()),
                    })
                }
                other => Err(SandboxError::Transport(format!(
                    "unexpected stream frame {other:?}"
                ))),
            }
        })
    }

    fn send_input<'a>(&'a mut self, _input: String) -> BoxFuture<'a, std::result::Result<(), SandboxError>> {
        Box::pin(async {
            // The ZBRT V1 wire carries a single static stdin buffer inside
            // Execute; there is no stdin streaming channel to write into.
            Err(SandboxError::Execution(
                "ZeroBoot V1 streams do not support input".into(),
            ))
        })
    }

    fn stop<'a>(&'a mut self) -> BoxFuture<'a, std::result::Result<(), SandboxError>> {
        Box::pin(async move {
            if self.terminated {
                return Ok(());
            }
            // Cancel exactly this stream's request: the encoded target lets
            // the guest verify it is the single active request (or a request
            // whose terminal already discharged, which is acked idempotently).
            // Then drain this stream's own frames until the guest's single
            // terminal frame, bounded by the stream's absolute deadline.
            if let Err(error) = self.session.send_cancel_target(self.request_id, None).await {
                // A write failure to a dead connection is not a stop failure:
                // the drain below observes the closure and terminates cleanly.
                if !matches!(error, SessionError::Io(_)) {
                    return Err(map_session_error(error));
                }
            }
            loop {
                match await_frame(&mut self.rx, self.deadline).await {
                    Ok(frame) => {
                        // Skip Output chunks and the CancelAck (which routes
                        // here because the cancel frame reused this request
                        // id); Exit / Error / Result are the terminal frames.
                        if matches!(frame.kind, Kind::Exit | Kind::Error | Kind::Result) {
                            self.discharge();
                            self.unregister();
                            return Ok(());
                        }
                    }
                    Err(SessionError::Io(_)) => {
                        self.discharge();
                        self.unregister();
                        return Ok(());
                    }
                    Err(error) => {
                        self.discharge();
                        self.unregister();
                        return Err(map_session_error(error));
                    }
                }
            }
        })
    }
}

/// Build the typed V1 Execute payload for an interactive stream request.
/// Pty and environment are not part of the ZBRT V1 wire and fail closed rather
/// than being silently dropped.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn stream_to_execute(spec: &StreamSpec, timeout: Duration) -> std::result::Result<Execute, SandboxError> {
    if spec.pty == Some(true) {
        return Err(SandboxError::Execution(
            "pty is not supported by ZeroBoot V1 streams".into(),
        ));
    }
    if !spec.env.is_empty() {
        return Err(SandboxError::Execution(
            "environment is not supported by ZeroBoot V1 streams".into(),
        ));
    }
    let mut argv = Vec::with_capacity(spec.args.len() + 1);
    argv.push(spec.command.clone());
    argv.extend(spec.args.iter().cloned());
    Ok(Execute {
        argv,
        cwd: spec.cwd.clone(),
        stdin: Vec::new(),
        timeout_ms: spec
            .timeout
            .unwrap_or(timeout)
            .as_millis()
            .min(u32::MAX as u128) as u32,
    })
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_MEM_MIB: u32 = 512;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_MEM_MIB_ENV: &str = "RFB_ZBRT_VM_MEM_MIB";
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn vm_mem_mib() -> u32 {
    // Capacity-evaluation override: lets operators measure the real memory
    // floor of a ZBRT microVM without touching wire behavior or defaults.
    std::env::var(VM_MEM_MIB_ENV)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|mib| *mib > 0)
        .unwrap_or(VM_MEM_MIB)
}
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_INIT_PATH: &str = "/init";
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const GUEST_CID: u32 = 3;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const SESSION_IO_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const SESSION_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const EXEC_DEADLINE_MARGIN: Duration = Duration::from_secs(15);

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn protocol_error<E: fmt::Display>(error: E) -> SessionError {
    SessionError::Protocol(error.to_string())
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn map_io_error(error: std::io::Error) -> SessionError {
    // tokio::time::timeout maps an expired deadline to TimedOut, so a single
    // kind check is enough to classify both our deadline guard and the framed
    // helpers.
    if error.kind() == std::io::ErrorKind::TimedOut {
        SessionError::Timeout
    } else {
        SessionError::Io(error)
    }
}


#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn remote_error(payload: &[u8]) -> SessionError {
    match ProtocolError::decode(payload) {
        Ok(error) => SessionError::Remote {
            code: error.code,
            message: error.message,
        },
        Err(error) => SessionError::Protocol(format!("invalid ZBRT Error payload: {error}")),
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn parse_legacy_result(payload: &[u8]) -> std::result::Result<(i32, Vec<u8>, Vec<u8>), SessionError> {
    if payload.len() < 12 {
        return Err(SessionError::Protocol(
            "legacy Result payload too short".into(),
        ));
    }
    let exit = i32::from_be_bytes(payload[0..4].try_into().unwrap());
    let out = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as usize;
    let err = u32::from_be_bytes(payload[8..12].try_into().unwrap()) as usize;
    if 12usize.checked_add(out).and_then(|n| n.checked_add(err)) != Some(payload.len()) {
        return Err(SessionError::Protocol(
            "legacy Result payload length mismatch".into(),
        ));
    }
    Ok((exit, payload[12..12 + out].to_vec(), payload[12 + out..].to_vec()))
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn map_session_error(error: SessionError) -> SandboxError {
    match error {
        SessionError::Timeout => SandboxError::Timeout,
        SessionError::Remote { message, .. } => SandboxError::Execution(message),
        SessionError::Io(error) => SandboxError::Transport(error.to_string()),
        SessionError::Protocol(message) => SandboxError::Transport(message),
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn check_supported(
    session: &ZeroBootSession,
    capability_name: &str,
    capability: Capability,
) -> std::result::Result<(), SandboxError> {
    if session.supports(capability_name) {
        Ok(())
    } else {
        Err(SandboxError::UnsupportedCapability(capability))
    }
}

/// Copy the caller-provided rootfs into this sandbox's private work dir and
/// return the staged path.
///
/// The Firecracker drive is mounted read-write inside the guest, so handing
/// the caller's file straight to Firecracker would leak guest writes into the
/// shared source image — and into every other sandbox reusing it. Each
/// sandbox boots from its own copy, which dies with the sandbox work dir.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn stage_private_rootfs(work_path: &str, source: &str) -> Result<String> {
    let staged = std::path::Path::new(work_path).join("rootfs.ext4");
    std::fs::copy(source, &staged)
        .map_err(|e| Error::Backend(format!("staging private rootfs failed: {e}")))?;
    staged
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Backend("private rootfs path is not valid UTF-8".into()))
}

/// Boot one Firecracker VM for a sandbox and return a reusable negotiated
/// session bound to it. The VM keeps running for the session's lifetime, so
/// every later exec/control RPC runs without a cold boot.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn boot_and_open(config: &Config) -> Result<ZeroBootSession> {
    let firecracker = config
        .firecracker
        .as_ref()
        .and_then(|path| path.to_str())
        .ok_or(Error::InvalidConfiguration("Firecracker path is required"))?;
    let kernel = config
        .kernel
        .as_ref()
        .and_then(|path| path.to_str())
        .ok_or(Error::InvalidConfiguration("kernel path is required"))?;
    let rootfs = config
        .rootfs
        .as_ref()
        .and_then(|path| path.to_str())
        .ok_or(Error::InvalidConfiguration("rootfs path is required"))?;
    let work = tempfile::Builder::new()
        .prefix("rfb-zeroboot-")
        .tempdir()
        .map_err(|e| Error::Backend(e.to_string()))?;
    let work_path = work
        .path()
        .to_str()
        .ok_or_else(|| Error::Backend("invalid work path".into()))?
        .to_owned();
    let (vm, uds) = tokio::task::spawn_blocking({
        let firecracker = firecracker.to_owned();
        let kernel = kernel.to_owned();
        let rootfs = rootfs.to_owned();
        move || {
            let rootfs = stage_private_rootfs(&work_path, &rootfs)?;
            let vm = crate::firecracker::FirecrackerVm::boot_with_runtime(
                &firecracker,
                &kernel,
                &rootfs,
                &work_path,
                vm_mem_mib(),
                VM_INIT_PATH,
                GUEST_CID,
            )
            .map_err(|e| Error::Backend(e.to_string()))?;
            let uds = vm
                .vsock_uds_path()
                .ok_or_else(|| Error::Backend("missing vsock UDS".into()))?
                .to_owned();
            Ok::<_, Error>((vm, uds))
        }
    })
    .await
    .map_err(|e| Error::Backend(format!("ZeroBoot boot worker failed: {e}")))??;
    let mut session = ZeroBootSession::open(&uds, config.guest_port, SESSION_CONNECT_TIMEOUT)
        .await
        .map_err(|e| Error::Backend(format!("ZeroBoot session open failed: {e}")))?;
    session._work = Some(work);
    session._vm = Some(vm);
    Ok(session)
}

#[derive(Debug, Clone)]
/// ZeroBoot sandbox provider.
pub struct ZeroBootProvider {
    config: Arc<Config>,
}
impl Default for ZeroBootProvider {
    fn default() -> Self {
        Self::new(Config::default())
    }
}
impl ZeroBootProvider {
    /// Construct a ZeroBoot provider from the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
    /// Return the provider configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }
    fn unavailable(&self) -> Result<()> {
        #[cfg(all(feature = "zeroboot", target_os = "linux"))]
        {
            self.config.validate().map_err(Error::InvalidConfiguration)
        }
        #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
        {
            Err(Error::Unsupported(Capability::Execute))
        }
    }
}
impl SandboxProvider for ZeroBootProvider {
    fn backend(&self) -> BackendKind {
        BackendKind::VirtualMachine
    }
    fn transport(&self) -> TransportKind {
        TransportKind::Vsock
    }
    fn capabilities(&self) -> &[Capability] {
        // The filesystem ops are part of the ZBRT V1 contract and verified
        // end-to-end; per-sandbox availability still depends on the guest's
        // HelloAck (see `ZeroBootSandbox::capabilities`).
        &[
            Capability::Execute,
            Capability::Health,
            Capability::Stream,
            Capability::Cancel,
            Capability::Ls,
            Capability::Find,
            Capability::Grep,
            Capability::ReadFile,
            Capability::WriteFile,
        ]
    }
    fn create<'a>(
        &'a self,
        spec: SandboxSpec,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn Sandbox>, ProviderError>> {
        Box::pin(async move {
            Ok(Box::new(self.create_zero_boot(spec).await?) as Box<dyn Sandbox>)
        })
    }
}

impl ZeroBootProvider {
    /// Create a sandbox and return the concrete ZeroBoot type. Diagnostic and
    /// test surface (e.g. asserting Firecracker teardown via
    /// [`ZeroBootSandbox::firecracker_pid`]); the [`SandboxProvider::create`]
    /// trait method delegates here and boxes the result.
    pub fn create_zero_boot<'a>(
        &'a self,
        spec: SandboxSpec,
    ) -> BoxFuture<'a, std::result::Result<ZeroBootSandbox, ProviderError>> {
        Box::pin(async move {
            spec.validate().map_err(ProviderError::InvalidSpec)?;
            let supported = self.capabilities();
            if let Some(capability) = spec
                .capabilities
                .iter()
                .copied()
                .find(|c| !supported.contains(c))
            {
                return Err(ProviderError::UnsupportedCapability(capability));
            }
            match self.unavailable() {
                Ok(()) => {}
                Err(Error::Unsupported(capability)) => {
                    return Err(ProviderError::UnsupportedCapability(capability));
                }
                Err(error) => return Err(ProviderError::Unavailable(error.to_string())),
            }
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                // Boot is a one-time cost per sandbox: subsequent exec/health
                // calls reuse this session and never cold-boot the VM.
                let session = boot_and_open(&self.config)
                    .await
                    .map_err(|e| ProviderError::Unavailable(e.to_string()))?;
                let capabilities = capabilities_from_negotiated(session.negotiated());
                Ok(ZeroBootSandbox {
                    config: self.config.clone(),
                    session: Arc::new(session),
                    capabilities,
                })
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(ProviderError::UnsupportedCapability(Capability::Execute))
            }
        })
    }
}

/// Sandbox created by the ZeroBoot provider. It owns one Firecracker VM and a
/// reusable ZBRT vsock session; dropping the sandbox tears down the VM.
pub struct ZeroBootSandbox {
    #[allow(dead_code)]
    config: Arc<Config>,
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    session: Arc<ZeroBootSession>,
    /// Capabilities derived from the guest's HelloAck: only operations the
    /// guest actually supports end-to-end are advertised.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    capabilities: Vec<Capability>,
}
impl ZeroBootSandbox {
    /// Build a sandbox around an already-connected session. Used by the
    /// provider after booting a VM and by mock-guest tests to exercise the
    /// sandbox routing surface without a real Firecracker runtime.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    #[doc(hidden)]
    pub fn from_session_for_test(
        config: Config,
        session: ZeroBootSession,
    ) -> Self {
        let capabilities = capabilities_from_negotiated(&session.negotiated);
        Self {
            config: Arc::new(config),
            session: Arc::new(session),
            capabilities,
        }
    }

    /// Test-only: OS process id of the backing Firecracker child.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    #[doc(hidden)]
    pub fn firecracker_pid(&self) -> Option<u32> {
        self.session.firecracker_pid()
    }
}

/// Map the capability names the guest acked in its HelloAck onto typed
/// [`Capability`]s. The sandbox advertises exactly the negotiated set,
/// mirroring the provider surface: acking `filesystem` enables the full
/// `Ls`/`Find`/`Grep`/`ReadFile`/`WriteFile` group.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn capabilities_from_negotiated(negotiated: &[String]) -> Vec<Capability> {
    let mut capabilities = Vec::new();
    if negotiated.iter().any(|c| c == "execute") {
        capabilities.push(Capability::Execute);
    }
    if negotiated.iter().any(|c| c == "health") {
        capabilities.push(Capability::Health);
    }
    if negotiated.iter().any(|c| c == "stream") {
        capabilities.push(Capability::Stream);
    }
    if negotiated.iter().any(|c| c == "cancel") {
        capabilities.push(Capability::Cancel);
    }
    if negotiated.iter().any(|c| c == "filesystem") {
        capabilities.extend([
            Capability::Ls,
            Capability::Find,
            Capability::Grep,
            Capability::ReadFile,
            Capability::WriteFile,
        ]);
    }
    capabilities
}
impl Sandbox for ZeroBootSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::VirtualMachine
    }
    fn transport(&self) -> TransportKind {
        TransportKind::Vsock
    }
    fn capabilities(&self) -> &[Capability] {
        #[cfg(all(feature = "zeroboot", target_os = "linux"))]
        {
            &self.capabilities
        }
        #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
        {
            &[]
        }
    }
    fn exec<'a>(
        &'a self,
        spec: ExecSpec,
    ) -> BoxFuture<'a, std::result::Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            spec.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "execute", Capability::Execute)?;
                let request = execute_request(&spec, self.config.timeout)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                self.session.exec(request).await.map_err(map_session_error)
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::Execute))
            }
        })
    }
    fn health<'a>(
        &'a self,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::Health, SandboxError>> {
        Box::pin(async move {
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "health", Capability::Health)?;
                self.session.health().await.map_err(map_session_error)
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::Health))
            }
        })
    }
    fn stream<'a>(
        &'a self,
        spec: crate::guest::StreamSpec,
    ) -> BoxFuture<'a, std::result::Result<Box<dyn crate::guest::GuestStream + 'a>, SandboxError>>
    {
        Box::pin(async move {
            spec.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "stream", Capability::Stream)?;
                let request = stream_to_execute(&spec, self.config.timeout)?;
                let session = self.session.clone();
                let stream = session.stream(request).await.map_err(map_session_error)?;
                Ok(Box::new(stream) as Box<dyn crate::guest::GuestStream + 'a>)
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::Stream))
            }
        })
    }
    fn cancel<'a>(
        &'a self,
        request: crate::guest::CancelRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::CancelResult, SandboxError>> {
        Box::pin(async move {
            request.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "cancel", Capability::Cancel)?;
                self.session
                    .cancel(request.id)
                    .await
                    .map_err(map_session_error)?;
                Ok(crate::guest::CancelResult { cancelled: true })
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::Cancel))
            }
        })
    }
    fn ls<'a>(
        &'a self,
        request: crate::guest::LsRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::LsResult, SandboxError>> {
        Box::pin(async move {
            request.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "filesystem", Capability::Ls)?;
                let payload = serde_json::to_vec(&request)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                let data = self
                    .session
                    .fs(fs_op::LS, &request.path, payload)
                    .await
                    .map_err(map_session_error)?;
                serde_json::from_slice(&data).map_err(|e| {
                    SandboxError::Execution(format!("invalid FsResult payload: {e}"))
                })
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::Ls))
            }
        })
    }
    fn find<'a>(
        &'a self,
        request: crate::guest::FindRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::FindResult, SandboxError>> {
        Box::pin(async move {
            request.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "filesystem", Capability::Find)?;
                let payload = serde_json::to_vec(&request)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                let data = self
                    .session
                    .fs(fs_op::FIND, &request.path, payload)
                    .await
                    .map_err(map_session_error)?;
                serde_json::from_slice(&data).map_err(|e| {
                    SandboxError::Execution(format!("invalid FsResult payload: {e}"))
                })
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::Find))
            }
        })
    }
    fn grep<'a>(
        &'a self,
        request: crate::guest::GrepRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::GrepResult, SandboxError>> {
        Box::pin(async move {
            request.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "filesystem", Capability::Grep)?;
                let payload = serde_json::to_vec(&request)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                let data = self
                    .session
                    .fs(fs_op::GREP, &request.path, payload)
                    .await
                    .map_err(map_session_error)?;
                serde_json::from_slice(&data).map_err(|e| {
                    SandboxError::Execution(format!("invalid FsResult payload: {e}"))
                })
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::Grep))
            }
        })
    }
    fn read<'a>(
        &'a self,
        request: crate::guest::ReadRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::ReadResult, SandboxError>> {
        Box::pin(async move {
            request.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "filesystem", Capability::ReadFile)?;
                let payload = serde_json::to_vec(&request)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                let data = self
                    .session
                    .fs(fs_op::READ, &request.path, payload)
                    .await
                    .map_err(map_session_error)?;
                serde_json::from_slice(&data).map_err(|e| {
                    SandboxError::Execution(format!("invalid FsResult payload: {e}"))
                })
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::ReadFile))
            }
        })
    }
    fn write<'a>(
        &'a self,
        request: crate::guest::WriteRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::WriteResult, SandboxError>> {
        Box::pin(async move {
            request.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                check_supported(&self.session, "filesystem", Capability::WriteFile)?;
                let payload = serde_json::to_vec(&request)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                let data = self
                    .session
                    .fs(fs_op::WRITE, &request.path, payload)
                    .await
                    .map_err(map_session_error)?;
                serde_json::from_slice(&data).map_err(|e| {
                    SandboxError::Execution(format!("invalid FsResult payload: {e}"))
                })
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(SandboxError::UnsupportedCapability(Capability::WriteFile))
            }
        })
    }
}
#[cfg(all(test, feature = "zeroboot", target_os = "linux"))]
mod stage_private_rootfs_tests {
    use super::*;

    #[test]
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    fn stages_a_private_copy_inside_the_work_dir() {
        let work = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let source_path = source.path().join("image.ext4");
        std::fs::write(&source_path, b"rootfs-bytes").unwrap();

        let staged = stage_private_rootfs(
            work.path().to_str().unwrap(),
            source_path.to_str().unwrap(),
        )
        .unwrap();

        assert!(staged.starts_with(work.path().to_str().unwrap()));
        assert_eq!(std::fs::read(&staged).unwrap(), b"rootfs-bytes");

        // Mutating the staged copy never reaches the shared source image.
        std::fs::write(&staged, b"mutated").unwrap();
        assert_eq!(std::fs::read(&source_path).unwrap(), b"rootfs-bytes");
    }
}
