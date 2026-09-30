// Linux-native ZeroBoot RFB provider over Firecracker virtio-vsock.
//
// Each sandbox owns one Firecracker VM and a pool of reusable ZBRT vsock
// sessions. The VM is booted once at create time; every exec runs over one of
// the negotiated sessions, so a command never triggers a cold boot. Only
// capabilities the ZeroBoot V1 guest actually implements end-to-end are
// advertised: Health and Execute today. Filesystem and Cancel frames are
// routed through the session only after the guest's HelloAck proves support;
// otherwise they fail closed at the sandbox boundary.

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use crate::guest::{GuestStream, StreamEvent, StreamSpec};
use crate::protocol::{Cancel, Execute, Fs, Health};
use crate::{
    BackendKind, BoxFuture, Capability, ExecResult, ExecSpec, ProviderError, Sandbox, SandboxError,
    SandboxProvider, SandboxSpec, TransportKind,
};
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use std::collections::HashMap;
use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use crate::protocol::{
    read_frame_async, write_frame_async, Error as ProtocolError, Exit, Frame, HelloAck, Kind,
    Output, HEADER_LEN,
};
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use crate::vsock::connect_firecracker_uds;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use std::os::unix::io::AsRawFd;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
use std::path::Path;
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
        Capability::Ls
        | Capability::Find
        | Capability::Grep
        | Capability::ReadFile
        | Capability::WriteFile => Some("filesystem"),
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
    Ok(Execute {
        argv,
        cwd: spec.cwd.clone(),
        stdin: spec.stdin.clone().unwrap_or_default(),
        timeout_ms: spec
            .timeout
            .unwrap_or(timeout)
            .as_millis()
            .min(u32::MAX as u128) as u32,
    })
}

/// Build typed V1 filesystem and cancellation payloads for host transports.
pub fn filesystem_request(op: u8, path: impl Into<String>, data: Vec<u8>) -> Fs {
    Fs {
        op,
        path: path.into(),
        data,
    }
}
/// Build a typed V1 cancellation payload.
pub fn cancel_request(reason: Option<String>) -> Cancel {
    Cancel {
        reason,
        target: None,
    }
}
/// Build a typed V1 cancellation payload targeting a specific request id.
pub fn cancel_target_request(reason: Option<String>, target: [u8; 16]) -> Cancel {
    Cancel {
        reason,
        target: Some(target),
    }
}
/// Build a typed V1 health request payload.
pub fn health_request() -> Health {
    Health {
        healthy: true,
        message: None,
    }
}

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

#[cfg_attr(not(all(feature = "zeroboot", target_os = "linux")), allow(dead_code))]
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
    #[cfg_attr(not(all(feature = "zeroboot", target_os = "linux")), allow(dead_code))]
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
    inflight: Arc<tokio::sync::Mutex<HashMap<[u8; 16], mpsc::Sender<Frame>>>>,
    /// Request id of the operation currently holding the guest's single turn
    /// (`exec`/`stream`). `cancel_active` targets exactly this request, so a
    /// cancel can never claim to have stopped a request whose host-side
    /// future is already gone (the old "first inflight entry" heuristic raced
    /// both registrations and abandoned futures).
    active_turn: tokio::sync::Mutex<Option<[u8; 16]>>,
    /// Serializes turn-occupying operations (`exec`/`stream`): the ZeroBoot
    /// V1 guest runs a single turn per connection, so concurrent occupants
    /// would fail closed with "a turn is already active". Queuing here turns
    /// that into sequential execution. `cancel`, `health`, and `fs`
    /// intentionally bypass the lock.
    turn_lock: Arc<tokio::sync::Mutex<()>>,
    /// Capabilities the guest advertised in its HelloAck.
    negotiated: Vec<String>,
    /// Cleared by the reader task when the relay connection ends (EOF, I/O,
    /// or protocol error); never set back to true. Pool routing reads this
    /// before handing a slot out, so a connection that can no longer carry
    /// frames is retired instead of failing every request routed to it.
    alive: Arc<std::sync::atomic::AtomicBool>,
    /// Background frame reader/demultiplexer.
    _reader: Option<tokio::task::JoinHandle<()>>,
    /// VM work directory kept alive for the session's lifetime.
    _work: Option<tempfile::TempDir>,
    /// flock'd `work.lock` inside the work directory: a live process holds
    /// LOCK_EX, so the process-startup scavenger can reclaim work dirs whose
    /// owner died (the kernel releases flock when the holder's fd is gone).
    _work_lock: Option<std::fs::File>,
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
                    // 1 ms retry granularity: the guest binds its vsock
                    // listener late in boot, and a coarse poll interval used
                    // to waste up to 100 ms per sandbox on cold start. Finer
                    // retries cost nothing when the connect succeeds and are
                    // what an extra session on a running VM mostly pays.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        };
        let mut session = Self::with_stream(stream);
        session.negotiated = session
            .hello_exchange(SESSION_IO_TIMEOUT)
            .await?
            .capabilities;
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
        let inflight: Arc<tokio::sync::Mutex<HashMap<[u8; 16], mpsc::Sender<Frame>>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reader_task = spawn_reader(reader, inflight.clone(), Arc::clone(&alive));
        Self {
            writer: tokio::sync::Mutex::new(writer),
            inflight,
            active_turn: tokio::sync::Mutex::new(None),
            turn_lock: Arc::new(tokio::sync::Mutex::new(())),
            negotiated: Vec::new(),
            alive,
            _reader: Some(reader_task),
            _work: None,
            _work_lock: None,
            _vm: None,
        }
    }

    /// Perform the ZBRT Hello exchange and return the guest's HelloAck. The
    /// negotiated capability set is the source of truth for what the sandbox
    /// may route: unsupported operations fail closed on the host.
    async fn hello_exchange(
        &self,
        io_timeout: Duration,
    ) -> std::result::Result<HelloAck, SessionError> {
        let hello = crate::protocol::hello_frame("rfb-host", [0u8; 16]).map_err(protocol_error)?;
        let frame = self
            .exchange(Kind::Hello, hello.payload, io_timeout, None)
            .await?;
        match frame.kind {
            Kind::HelloAck => HelloAck::decode(&frame.payload).map_err(protocol_error),
            Kind::Error => Err(remote_error(&frame.payload)),
            other => Err(SessionError::Protocol(format!(
                "unexpected Hello response {other:?}"
            ))),
        }
    }

    /// Deterministically close the session: shut the write half down (FIN to
    /// the guest), which ends the guest's serve loop and then the local
    /// reader task. Dropping alone does NOT close the socket — the reader
    /// task owns the read half and keeps the fd open.
    pub async fn close(&self) {
        use tokio::io::AsyncWriteExt;
        // A contended try_lock would silently skip the FIN and reintroduce
        // the nondeterministic teardown this method exists to prevent —
        // close() is rare, so blocking briefly is correct.
        let mut writer = self.writer.lock().await;
        let _ = writer.shutdown().await;
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

    /// Test-only: whether the reader task still believes the relay connection
    /// is alive. Lets tests wait for dead-slot detection deterministically
    /// instead of racing the reader's exit.
    #[doc(hidden)]
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The capability set the guest advertised in its HelloAck.
    pub fn negotiated(&self) -> &[String] {
        &self.negotiated
    }

    /// Run one command, consuming Output frames until Exit and returning the
    /// captured streams. A legacy single-frame `Result` payload (used by older
    /// ZeroBoot rootfs images) is accepted for backward compatibility.
    ///
    /// Takes `&Arc<Self>` so the turn/inflight ownership can be moved into an
    /// RAII turn guard: an abandoned future (dropped mid-exchange) cleans
    /// up its own registration instead of leaking it for the session lifetime.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn exec(
        self: &Arc<Self>,
        request: Execute,
    ) -> std::result::Result<ExecResult, SessionError> {
        let payload = request.encode().map_err(protocol_error)?;
        let (turn_guard, mut rx, deadline) = self.begin_turn(payload, request.timeout_ms).await?;
        let request_id = turn_guard.request_id();
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
                    let output = match Output::decode(&frame.payload) {
                        Ok(output) => output,
                        Err(error) => break Err(protocol_error(error)),
                    };
                    match output.stream {
                        0 => stdout.extend_from_slice(&output.data),
                        1 => stderr.extend_from_slice(&output.data),
                        other => {
                            break Err(SessionError::Protocol(format!(
                                "unknown output stream {other}"
                            )))
                        }
                    }
                    if stdout.len().saturating_add(stderr.len()) > MAX_EXEC_OUTPUT_BYTES {
                        break Err(SessionError::Protocol(
                            "guest output exceeded the 16 MiB limit".into(),
                        ));
                    }
                }
                Kind::Exit => {
                    let exit = match Exit::decode(&frame.payload) {
                        Ok(exit) => exit,
                        Err(error) => break Err(protocol_error(error)),
                    };
                    break Ok(ExecResult {
                        status: Some(exit.code),
                        stdout,
                        stderr,
                        timed_out: false,
                    });
                }
                Kind::Result => {
                    let (code, out, err) = match parse_legacy_result(&frame.payload) {
                        Ok(parsed) => parsed,
                        Err(error) => break Err(error),
                    };
                    break Ok(ExecResult {
                        status: Some(code),
                        stdout: out,
                        stderr: err,
                        timed_out: false,
                    });
                }
                Kind::CancelAck => {
                    // A targeted cancel for this request was acknowledged
                    // (its frame reused this request id, so it routes here);
                    // the cancel-induced Exit follows as the terminal frame.
                }
                Kind::Error => break Err(remote_error(&frame.payload)),
                other => {
                    break Err(SessionError::Protocol(format!(
                        "unexpected frame while awaiting exit: {other:?}"
                    )))
                }
            }
        };
        drop(turn_guard);
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

    /// Send a targeted Cancel for the request that currently holds this
    /// session's guest turn, if one exists, and report whether a cancel was
    /// actually sent. The target is the session's published `active_turn` id —
    /// not a heuristic over the inflight map — so an idle session (no active
    /// turn) reports `false` honestly and an abandoned future can no longer be
    /// "cancelled" after its host-side waiter is gone: `TurnGuard` clears the
    /// id when the turn discharges or the future drops.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn cancel_active(
        &self,
        reason: Option<String>,
    ) -> std::result::Result<bool, SessionError> {
        let target = *self.active_turn.lock().await;
        let Some(target) = target else {
            return Ok(false);
        };
        // Fire-and-read-on-target: the CancelAck (and the cancel-induced
        // terminal) are routed by the TARGET request id into the active
        // operation's own channel, which consumes them. Awaiting an ack here
        // would time out — this Cancel's frames are addressed to the target,
        // not to a fresh exchange id.
        self.send_cancel_target(target, reason).await?;
        Ok(true)
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
    pub async fn stream(
        self: &Arc<Self>,
        request: Execute,
    ) -> std::result::Result<ZeroBootStream, SessionError> {
        if !self.supports("stream") {
            return Err(SessionError::Protocol(
                "guest does not advertise the stream capability".into(),
            ));
        }
        let payload = request.encode().map_err(protocol_error)?;
        let (turn_guard, rx, deadline) = self.begin_turn(payload, request.timeout_ms).await?;
        Ok(ZeroBootStream {
            session: self.clone(),
            request_id: turn_guard.request_id(),
            rx,
            deadline,
            terminated: false,
            pending: std::collections::VecDeque::new(),
            guard: Some(turn_guard),
            _slot: None,
            output_bytes: 0,
        })
    }

    /// Shared head of every turn-occupying request (`exec`/`stream`):
    /// allocate a fresh request id, derive the absolute deadline, queue on
    /// the guest's single-turn lock, register the bounded demux channel,
    /// publish the active-turn id, and write the Execute frame.
    ///
    /// Order is load-bearing: the inflight entry is inserted before the
    /// active-turn id is published, so a racing `cancel_active` can never
    /// target a request that has no channel. On a write failure the guard
    /// drops on the way out (`?`), which removes the inflight entry and the
    /// active-turn registration — the request never reached the wire.
    async fn begin_turn(
        self: &Arc<Self>,
        payload: Vec<u8>,
        deadline_ms: u32,
    ) -> std::result::Result<(TurnGuard, mpsc::Receiver<Frame>, tokio::time::Instant), SessionError>
    {
        let request_id = new_request_id();
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(deadline_ms))
            + EXEC_DEADLINE_MARGIN;
        // Hold the turn lock for the whole command: a concurrent exec or
        // stream queues here instead of failing against the guest's
        // single-turn contract. Owned guard so it can live inside TurnGuard.
        let turn = self.turn_lock.clone().lock_owned().await;
        // Bounded demux channel: a stalled (or abandoned) consumer cannot
        // grow host memory without bound; the reader drops overflow instead.
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CHANNEL_CAPACITY);
        self.inflight.lock().await.insert(request_id, tx);
        // Publish the turn id only after the inflight registration, so a
        // racing cancel_active can never target a request that has no channel.
        *self.active_turn.lock().await = Some(request_id);
        let turn_guard = TurnGuard {
            session: Arc::clone(self),
            _inflight: InflightGuard {
                inflight: Arc::clone(&self.inflight),
                request_id,
            },
            _turn: turn,
        };
        self.write_request(Kind::Execute, request_id, payload)
            .await?;
        Ok((turn_guard, rx, deadline))
    }

    /// Lock the writer and push one request frame; the writer lock is held
    /// for exactly the write, never across a wait for the reply.
    async fn write_request(
        &self,
        kind: Kind,
        request_id: [u8; 16],
        payload: Vec<u8>,
    ) -> std::result::Result<(), SessionError> {
        let mut writer = self.writer.lock().await;
        let frame = Frame {
            kind,
            flags: 0,
            request_id,
            payload,
        };
        write_frame_async(&mut *writer, &frame)
            .await
            .map_err(map_io_error)
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
        self.write_request(Kind::Cancel, target, payload).await
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
        let (tx, mut rx) = mpsc::channel::<Frame>(FRAME_CHANNEL_CAPACITY);
        self.inflight.lock().await.insert(request_id, tx);
        // RAII removal: the old manual cleanup at the end of this function
        // never ran when the future itself was dropped mid-exchange, leaking
        // the inflight entry (and every late frame routed to it) for the
        // session's remaining lifetime.
        let _inflight = InflightGuard {
            inflight: Arc::clone(&self.inflight),
            request_id,
        };
        match self.write_request(kind, request_id, payload).await {
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
        }
    }
}

/// Bound on one request's demux channel. Deep enough that a burst of Output
/// frames never blocks the reader under normal pacing, shallow enough that a
/// stalled consumer cannot grow host memory without bound (the reader drops
/// overflow, rate-limited trace, instead of queueing forever).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const FRAME_CHANNEL_CAPACITY: usize = 256;

/// Per-request byte budget for the demux path: the frame-count channel bound
/// alone caps a request's queued bytes only if frames are small, so a
/// firehose guest is capped here instead. Sized above
/// [`MAX_EXEC_OUTPUT_BYTES`] (the consumer's own cap) so a legitimate
/// oversized-output command trips its own limit without ALSO exhausting the
/// byte budget — but that headroom only holds while the consumer keeps up.
/// Under host backpressure (a stalled or slow consumer) the channel fills and
/// frames ARE dropped; the request's terminal frame then fails closed on the
/// lost output (see [`terminal_frame`]) instead of returning a cleanly
/// truncated result.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const DEMUX_REQUEST_BYTE_BUDGET: usize = 32 * 1024 * 1024;

/// How long the reader retries a full demux channel before dropping a
/// terminal frame (see [`deliver_terminal`]).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const TERMINAL_DELIVERY_WINDOW: Duration = Duration::from_secs(10);

/// RAII registration of one in-flight request's demux channel. Dropping it
/// removes the inflight entry: inline when the map lock is uncontended (the
/// common case at termination time), or via a spawned task when the reader
/// task is currently routing a frame — mirroring the old `unregister`
/// fallback so cleanup never blocks a drop.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
struct InflightGuard {
    inflight: Arc<tokio::sync::Mutex<HashMap<[u8; 16], mpsc::Sender<Frame>>>>,
    request_id: [u8; 16],
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.inflight.try_lock() {
            inflight.remove(&self.request_id);
            return;
        }
        // Contended (reader task routing a frame): fall back to a task.
        let inflight = Arc::clone(&self.inflight);
        let request_id = self.request_id;
        tokio::spawn(async move {
            inflight.lock().await.remove(&request_id);
        });
    }
}

/// RAII ownership of the guest's single turn for one request: the turn-lock
/// guard, the request's inflight registration, and the session's published
/// `active_turn` id. Dropping it releases all three in order (active turn
/// cleared first, so a racing `cancel_active` never targets a request whose
/// waiter is gone; then the inflight entry; then the turn lock).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
struct TurnGuard {
    session: Arc<ZeroBootSession>,
    _inflight: InflightGuard,
    _turn: tokio::sync::OwnedMutexGuard<()>,
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl Drop for TurnGuard {
    fn drop(&mut self) {
        let request_id = self._inflight.request_id;
        if let Ok(mut active) = self.session.active_turn.try_lock() {
            // Conditional clear: the spawned-task fallback below can land
            // after a NEW turn already claimed the id slot.
            if *active == Some(request_id) {
                *active = None;
            }
            return;
        }
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            let mut active = session.active_turn.lock().await;
            if *active == Some(request_id) {
                *active = None;
            }
        });
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl TurnGuard {
    /// The wire request id this turn registered (copied out so callers can
    /// match routed frames while the guard itself stays owned elsewhere).
    fn request_id(&self) -> [u8; 16] {
        self._inflight.request_id
    }
}

/// A pool of negotiated ZBRT sessions against one ZeroBoot guest.
///
/// The V1 contract allows one active turn *per connection*, and the guest
/// serves every accepted connection independently (own runtime service, own
/// workspace executor, own worker thread). The pool is therefore what makes a
/// VM's capacity usable by more than one caller at a time: a request only
/// waits when every slot is busy, instead of queueing behind a single
/// connection's turn lock. Sessions are reused across requests, so steady
/// state pays neither a connect nor a Hello handshake per command.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
struct SessionPool {
    sessions: Vec<Arc<ZeroBootSession>>,
    busy: Vec<std::sync::atomic::AtomicBool>,
    /// Slots whose relay connection the reader task reported dead. A
    /// condemned slot is never handed out again; its semaphore permit is
    /// retired, so `free` counts live connections only. The flag is set
    /// exactly once per slot, by the one acquirer that claimed the slot's
    /// busy flag when the death was discovered.
    dead: Vec<std::sync::atomic::AtomicBool>,
    free: Arc<tokio::sync::Semaphore>,
}

/// A claimed pool slot. Dropping it frees the slot and its session, so a
/// cancelled or failed request can never leak capacity.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
struct PooledSession {
    session: Arc<ZeroBootSession>,
    pool: Arc<SessionPool>,
    slot: usize,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl PooledSession {
    fn session(&self) -> &Arc<ZeroBootSession> {
        &self.session
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl Drop for PooledSession {
    fn drop(&mut self) {
        // Release the flag before the permit: a waiter that wins the permit
        // must always find the slot it just freed marked free.
        self.pool.busy[self.slot].store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl SessionPool {
    fn new(sessions: Vec<Arc<ZeroBootSession>>) -> Arc<Self> {
        let count = sessions.len();
        assert!(count > 0, "a session pool needs at least one session");
        Arc::new(Self {
            sessions,
            busy: (0..count)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            dead: (0..count)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            free: Arc::new(tokio::sync::Semaphore::new(count)),
        })
    }

    /// The primary session (slot 0) regardless of liveness: the backing VM
    /// and its work-dir ownership are attached to exactly this session, so
    /// VM-level lookups must always read here, never a survivor slot.
    fn primary(&self) -> &Arc<ZeroBootSession> {
        &self.sessions[0]
    }

    /// Session for the health probe only: the first connection the reader
    /// task has not reported dead. Falls back to the primary when every slot
    /// is dead, so the probe surfaces the connection's real error instead of
    /// a synthetic one. The guest answers Health directly off its runtime
    /// service (before any turn takes the workspace executor out — see
    /// rfb-runtime/src/runtime_service/mod.rs `filesystem_rpc`), so health
    /// interleaves with an active turn on this connection instead of claiming
    /// a pool slot. Filesystem RPCs run on the guest's workspace executor and
    /// therefore fail while a turn is active on their connection — they must
    /// route through [`SessionPool::acquire`] onto a free connection, not
    /// here.
    fn primary_alive(&self) -> &Arc<ZeroBootSession> {
        self.sessions
            .iter()
            .find(|session| session.alive.load(std::sync::atomic::Ordering::Acquire))
            .unwrap_or(&self.sessions[0])
    }

    /// Every session, for operations that must reach whichever one holds the
    /// target (cancel is idempotent per connection).
    fn all(&self) -> &[Arc<ZeroBootSession>] {
        &self.sessions
    }

    /// Wait for a free live slot and claim it.
    ///
    /// A slot whose reader task reported the connection dead is never handed
    /// out: the V1 pool holds no rebuild parameters (the relay UDS lives in
    /// the session-creation path), so a dead slot is condemned instead — its
    /// permit is retired from the semaphore, shrinking the pool's usable
    /// capacity by one, and the scan continues on the remaining live slots.
    /// When every slot is dead the semaphore is closed, which wakes every
    /// blocked waiter into the same error; future callers fail here too,
    /// rather than as a write failure or a timeout hang on a corpse
    /// connection.
    ///
    /// Invariant: an available permit always stands for a free live slot.
    /// A dead slot's permit is retired (`forget`) the moment the death is
    /// discovered under the claimer's own busy CAS, and a slot is condemned
    /// exactly once, so retiring and freeing can never double-count.
    async fn acquire(self: &Arc<Self>) -> std::result::Result<PooledSession, ProviderError> {
        loop {
            let permit = match self.free.clone().acquire_owned().await {
                Ok(permit) => permit,
                // The semaphore is closed only when every slot has been
                // condemned; closing is what wakes everyone blocked on it.
                Err(_) => {
                    return Err(ProviderError::Unavailable(
                        "all pooled guest connections are dead".into(),
                    ))
                }
            };
            let slot = self.busy.iter().enumerate().position(|(i, busy)| {
                !self.dead[i].load(std::sync::atomic::Ordering::Relaxed)
                    && busy
                        .compare_exchange(
                            false,
                            true,
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Relaxed,
                        )
                        .is_ok()
            });
            let Some(slot) = slot else {
                // Unreachable while the invariant holds (a condemned slot's
                // permit is retired, never available). Fail closed instead of
                // spinning if it is ever violated.
                drop(permit);
                return Err(ProviderError::Unavailable(
                    "no live pooled guest connection is free".into(),
                ));
            };
            if self.sessions[slot]
                .alive
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Ok(PooledSession {
                    session: Arc::clone(&self.sessions[slot]),
                    pool: Arc::clone(self),
                    slot,
                    _permit: permit,
                });
            }
            // Condemn: the claimer (the only holder of this slot's busy CAS)
            // marks the slot dead, releases the busy flag, and retires the
            // permit so the dead slot leaves the capacity pool entirely.
            self.dead[slot].store(true, std::sync::atomic::Ordering::Release);
            self.busy[slot].store(false, std::sync::atomic::Ordering::Release);
            permit.forget();
            if self
                .dead
                .iter()
                .all(|dead| dead.load(std::sync::atomic::Ordering::Relaxed))
            {
                // Retiring a permit never wakes a semaphore waiter, so once
                // nothing is live, close the semaphore: every blocked (and
                // every future) waiter surfaces the all-dead error instead
                // of parking forever.
                self.free.close();
                return Err(ProviderError::Unavailable(
                    "all pooled guest connections are dead".into(),
                ));
            }
            // Live slots remain: scan again. The next claim either proceeds
            // immediately (another free live slot) or parks until one of
            // their holders discharges — a retired permit is never handed
            // back, so this cannot spin on a corpse.
        }
    }
}

/// Hex-encode a 128-bit wire request id for diagnostics traces.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn rid_hex(request_id: &[u8; 16]) -> String {
    request_id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Background demultiplexer: reads every inbound ZBRT frame and routes it to
/// the channel registered for its `request_id`. Frames for unregistered ids
/// (e.g. racing a completed request) are dropped.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn spawn_reader(
    mut reader: tokio::io::ReadHalf<tokio::net::UnixStream>,
    inflight: Arc<tokio::sync::Mutex<HashMap<[u8; 16], mpsc::Sender<Frame>>>>,
    alive: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut overflowed: u64 = 0;
        let mut budget_dropped: u64 = 0;
        // Cumulative admitted Output bytes per request id. Bounds host memory
        // per request to `DEMUX_REQUEST_BYTE_BUDGET` regardless of frame
        // count; entries are retired when the request's terminal frame is
        // routed (also on the no-in-flight path, so an abandoned request
        // never leaks its entry).
        let mut admitted: HashMap<[u8; 16], u64> = HashMap::new();
        // Guest output bytes DROPPED per request id (byte-budget overflow or
        // full-channel overflow). A request that lost frames can no longer
        // produce a trustworthy result, so its terminal frame is replaced by
        // a synthetic Error ([`terminal_frame`]) instead of letting exec
        // return a silently truncated result with a misleading exit code;
        // entries retire together with the terminal.
        let mut dropped: HashMap<[u8; 16], u64> = HashMap::new();
        loop {
            // Canonical single-implementation frame decoder shared with the
            // guest (`rfb::protocol` re-exports rfb-runtime's zeroboot
            // protocol); the connection closing (or a protocol error) ends
            // the demultiplexer.
            let frame = match read_frame_async(&mut reader).await {
                Ok(frame) => frame,
                Err(_) => break,
            };
            let sender = inflight.lock().await.get(&frame.request_id).cloned();
            let Some(sender) = sender else {
                // Late or unknown frames (e.g. racing a completed request) are
                // dropped by design — but they are also the classic symptom of
                // a routing bug, so leave a trace. The request is gone either
                // way: retire its budget/drop entries so an abandoned request
                // cannot leak them.
                let _ = admitted.remove(&frame.request_id);
                let _ = dropped.remove(&frame.request_id);
                eprintln!(
                    "rfb zeroboot: dropped frame kind={:?} id={} (no in-flight request)",
                    frame.kind,
                    rid_hex(&frame.request_id)
                );
                continue;
            };
            match frame.kind {
                // Terminal frames end the consumer's control loop; dropping
                // one would turn a finished command into a fake Timeout, so
                // they are delivered even against a full channel.
                Kind::Exit | Kind::Result | Kind::Error => {
                    let _ = admitted.remove(&frame.request_id);
                    let kind = frame.kind;
                    let frame = terminal_frame(&frame, &mut dropped);
                    if !deliver_terminal(&sender, &frame).await {
                        eprintln!(
                            "rfb zeroboot: dropped terminal frame kind={:?} id={} (channel never drained)",
                            kind,
                            rid_hex(&frame.request_id)
                        );
                    }
                    continue;
                }
                Kind::Output => {
                    // Byte-budgeted admission: the frame-count channel bound
                    // alone caps a request's queued bytes only if frames are
                    // small, so a firehose guest is capped here instead.
                    let frame_bytes = (HEADER_LEN + frame.payload.len()) as u64;
                    let admitted_bytes = admitted.entry(frame.request_id).or_insert(0);
                    if *admitted_bytes + frame_bytes > DEMUX_REQUEST_BYTE_BUDGET as u64 {
                        budget_dropped += 1;
                        if budget_dropped == 1 || budget_dropped.is_multiple_of(1024) {
                            eprintln!(
                                "rfb zeroboot: request over the demux byte budget, dropped {budget_dropped} frame(s) so far"
                            );
                        }
                        // Budget overflow is lost guest output for this
                        // request: record it so the terminal fails closed.
                        *dropped.entry(frame.request_id).or_insert(0) += frame_bytes;
                        continue;
                    }
                    *admitted_bytes += frame_bytes;
                }
                // Small control frames (acknowledgements): frame-count bounded
                // like before.
                _ => {}
            }
            // Bounded channel: a stalled or abandoned consumer cannot
            // grow host memory without bound. Overflow is dropped, with
            // a rate-limited trace (first drop, then every 1024th) so a
            // firehose guest cannot flood stderr either.
            if let Err(error) = sender.try_send(frame) {
                overflowed += 1;
                if overflowed == 1 || overflowed.is_multiple_of(1024) {
                    eprintln!(
                        "rfb zeroboot: request channel full, dropped {overflowed} frame(s) so far"
                    );
                }
                // An Output frame that never reached the consumer is lost
                // guest data under host backpressure: record it so the
                // terminal frame fails closed instead of returning a cleanly
                // truncated result.
                if let mpsc::error::TrySendError::Full(frame) = error {
                    if frame.kind == Kind::Output {
                        *dropped.entry(frame.request_id).or_insert(0) +=
                            (HEADER_LEN + frame.payload.len()) as u64;
                    }
                }
            }
        }
        // The connection is gone: mark the session dead so pool routing
        // retires the slot, then close every pending request channel so all
        // waiters observe `None` and fail instead of hanging.
        alive.store(false, std::sync::atomic::Ordering::Release);
        let senders: Vec<_> = inflight.lock().await.drain().map(|(_, tx)| tx).collect();
        drop(senders);
    })
}

/// Deliver a terminal frame even when its request's demux channel is
/// momentarily full: the consumer's control loop terminates on exactly this
/// frame, so dropping it would turn a completed command into a fake Timeout.
/// Retries with a 1 ms backoff until the consumer drains (the common case),
/// the consumer disappears (channel closed — nothing left to deliver), or a
/// generous window passes (degrade to the drop behavior rather than wedging
/// the demultiplexer for every other request).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn deliver_terminal(sender: &mpsc::Sender<Frame>, frame: &Frame) -> bool {
    let deadline = tokio::time::Instant::now() + TERMINAL_DELIVERY_WINDOW;
    loop {
        match sender.try_send(frame.clone()) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
}

/// Terminal delivery for one request: when the demultiplexer dropped any of
/// the request's Output frames (byte-budget or full-channel overflow), the
/// clean terminal is replaced by a synthetic `Error` frame carrying the
/// dropped byte count, so exec/stream fail closed on the lost output instead
/// of surfacing a silently truncated result with a misleading exit code. A
/// request without drops gets its own terminal verbatim, and a guest `Error`
/// terminal is delivered unchanged (the command already failed).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn terminal_frame(terminal: &Frame, dropped: &mut HashMap<[u8; 16], u64>) -> Frame {
    let dropped_bytes = dropped.remove(&terminal.request_id).unwrap_or(0);
    if dropped_bytes == 0 || terminal.kind == Kind::Error {
        return terminal.clone();
    }
    Frame {
        kind: Kind::Error,
        flags: 0,
        request_id: terminal.request_id,
        payload: ProtocolError {
            code: 1,
            message: format!(
                "host dropped {dropped_bytes} byte(s) of guest output (demux overflow); the result is incomplete"
            ),
        }
        .encode()
        .unwrap_or_default(),
    }
}

/// Wait for the next frame of a request, bounded by the remaining exec
/// deadline (or the fixed I/O timeout when no deadline is set).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn await_frame_bounded(
    rx: &mut mpsc::Receiver<Frame>,
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
    rx: &mut mpsc::Receiver<Frame>,
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
    rx: mpsc::Receiver<Frame>,
    /// Absolute deadline for every frame wait on this stream (output reads
    /// and the `stop` drain). Derived from the Execute request's timeout.
    deadline: tokio::time::Instant,
    terminated: bool,
    /// Events decoded but not yet delivered (legacy `Result` frames expand to
    /// stdout/stderr chunks plus a terminal Exit).
    pending: std::collections::VecDeque<StreamEvent>,
    /// Turn + inflight + active-turn ownership for this stream's request.
    /// Released as soon as the terminal frame discharges the turn — or on
    /// drop, which also unregisters the request and frees the queue (the
    /// guest turn itself stays active until the next request fails closed —
    /// the pre-existing abandoned-stream behavior).
    guard: Option<TurnGuard>,
    /// Pool slot backing this stream; released when the stream discharges or
    /// drops, so another request can claim the connection.
    _slot: Option<PooledSession>,
    /// Aggregate delivered output bytes (stdout + stderr). Streams share the
    /// exec path's 16 MiB cap: without it a chatty guest over an
    /// unbounded channel grows host memory without bound.
    output_bytes: usize,
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl ZeroBootStream {
    /// Mark the stream terminated and release its turn, inflight
    /// registration, and pool slot so a queued exec or stream can start.
    fn discharge(&mut self) {
        self.terminated = true;
        self.guard = None;
        self._slot = None;
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl Drop for ZeroBootStream {
    fn drop(&mut self) {
        // Cleanup is delegated to the RAII fields: a stream that never
        // reached its terminal frame is cleaned up here by `guard`
        // (inflight entry + active turn + turn lock) and `_slot` (pool
        // capacity). The pending queue and receiver drop with the struct.
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl GuestStream for ZeroBootStream {
    fn next_event<'a>(
        &'a mut self,
    ) -> BoxFuture<'a, std::result::Result<Option<StreamEvent>, SandboxError>> {
        Box::pin(async move {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
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
                    return Ok(None);
                }
                Err(error) => {
                    self.discharge();
                    return Err(map_session_error(error));
                }
            };
            if frame.request_id != self.request_id {
                self.discharge();
                return Err(SandboxError::Transport(
                    "request_id mismatch on stream frame".into(),
                ));
            }
            match frame.kind {
                Kind::Output => {
                    let output = Output::decode(&frame.payload).map_err(|e| {
                        SandboxError::Transport(format!("invalid Output frame: {e}"))
                    })?;
                    let event = match output.stream {
                        0 => StreamEvent::Stdout { data: output.data },
                        1 => StreamEvent::Stderr { data: output.data },
                        other => {
                            self.discharge();
                            return Err(SandboxError::Transport(format!(
                                "unknown output stream id {other}"
                            )));
                        }
                    };
                    // Same aggregate cap the exec path enforces.
                    self.output_bytes = self.output_bytes.saturating_add(event_data_len(&event));
                    if self.output_bytes > MAX_EXEC_OUTPUT_BYTES {
                        self.discharge();
                        return Err(SandboxError::Transport(
                            "guest output exceeded the 16 MiB limit".into(),
                        ));
                    }
                    Ok(Some(event))
                }
                Kind::Exit => {
                    let exit = Exit::decode(&frame.payload)
                        .map_err(|e| SandboxError::Transport(format!("invalid Exit frame: {e}")))?;
                    self.discharge();
                    Ok(Some(StreamEvent::Exit {
                        code: Some(exit.code),
                    }))
                }
                Kind::Result => {
                    let (code, out, err) = parse_legacy_result(&frame.payload)
                        .map_err(|e| SandboxError::Transport(e.to_string()))?;
                    self.discharge();
                    // Emit the captured streams first (stdout has wire
                    // priority), then the terminal Exit. These are queued
                    // because discharge() drops the frame sender: reading
                    // them from the channel would report a clean end before
                    // the exit code is delivered.
                    if !out.is_empty() {
                        self.pending.push_back(StreamEvent::Stdout { data: out });
                    }
                    if !err.is_empty() {
                        self.pending.push_back(StreamEvent::Stderr { data: err });
                    }
                    self.pending
                        .push_back(StreamEvent::Exit { code: Some(code) });
                    Ok(self.pending.pop_front())
                }
                Kind::Error => {
                    let error = remote_error(&frame.payload);
                    self.discharge();
                    Err(match error {
                        SessionError::Remote { message, .. } => SandboxError::Execution(message),
                        other => SandboxError::Transport(other.to_string()),
                    })
                }
                other => Err(SandboxError::Transport(format!(
                    "unexpected stream frame {other:?}"
                ))),
            }
        })
    }

    fn send_input<'a>(
        &'a mut self,
        _input: String,
    ) -> BoxFuture<'a, std::result::Result<(), SandboxError>> {
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
                            return Ok(());
                        }
                    }
                    Err(SessionError::Io(_)) => {
                        self.discharge();
                        return Ok(());
                    }
                    Err(error) => {
                        self.discharge();
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
fn stream_to_execute(
    spec: &StreamSpec,
    timeout: Duration,
) -> std::result::Result<Execute, SandboxError> {
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

// Default per-VM memory. The measured floor is 48 MiB (Firecracker rejects
// less); 128 leaves ~2.5x headroom for real commands and the embedded
// interpreters while keeping 100 hot-restored VMs inside a 2 GiB host
// (peak PSS ~170 MiB at the floor, per tests/zeroboot_concurrency.rs runs).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_MEM_MIB: u32 = 128;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_MEM_MIB_ENV: &str = "RFB_ZBRT_VM_MEM_MIB";
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn vm_mem_mib() -> u32 {
    // Capacity-evaluation override: lets operators measure the real memory
    // floor of a ZBRT microVM without touching wire behavior or defaults.
    match env_positive(VM_MEM_MIB_ENV) {
        Some(value) if value < 32 => {
            eprintln!(
                "rfb: {VM_MEM_MIB_ENV}={value} is below the 32 MiB sanity floor; \
                 falling back to the default {VM_MEM_MIB} MiB"
            );
            VM_MEM_MIB
        }
        // The measured Firecracker floor is 48 MiB (below that the VM can
        // fail its machine-config or OOM at boot); honor the override for
        // capacity probing but say so in the logs.
        Some(value) if value < 48 => {
            eprintln!(
                "rfb: {VM_MEM_MIB_ENV}={value} is below the measured Firecracker \
                 floor of 48 MiB; honoring it, but the VM may fail to boot"
            );
            value
        }
        Some(value) => value,
        None => VM_MEM_MIB,
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_VCPU: u32 = 1;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_VCPU_ENV: &str = "RFB_ZBRT_VM_VCPU";
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn vm_vcpu() -> u32 {
    // Capacity override: each guest session runs its command on a worker
    // thread, so a single vCPU serializes CPU-bound commands even when the
    // host multiplexes several sessions onto the VM.
    env_positive(VM_VCPU_ENV).unwrap_or(VM_VCPU)
}

/// Session pool size: how many ZBRT connections the provider opens per VM.
/// The V1 contract is single-active *per connection*, so this is the number of
/// commands the VM can run concurrently. Measured on a 1-vCPU VM: 16 workers
/// reach effective concurrency 14.97 (the ladder in rfb-ben); 8 keeps the
/// default parallel capacity well above the old 4 while costing only 8 idle
/// connections.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_SESSIONS: usize = 8;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const VM_SESSIONS_ENV: &str = "RFB_ZBRT_SESSIONS";
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn vm_sessions() -> usize {
    env_positive(VM_SESSIONS_ENV)
        .map(|n| n as usize)
        .unwrap_or(VM_SESSIONS)
}

/// Hot-start snapshot directory (`RFB_ZBRT_SNAPSHOT_DIR`). When set, the
/// provider boots its parent VM once, pauses it, and snapshots it; every later
/// sandbox restores from that snapshot instead of paying a cold boot
/// (~450-950 ms -> tens of ms). The directory holds the parent snapshot under
/// `parent/` (vmstate + memory.bin + identity.json); restored children share
/// the parent's memory file copy-on-write, so no per-create memory copy
/// happens. Put the directory on tmpfs for the best create latency.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const SNAPSHOT_DIR_ENV: &str = "RFB_ZBRT_SNAPSHOT_DIR";
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn snapshot_dir() -> Option<PathBuf> {
    std::env::var(SNAPSHOT_DIR_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

/// Snapshot shards (`RFB_ZBRT_SNAPSHOT_SHARDS`, default 2). Each shard keeps
/// its own parent snapshot and its own flock, so hot restores on different
/// shards run fully in parallel — one shard serializes at ~200 ms per create,
/// and N shards cut a 100-create burst's wall clock roughly N-fold (measured:
/// 1 shard ~200 ms/create, 2 shards ~106, 4 shards ~54). Default 2 buys the
/// halved burst latency for one extra parent boot (~1 s, once) and one extra
/// snapshot's worth of tmpfs pages. Round robin spreads creates evenly.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const SNAPSHOT_SHARDS_ENV: &str = "RFB_ZBRT_SNAPSHOT_SHARDS";
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn snapshot_shard() -> Option<PathBuf> {
    let base = snapshot_dir()?;
    let shards = env_positive(SNAPSHOT_SHARDS_ENV).unwrap_or(2).min(64) as usize;
    if shards <= 1 {
        return Some(base);
    }
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % shards;
    Some(base.join(format!("shard-{index}")))
}

/// Read a positive numeric override from the environment, ignoring malformed
/// or zero values so a bad knob can never boot a broken VM.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn env_positive(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|value| *value > 0)
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
/// Aggregate cap on one exec turn's captured output (mirrors the NDJSON/ZBRT
/// response caps).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const MAX_EXEC_OUTPUT_BYTES: usize = crate::core::MAX_GUEST_PAYLOAD_BYTES;

/// Delivered payload bytes of one stream event (for the stream output cap).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn event_data_len(event: &StreamEvent) -> usize {
    match event {
        StreamEvent::Stdout { data } | StreamEvent::Stderr { data } => data.len(),
        _ => 0,
    }
}

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
/// Parse the legacy single-frame `Result` payload (`exit_code:i32,
/// stdout_len:u32, stderr_len:u32` then the bytes) — the canonical decoder
/// shared by the provider and the CLI verify path.
pub fn parse_legacy_result(
    payload: &[u8],
) -> std::result::Result<(i32, Vec<u8>, Vec<u8>), SessionError> {
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
    Ok((
        exit,
        payload[12..12 + out].to_vec(),
        payload[12 + out..].to_vec(),
    ))
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
fn map_provider_error(error: ProviderError) -> SandboxError {
    SandboxError::Transport(error.to_string())
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn check_supported(
    session: &ZeroBootSession,
    capability: Capability,
) -> std::result::Result<(), SandboxError> {
    // The wire name comes from the same table the HelloAck inverse map uses —
    // one vocabulary, no string literals at call sites.
    match capability_name(capability) {
        Some(name) if session.supports(name) => Ok(()),
        _ => Err(SandboxError::UnsupportedCapability(capability)),
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
/// Copy `source` to a private `<work_path>/rootfs.ext4` and return its path.
/// Every boot/restore stages its own copy so a guest's rw writes never touch
/// the artifact image (or a sibling's staged copy).
///
/// # Errors
///
/// Returns `Err` when the copy fails.
pub fn stage_private_rootfs(work_path: &str, source: &str) -> Result<String> {
    let staged = std::path::Path::new(work_path).join("rootfs.ext4");
    std::fs::copy(source, &staged)
        .map_err(|e| Error::Backend(format!("staging private rootfs failed: {e}")))?;
    staged
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Backend("private rootfs path is not valid UTF-8".into()))
}

/// The vsock relay UDS path a booted or restored VM's runtime baked in.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn vm_uds(vm: &crate::firecracker::FirecrackerVm) -> Result<String> {
    vm.vsock_uds_path()
        .map(str::to_owned)
        .ok_or_else(|| Error::Backend("missing vsock UDS".into()))
}

/// Create the per-sandbox work dir, its UTF-8 path, and the flock liveness
/// marker: the startup scavenger reclaims the dir only if it can take that
/// flock, which is impossible while the holder is alive.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn create_work_dir() -> Result<(tempfile::TempDir, String, std::fs::File)> {
    let work = tempfile::Builder::new()
        .prefix("rfb-zeroboot-")
        .tempdir()
        .map_err(|e| Error::Backend(e.to_string()))?;
    let work_path = work
        .path()
        .to_str()
        .ok_or_else(|| Error::Backend("invalid work path".into()))?
        .to_owned();
    let work_lock = create_work_lock(&work_path)?;
    Ok((work, work_path, work_lock))
}

/// Stage a private rootfs copy into `work_path` and boot one Firecracker VM
/// with the process-wide shape, on the blocking pool; returns the VM and its
/// baked vsock relay UDS path. `label` names the worker in join-failure
/// diagnostics.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn boot_vm_with_staged_rootfs(
    label: &str,
    firecracker: String,
    kernel: String,
    rootfs: String,
    work_path: String,
) -> Result<(crate::firecracker::FirecrackerVm, String)> {
    tokio::task::spawn_blocking(move || {
        let rootfs = stage_private_rootfs(&work_path, &rootfs)?;
        let vm = crate::firecracker::FirecrackerVm::boot_with_runtime(
            &firecracker,
            &kernel,
            &rootfs,
            &work_path,
            crate::firecracker::VmResources::new(vm_mem_mib(), vm_vcpu()),
            VM_INIT_PATH,
            GUEST_CID,
        )
        .map_err(|e| Error::Backend(e.to_string()))?;
        let uds = vm_uds(&vm)?;
        Ok::<_, Error>((vm, uds))
    })
    .await
    .map_err(|e| Error::Backend(format!("ZeroBoot {label} worker failed: {e}")))?
}

/// Open the primary pool session, hand it ownership of the VM, its work dir,
/// and the scavenger lock, and fan out the remaining pool sessions over the
/// same relay UDS. Dropping the primary session tears the VM down; the work
/// lock keeps the dir scavenge-proof while the pool lives. `work_path` set
/// attaches the child's Firecracker log tail to a failed open (restore
/// diagnostics).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn open_primary_pool(
    config: &Config,
    uds: &str,
    work_path: Option<&str>,
    work: tempfile::TempDir,
    work_lock: std::fs::File,
    vm: crate::firecracker::FirecrackerVm,
) -> Result<Vec<Arc<ZeroBootSession>>> {
    let mut primary = ZeroBootSession::open(uds, config.guest_port, SESSION_CONNECT_TIMEOUT)
        .await
        .map_err(|e| match work_path {
            // A failed vsock relay restore shows up here as a session-open
            // failure only; attach the child's Firecracker log tail.
            Some(work_path) => {
                let log = std::fs::read_to_string(format!("{work_path}/firecracker.log"))
                    .unwrap_or_default();
                let tail: String = log.lines().rev().take(8).collect::<Vec<_>>().join(" | ");
                Error::Backend(format!(
                    "ZeroBoot session open failed: {e}; firecracker log tail: {tail}"
                ))
            }
            None => Error::Backend(format!("ZeroBoot session open failed: {e}")),
        })?;
    primary._work = Some(work);
    primary._work_lock = Some(work_lock);
    primary._vm = Some(vm);
    let mut sessions = vec![Arc::new(primary)];
    sessions.extend(open_extra_sessions(uds, config.guest_port).await?);
    Ok(sessions)
}

/// Boot one Firecracker VM for a sandbox and return the negotiated session
/// pool bound to it. The VM keeps running for the pool's lifetime, so every
/// later exec/control RPC runs without a cold boot. The pool size comes from
/// `RFB_ZBRT_SESSIONS`; extra sessions pay one connect + Hello handshake at
/// create time and then serve commands concurrently.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn boot_and_open(config: &Config) -> Result<Vec<Arc<ZeroBootSession>>> {
    let (firecracker, kernel, rootfs) = vm_paths(config)?;
    // Fail closed when the configured Firecracker binary contradicts a
    // neighboring SHA256SUMS manifest, before any process is spawned.
    verify_firecracker(config)?;
    let (work, work_path, work_lock) = create_work_dir()?;
    let (vm, uds) =
        boot_vm_with_staged_rootfs("boot", firecracker, kernel, rootfs, work_path).await?;
    open_primary_pool(config, &uds, None, work, work_lock, vm).await
}

/// Open the remaining pool sessions concurrently over one connected relay:
/// every session is an independent connection to the same guest, so a serial
/// loop would multiply the per-open cost by the pool size on every boot.
/// Shared by the cold-boot and the snapshot-restore paths.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn open_extra_sessions(uds: &str, port: u32) -> Result<Vec<Arc<ZeroBootSession>>> {
    let extra = vm_sessions().saturating_sub(1);
    let mut sessions = Vec::with_capacity(extra);
    if extra == 0 {
        return Ok(sessions);
    }
    let mut handles = Vec::with_capacity(extra);
    for _ in 0..extra {
        let uds = uds.to_owned();
        handles.push(tokio::spawn(async move {
            ZeroBootSession::open(&uds, port, SESSION_CONNECT_TIMEOUT).await
        }));
    }
    for handle in handles {
        let session = handle
            .await
            .map_err(|e| Error::Backend(format!("ZeroBoot session task failed: {e}")))?
            .map_err(|e| Error::Backend(format!("ZeroBoot session open failed: {e}")))?;
        sessions.push(Arc::new(session));
    }
    Ok(sessions)
}

/// A content fingerprint of everything a parent snapshot bakes in. A restored
/// child inherits the parent's kernel, rootfs content, machine shape, and
/// guest protocol port, so any change here invalidates the stored snapshot.
/// The three asset files are hashed by CONTENT (not path): rebuilding an
/// image at the same path must invalidate the stored snapshot.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
const SNAPSHOT_IDENTITY_VERSION: u32 = 2;

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn file_sha256(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    // Process-level (len, mtime) cache: hot creates re-fingerprint the same
    // 3 assets (kernel/rootfs/firecracker) on every boot_and_open /
    // ensure_parent_snapshot, and the rootfs is hundreds of MiB. A stat pair
    // is orders of magnitude cheaper than re-hashing; any write bumps mtime
    // (or length), which invalidates the entry.
    type Cache = std::sync::Mutex<HashMap<PathBuf, (u64, u128, String)>>;
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    let lock_cache = || CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let stamp = std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.len(), mtime_nanos(&meta)));
    let cache_key = stamp.map(|(len, mtime)| (path.to_path_buf(), len, mtime));
    if let Some((key_path, len, mtime)) = &cache_key {
        let cache = lock_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_len, cached_mtime, hash)) = cache.get(key_path) {
            if *cached_len == *len && *cached_mtime == *mtime {
                return hash.clone();
            }
        }
    }
    let mut hasher = Sha256::new();
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return "unreadable".to_owned(),
    };
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match std::io::Read::read(&mut file, &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => hasher.update(&chunk[..n]),
        }
    }
    let hash: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if let Some((key_path, len, mtime)) = cache_key {
        lock_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key_path, (len, mtime, hash.clone()));
    }
    hash
}

/// Nanosecond modification time of `meta`, 0 when the platform cannot report
/// it (used only as a cache-invalidation stamp, never as a security signal).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or(0)
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn snapshot_identity(config: &Config) -> String {
    let hash = |path: &Option<PathBuf>| match path {
        Some(p) if p.is_file() => file_sha256(p),
        _ => "missing".to_owned(),
    };
    let identity = serde_json::json!({
        "version": SNAPSHOT_IDENTITY_VERSION,
        "kernel_sha256": hash(&config.kernel),
        "rootfs_sha256": hash(&config.rootfs),
        "firecracker_sha256": hash(&config.firecracker),
        "guest_port": config.guest_port,
        "mem_mib": vm_mem_mib(),
        "vcpus": vm_vcpu(),
    });
    identity.to_string()
}

/// Serialize the provider's VM configuration for every boot/restore path. All
/// fields are required by `validate()` before this runs.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn vm_paths(config: &Config) -> Result<(String, String, String)> {
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
    Ok((firecracker.to_owned(), kernel.to_owned(), rootfs.to_owned()))
}

/// Cross-process + cross-task lock around hot-start creates (flock on
/// `<dir>/parent.lock`). The locked window covers [remove baked relay path →
/// spawn Firecracker → configure → load snapshot → open the pool sessions]:
///
/// - preparing a parent snapshot while another process restores the same dir
///   would corrupt the shared parent (two booted VMs, one work dir);
/// - the patched Firecracker build bakes ONE relay path into the snapshot, so
///   every restore binds that same path name — binding races and (harmlessly,
///   because sessions opened inside the lock stay connected to their own
///   inode) name swaps must be serialized.
///
/// Locks are held for ~100 ms per create; sandboxes run fully parallel after.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
struct SnapshotDirLock {
    _file: std::fs::File,
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl SnapshotDirLock {
    fn acquire(dir: &std::path::Path) -> Result<Self> {
        // A fresh host has no snapshot dir yet (/dev/shm is volatile); the
        // first cold create must be able to take the lock before anything
        // else creates the layout. Lock and dir are user-only: the lock file
        // itself is 0600, the dir 0700 (memory.bin lives beneath it).
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Backend(format!("create snapshot dir {}: {e}", dir.display())))?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let path = dir.join("parent.lock");
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(&path)
        }
        .map_err(|e| Error::Backend(format!("open snapshot lock {}: {e}", path.display())))?;
        // Bounded non-blocking flock loop: a stale lock holder is impossible
        // (the kernel releases flock when the owner's fds close), so a wait
        // here is genuine contention with a live create/restore. Hot creates
        // serialize at ~100 ms per shard; a two-minute wait means the holder
        // is wedged, and a clean error beats hanging the caller forever.
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            if flock_exclusive(file.as_raw_fd(), true) {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                if std::time::Instant::now() >= deadline {
                    return Err(Error::Backend(format!(
                        "flock {} timed out after 120s: a competing snapshot holder is wedged",
                        path.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            return Err(Error::Backend(format!("flock {}: {error}", path.display())));
        }
        Ok(Self { _file: file })
    }
}

/// Create `dir` (and its parents) and force mode 0700 immediately, failing
/// closed. Snapshot directories hold VM memory and guest secrets, so a chmod
/// that cannot be applied aborts the create instead of leaving a window (or a
/// permanently permissive directory).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn create_dir_private(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Backend(format!("create {}: {e}", dir.display())))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::Backend(format!("chmod 0700 {}: {e}", dir.display())))
}

/// Take `LOCK_EX` on an owned fd, blocking or non-blocking. Centralizes this
/// module's one unsafe flock: for every caller the safety argument is the
/// same — the fd is owned for the duration of the single `flock(2)` call, and
/// no close or reuse of it can race the syscall.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn flock_exclusive(fd: i32, nonblocking: bool) -> bool {
    let flags = if nonblocking {
        libc::LOCK_EX | libc::LOCK_NB
    } else {
        libc::LOCK_EX
    };
    // SAFETY: flock(2) on an fd the caller owns for the duration of the call.
    let rc = unsafe { libc::flock(fd, flags) };
    rc == 0
}

/// Create and LOCK_EX a scavenger-owned lock file with no open→flock window.
/// The file is created under a unique temporary name (`<path>.tmp-<pid>-<n>`),
/// flocked, marked, and only then renamed onto `path`: the scavenger probes
/// the final name, so it can never win a flock on a lock file whose owner has
/// not taken it yet — the window that let it reclaim a live work directory
/// mid-create. Callers create these in freshly-made directories, so the final
/// name must not exist; a collision fails closed rather than silently
/// replacing another holder's lock. A leftover temporary file from a crashed
/// create is invisible to the scavenger (wrong name) and harmless.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn create_owned_lock(
    path: &Path,
    marker: &[u8],
) -> std::result::Result<std::fs::File, std::io::Error> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut temp_name = path.as_os_str().to_os_string();
    temp_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let temp = std::path::PathBuf::from(temp_name);
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
    }?;
    let result = (|| {
        if !flock_exclusive(file.as_raw_fd(), false) {
            return Err(std::io::Error::last_os_error());
        }
        // Ownership marker (see create_work_lock): the directory name prefix
        // plus this content identify the directory as ours to the scavenger.
        {
            use std::io::Write;
            let _ = (&file).write_all(marker);
        }
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "lock file already exists",
            ));
        }
        std::fs::rename(&temp, path)
    })();
    if let Err(error) = result {
        drop(file);
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(file)
}

/// Create and LOCK_EX `<work_path>/work.lock`, returning the owning fd. A live
/// process holds this flock for the session's lifetime, which is exactly what
/// makes the process-startup scavenger safe: it only reclaims directories
/// whose lock it can take without blocking.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn create_work_lock(work_path: &str) -> Result<std::fs::File> {
    let path = Path::new(work_path).join("work.lock");
    create_owned_lock(&path, b"rfb-work-lock\n")
        .map_err(|e| Error::Backend(format!("create work lock {}: {e}", path.display())))
}

/// Fail closed when the configured Firecracker binary contradicts a
/// neighboring `SHA256SUMS` manifest (see
/// [`crate::zeroboot::verify_firecracker_binary`]).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn verify_firecracker(config: &Config) -> Result<()> {
    let path = config
        .firecracker
        .as_ref()
        .ok_or(Error::InvalidConfiguration("Firecracker path is required"))?;
    match super::verification::verify_firecracker_binary(path) {
        Ok(_) => Ok(()),
        Err(error) => Err(Error::Backend(error)),
    }
}

/// Reclaim stale directories directly under `root`. A candidate directory
/// must (a) carry the `dir_prefix` name prefix, (b) contain a `<lock_name>`
/// file whose content is an ownership marker written by our lock creators
/// (empty files from interrupted creates are tolerated), and (c) take a
/// NON-BLOCKING `LOCK_EX` flock — together these prove the directory is ours
/// AND no live process owns it; then it is removed and counted. Anything
/// else is left alone: the scavenger must never touch directories outside
/// its own namespace, no matter what they happen to contain. IO errors are
/// logged, never propagated; the scavenger runs on the create path and must
/// not fail a sandbox create.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
#[doc(hidden)]
pub fn scavenge_stale_in(root: &std::path::Path, lock_name: &str, dir_prefix: &str) -> usize {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!(
                "rfb zeroboot: scavenge cannot list {}: {error}",
                root.display()
            );
            return 0;
        }
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // Namespace filter first: never even probe directories that are not
        // ours by name.
        if !entry.file_name().to_string_lossy().starts_with(dir_prefix) {
            continue;
        }
        let lock_path = dir.join(lock_name);
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            // No lock file: not (yet) one of our live directories — keep it.
            Err(_) => continue,
        };
        // Content marker: an empty file is ours (pre-marker create race); a
        // non-empty file that does not carry our magic is not.
        {
            use std::io::Read;
            let mut magic = String::new();
            if file
                .try_clone()
                .and_then(|mut f| f.read_to_string(&mut magic))
                .is_ok()
                && !magic.is_empty()
                && !magic.starts_with("rfb-work-lock")
                && !magic.starts_with("rfb-fork-lock")
            {
                continue;
            }
        }
        // LOCK_NB never blocks; a live holder (or a transient error) leaves
        // the directory alone.
        if !flock_exclusive(file.as_raw_fd(), true) {
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed += 1,
            Err(error) => eprintln!("rfb zeroboot: scavenge {} failed: {error}", dir.display()),
        }
    }
    removed
}

/// Process-startup scavenger: once per process, reclaim orphaned session work
/// directories in the system temp dir (`rfb-zeroboot-*`, lock `work.lock`)
/// and stale fork checkpoint dirs under the snapshot base (`fork-*` per
/// shard, lock `fork.lock`). Repeated calls report the first call's count.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
#[doc(hidden)]
pub fn scavenge_stale_state(snapshot_base: Option<&std::path::Path>) -> usize {
    static SCAVENGED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SCAVENGED.get_or_init(|| {
        let mut removed = scavenge_stale_in(&std::env::temp_dir(), "work.lock", "rfb-zeroboot-");
        if let Some(base) = snapshot_base {
            // Sharded layouts keep their fork dirs under `shard-N/`; the
            // single-shard layout keeps them directly under the base.
            removed += scavenge_stale_in(base, "fork.lock", "fork-");
            if let Ok(entries) = std::fs::read_dir(base) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && entry.file_name().to_string_lossy().starts_with("shard-") {
                        removed += scavenge_stale_in(&path, "fork.lock", "fork-");
                    }
                }
            }
        }
        removed
    })
}

/// Tighten snapshot permissions: `memory.bin`/`vmstate` contain a full copy
/// of the VM's memory (guest secrets included) and must not be world-readable.
///
/// Directory hardening (`parent`, `parent/work` — and transitively every
/// fork checkpoint dir this is called on) is fail-closed: a chmod that fails
/// or verifies with a wider mode is an `Err`, because a shared tmpfs dir
/// exposing VM memory is exactly the state this function exists to prevent.
/// File hardening stays best-effort (eprintln + verification warning) so a
/// quirky filesystem cannot wedge the create path — the enclosing directory
/// is already 0700 there.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
fn harden_snapshot_permissions(parent: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tighten_dir = |path: &std::path::Path| -> Result<()> {
        if !path.is_dir() {
            // Fork checkpoints have no `work/` subdir; nothing to harden.
            return Ok(());
        }
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)) {
            return Err(Error::Backend(format!(
                "snapshot dir chmod failed on {}: {error}",
                path.display()
            )));
        }
        match std::fs::metadata(path) {
            Ok(meta) if meta.permissions().mode() & 0o777 != 0o700 => Err(Error::Backend(format!(
                "snapshot dir {} keeps permissive mode {:o} (expected 700)",
                path.display(),
                meta.permissions().mode() & 0o777
            ))),
            Ok(_) => Ok(()),
            Err(error) => Err(Error::Backend(format!(
                "snapshot dir mode verify failed on {}: {error}",
                path.display()
            ))),
        }
    };
    let tighten_file = |path: &std::path::Path| {
        if !path.is_file() {
            return;
        }
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!(
                "rfb: snapshot permission hardening failed on {}: {error}",
                path.display()
            );
            return;
        }
        match std::fs::metadata(path) {
            Ok(meta) if meta.permissions().mode() & 0o777 != 0o600 => {
                eprintln!(
                    "rfb: snapshot {} keeps permissive mode {:o} (expected 600)",
                    path.display(),
                    meta.permissions().mode() & 0o777
                );
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!(
                    "rfb: snapshot mode verify failed on {}: {error}",
                    path.display()
                );
            }
        }
    };
    tighten_dir(parent)?;
    tighten_dir(&parent.join("work"))?;
    for name in ["vmstate", "memory.bin", "identity.json"] {
        tighten_file(&parent.join(name));
    }
    Ok(())
}

/// Return the parent snapshot paths for `dir`, creating the snapshot on first
/// use: boot a parent VM, verify the agent with one probe session, close it
/// deterministically, pause, and snapshot. The caller holds the
/// [`SnapshotDirLock`] across this and the restore.
///
/// Layout under `dir/parent/`: `vmstate`, `memory.bin`, `identity.json`
/// (written last — its presence marks the snapshot complete), plus the paths
/// the snapshot bakes in and every restored child reuses: `rootfs.ext4` (the
/// parent's staged image; guest writes land on the per-VM workspace tmpfs, so
/// sharing the image across children is safe) and `work/` (holds the vsock
/// relay UDS path baked into the vmstate).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn ensure_parent_snapshot(
    config: &Config,
    dir: &std::path::Path,
) -> Result<(PathBuf, PathBuf)> {
    let parent = dir.join("parent");
    let vmstate = parent.join("vmstate");
    let mem = parent.join("memory.bin");
    let identity_path = parent.join("identity.json");
    if vmstate.is_file() && mem.is_file() {
        // A stored snapshot must match the current kernel/rootfs/shape
        // (content-hashed), or a restored child would silently boot a stale
        // guest configuration. identity.json is written after both snapshot
        // files, so it doubles as the completeness marker for an interrupted
        // prepare. The caller holds the dir lock, so the check+delete is
        // race-free.
        let matches = std::fs::read_to_string(&identity_path)
            .map(|stored| stored == snapshot_identity(config))
            .unwrap_or(false);
        if matches {
            return Ok((vmstate, mem));
        }
        let _ = std::fs::remove_dir_all(&parent);
    }
    let (firecracker, kernel, rootfs) = vm_paths(config)?;
    // Fail closed before booting the parent when a neighboring SHA256SUMS
    // manifest lists this binary with a different digest.
    verify_firecracker(config)?;
    create_dir_private(&parent)?;
    let work_path = parent
        .join("work")
        .to_str()
        .ok_or_else(|| Error::Backend("parent work path is not valid UTF-8".into()))?
        .to_owned();
    create_dir_private(std::path::Path::new(&work_path))?;
    let (mut vm, _uds) =
        boot_vm_with_staged_rootfs("parent snapshot", firecracker, kernel, rootfs, work_path)
            .await?;
    // The probe session blocks until the guest's vsock listener answers, so
    // the pause below always snapshots a fully booted agent. close() shuts
    // the write half down (FIN), which deterministically ends the guest's
    // serve loop — unlike a bare drop, which leaves the reader task owning
    // the socket.
    let probe = ZeroBootSession::open(&_uds, config.guest_port, SESSION_CONNECT_TIMEOUT)
        .await
        .map_err(|e| Error::Backend(format!("ZeroBoot parent probe failed: {e}")))?;
    if !probe.supports("execute") {
        return Err(Error::Backend(
            "ZeroBoot parent guest did not advertise execute".into(),
        ));
    }
    probe.close().await;
    // Brief settle for the guest's connection teardown before the pause;
    // snapshotting mid-teardown froze new post-restore connections.
    tokio::time::sleep(Duration::from_millis(150)).await;
    tokio::task::spawn_blocking({
        let vmstate = vmstate.clone();
        let mem = mem.clone();
        move || {
            vm.create_snapshot(
                vmstate
                    .to_str()
                    .ok_or_else(|| Error::Backend("vmstate path is not valid UTF-8".into()))?,
                mem.to_str()
                    .ok_or_else(|| Error::Backend("mem path is not valid UTF-8".into()))?,
            )
            .map_err(|e| Error::Backend(e.to_string()))?;
            // Killing the parent firecracker releases the VM; the snapshot
            // files are already durable on disk.
            vm.kill();
            Ok::<_, Error>(())
        }
    })
    .await
    .map_err(|e| Error::Backend(format!("ZeroBoot parent snapshot writer failed: {e}")))??;
    // identity.json doubles as the snapshot's completeness marker (written
    // after both snapshot files). Harden BEFORE checking the write result: a
    // failed prepare must not leave a readable half-snapshot behind either —
    // the chmod is fail-closed, the write error is reported only after it.
    let identity_result = std::fs::write(&identity_path, snapshot_identity(config))
        .map_err(|e| Error::Backend(e.to_string()));
    harden_snapshot_permissions(&parent)?;
    identity_result?;
    Ok((vmstate, mem))
}

/// Which restore flavor the configured Firecracker build took (process-wide;
/// one binary per process is the deployment shape). 0 = unknown, 1 = upstream
/// (device re-declaration works), 2 = patched (bare load only).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
static RESTORE_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Create a sandbox by restoring the parent snapshot: no kernel boot, no
/// memory copy. The caller must hold the [`SnapshotDirLock`] for `dir` across
/// this call — the locked window ends when the pool sessions are connected,
/// which is exactly what makes concurrent hot creates safe on the patched
/// Firecracker build (each restore rebinds the baked relay path name; already
/// connected sessions keep their own inode).
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
async fn restore_and_open(
    config: &Config,
    dir: &std::path::Path,
) -> Result<Vec<Arc<ZeroBootSession>>> {
    use std::sync::atomic::Ordering;
    let (vmstate, mem) = ensure_parent_snapshot(config, dir).await?;
    // Re-harden before any restore touches the snapshot: the cached-snapshot
    // early return skips ensure_parent_snapshot's own hardening, and a
    // permissive mode must fail the restore rather than be tolerated.
    harden_snapshot_permissions(&dir.join("parent"))?;
    let (firecracker, _kernel, rootfs) = vm_paths(config)?;
    let (work, work_path, work_lock) = create_work_dir()?;
    let mode = RESTORE_MODE.load(Ordering::Relaxed);
    // Bare restore (patched build) reuses the parent's baked rootfs — the
    // per-create staging copy would be discarded, so skip it entirely. The
    // parent's image is shared-safe: guest writes land on the workspace tmpfs.
    let (vm, uds) = tokio::task::spawn_blocking({
        let firecracker = firecracker.clone();
        let rootfs = rootfs.clone();
        let vmstate = vmstate.clone();
        let mem = mem.clone();
        let work_path = work_path.clone();
        move || {
            let staged = if mode == 2 {
                None
            } else {
                Some(stage_private_rootfs(&work_path, &rootfs)?)
            };
            let rootfs_for_child: &str = staged.as_deref().unwrap_or(&rootfs);
            let (vm, bare) = crate::firecracker::FirecrackerVm::restore_from_snapshot(
                &firecracker,
                &work_path,
                crate::firecracker::VmResources::new(vm_mem_mib(), vm_vcpu()),
                rootfs_for_child,
                GUEST_CID,
                &vmstate.to_string_lossy(),
                &mem.to_string_lossy(),
            )
            .map_err(|e| Error::Backend(e.to_string()))?;
            if bare {
                RESTORE_MODE.store(2, Ordering::Relaxed);
            } else {
                RESTORE_MODE.store(1, Ordering::Relaxed);
            }
            let uds = vm_uds(&vm)?;
            Ok::<_, Error>((vm, uds))
        }
    })
    .await
    .map_err(|e| Error::Backend(format!("ZeroBoot restore worker failed: {e}")))??;
    open_primary_pool(config, &uds, Some(work_path.as_str()), work, work_lock, vm).await
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
        Box::pin(
            async move { Ok(Box::new(self.create_zero_boot(spec).await?) as Box<dyn Sandbox>) },
        )
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
            crate::core::check_create_spec(&spec, self.capabilities())?;
            // ZeroBoot has no per-sandbox sizing knob on the create path
            // (memory/vcpu come from RFB_ZBRT_* process-wide env), so every
            // declared resource constraint fails closed instead of being
            // silently ignored.
            crate::core::check_create_resources(&spec, false)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                // Reclaim work/snapshot directories whose owning process died
                // (their flocks were dropped by the kernel). Once per process,
                // and only directories whose lock is provably unheld.
                let _ = scavenge_stale_state(snapshot_dir().as_deref());
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
                // Hot path: when RFB_ZBRT_SNAPSHOT_DIR is set, restore the
                // parent snapshot instead of booting a fresh VM per sandbox.
                // The shard's dir lock is held across ensure+restore+pool-open
                // and released as soon as this sandbox's sessions are
                // connected — that window is what makes concurrent hot creates
                // safe; RFB_ZBRT_SNAPSHOT_SHARDS > 1 replicates the lock per
                // shard so large create bursts restore in parallel.
                // Cold path: one boot per sandbox (unchanged default).
                let shard = snapshot_shard();
                let sessions = match &shard {
                    Some(dir) => {
                        let _lock = tokio::task::spawn_blocking({
                            let dir = dir.clone();
                            move || SnapshotDirLock::acquire(&dir).map_err(|e| e.to_string())
                        })
                        .await
                        .map_err(|e| ProviderError::Unavailable(e.to_string()))?
                        .map_err(ProviderError::Unavailable)?;
                        // The base dir (parent of the shard) only exposes shard
                        // entries, but keep it owner-only as well once it exists.
                        #[cfg(target_os = "linux")]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            if let Some(base) = snapshot_dir() {
                                let _ = std::fs::set_permissions(
                                    &base,
                                    std::fs::Permissions::from_mode(0o700),
                                );
                            }
                        }
                        restore_and_open(&self.config, dir)
                            .await
                            .map_err(|e| ProviderError::Unavailable(e.to_string()))?
                    }
                    None => boot_and_open(&self.config)
                        .await
                        .map_err(|e| ProviderError::Unavailable(e.to_string()))?,
                };
                Ok(ZeroBootSandbox::from_pool(
                    self.config.clone(),
                    sessions,
                    shard,
                    None,
                ))
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                Err(ProviderError::UnsupportedCapability(Capability::Execute))
            }
        })
    }
}

/// Sandbox created by the ZeroBoot provider. It owns one Firecracker VM and a
/// pool of ZBRT vsock sessions into it; dropping the sandbox tears down the VM.
pub struct ZeroBootSandbox {
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    config: Arc<Config>,
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    pool: Arc<SessionPool>,
    /// Capabilities derived from the guest's HelloAck: only operations the
    /// guest actually supports end-to-end are advertised.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    capabilities: Vec<Capability>,
    /// Snapshot shard dir this sandbox's VM was restored from (hot mode
    /// only) — where future forks checkpoint to.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    snapshot_dir: Option<PathBuf>,
    /// Fork checkpoint this sandbox was restored from (if any). The
    /// checkpoint's `memory.bin` backs the sandbox's VM via CoW for its whole
    /// lifetime, so the dir is removed when the LAST sibling restored from it
    /// drops (shared [`ForkCheckpoint`] guard).
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    fork_checkpoint: Option<Arc<ForkCheckpoint>>,
}

/// Shared ownership of one fork checkpoint dir: dropped (and removed) when
/// the last sandbox restored from it goes away.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
struct ForkCheckpoint {
    dir: PathBuf,
    /// flock'd `<dir>/fork.lock`: the live-process liveness marker the
    /// process-startup scavenger probes, held for as long as any sibling
    /// restored from this checkpoint is alive.
    _lock: std::fs::File,
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl Drop for ForkCheckpoint {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Monotonic fork-dir suffix, combined with the pid so concurrent processes
/// never collide inside the shared snapshot dir.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
static FORK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl ZeroBootSandbox {
    /// One ZBRT filesystem RPC: validate, fail closed on an un-negotiated
    /// capability, encode, round-trip the Fs channel, and decode. The single
    /// generic behind ls/find/grep/read/write; `path` is the request's guest
    /// path (the same bytes the typed payload serializes).
    fn fs_rpc<'a, T, R>(
        &'a self,
        capability: Capability,
        op: u8,
        path: String,
        request: T,
    ) -> BoxFuture<'a, std::result::Result<R, SandboxError>>
    where
        T: FsRequest + Send + 'a,
        R: serde::de::DeserializeOwned + 'a,
    {
        Box::pin(async move {
            request.validate().map_err(SandboxError::InvalidSpec)?;
            #[cfg(all(feature = "zeroboot", target_os = "linux"))]
            {
                // Filesystem RPCs execute on the guest's workspace executor,
                // which a running turn has taken out of its connection's
                // runtime service. Routing through the pool claims a free
                // connection instead of failing (or queueing) behind the
                // primary session's active turn.
                let slot = self.pool.acquire().await.map_err(map_provider_error)?;
                check_supported(slot.session(), capability)?;
                let payload = serde_json::to_vec(&request)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                let data = slot
                    .session()
                    .fs(op, &path, payload)
                    .await
                    .map_err(map_session_error)?;
                serde_json::from_slice(&data)
                    .map_err(|e| SandboxError::Execution(format!("invalid FsResult payload: {e}")))
            }
            #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
            {
                let _ = (op, path);
                Err(SandboxError::UnsupportedCapability(capability))
            }
        })
    }
}

#[cfg(all(feature = "zeroboot", target_os = "linux"))]
impl ZeroBootSandbox {
    /// Assemble a sandbox around an already-open session pool; the advertised
    /// capabilities derive from the primary session's HelloAck. Shared by the
    /// provider's create path, fork, and the mock-guest test constructors so
    /// the field wiring cannot drift between them.
    fn from_pool(
        config: Arc<Config>,
        sessions: Vec<Arc<ZeroBootSession>>,
        snapshot_dir: Option<PathBuf>,
        fork_checkpoint: Option<Arc<ForkCheckpoint>>,
    ) -> Self {
        let capabilities = capabilities_from_negotiated(sessions[0].negotiated());
        Self {
            config,
            pool: SessionPool::new(sessions),
            capabilities,
            snapshot_dir,
            fork_checkpoint,
        }
    }

    /// Fork this sandbox: checkpoint the VM's full live state (pause → full
    /// snapshot), tear the checkpointed VM down, and restore TWO fresh VMs
    /// from the checkpoint — the returned pair is `(continued original,
    /// fork)`. Both start with every file, process, and byte of state the
    /// original had at fork time (the workspace is a tmpfs held in guest
    /// memory, so the checkpoint captures it whole).
    ///
    /// Why the original's VM is replaced: the checkpoint resets the guest's
    /// live vsock connections, and the patched Firecracker build bakes ONE
    /// relay path into every snapshot — two live VMs restored from one
    /// checkpoint hand that name to each other in a fixed order (restore A
    /// binds it, restore B rebinds it; A's already-connected pool keeps
    /// working). So instead of leaving the original unreachable behind a
    /// stolen name, fork() gives the caller two fully-connected sandboxes
    /// and consumes `self`.
    ///
    /// Mirrors forkd's live-fork semantics; the checkpoint is a full memory
    /// dump (no incremental diff), so one fork costs ~0.5-1 s.
    ///
    /// Requires hot mode (`RFB_ZBRT_SNAPSHOT_DIR`): the checkpoint dir must
    /// outlive this call, and cold-booted VMs bake a relay path inside a
    /// per-sandbox tempdir that dies with the original.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the sandbox has no hot-mode dir or attached VM, or
    /// when the checkpoint or the restores fail. On error the original VM is
    /// still torn down (it was checkpointed, then paused); the checkpoint is
    /// cleaned up.
    pub async fn fork(
        self,
    ) -> std::result::Result<(ZeroBootSandbox, ZeroBootSandbox), ProviderError> {
        use std::sync::atomic::Ordering;
        let Self {
            config,
            pool,
            capabilities: _,
            snapshot_dir,
            fork_checkpoint: _,
        } = self;
        let base = snapshot_dir.clone().ok_or_else(|| {
            ProviderError::Unavailable(
                "fork requires hot mode (RFB_ZBRT_SNAPSHOT_DIR): the checkpoint must live in a persistent snapshot directory".into(),
            )
        })?;
        let vm = pool.primary()._vm.as_ref().ok_or_else(|| {
            ProviderError::Unavailable("sandbox has no attached Firecracker VM".into())
        })?;
        let index = FORK_SEQ.fetch_add(1, Ordering::Relaxed);
        let fork_dir = base.join(format!("fork-{}-{index}", std::process::id()));
        // ensure_parent_snapshot's layout: <dir>/parent/{vmstate,memory.bin,
        // identity.json}. The fork checkpoint must adopt it exactly, or the
        // restore silently boots a fresh (stateless) parent instead.
        let checkpoint = fork_dir.join("parent");
        create_dir_private(&checkpoint).map_err(|e| ProviderError::Unavailable(e.to_string()))?;
        // fork_dir itself must be owner-only before the memory files land in
        // its subtree; a chmod failure aborts the fork (fail-closed).
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) =
                std::fs::set_permissions(&fork_dir, std::fs::Permissions::from_mode(0o700))
            {
                let _ = std::fs::remove_dir_all(&fork_dir);
                return Err(ProviderError::Unavailable(format!(
                    "fork dir chmod failed on {}: {error}",
                    fork_dir.display()
                )));
            }
        }
        // Liveness marker: created before the (possibly slow) checkpoint so a
        // concurrently starting process can never reclaim this dir mid-fork.
        let fork_lock = {
            let path = fork_dir.join("fork.lock");
            match create_owned_lock(&path, b"rfb-fork-lock\n") {
                Ok(file) => file,
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&fork_dir);
                    return Err(ProviderError::Unavailable(format!(
                        "create fork lock {}: {error}",
                        path.display()
                    )));
                }
            }
        };
        let vmstate = checkpoint.join("vmstate");
        let mem = checkpoint.join("memory.bin");
        let path_str = |path: &std::path::Path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| ProviderError::Unavailable("fork dir is not valid UTF-8".into()))
        };
        let vmstate_str = path_str(&vmstate)?;
        let mem_str = path_str(&mem)?;

        // Same lock window as a hot create: the checkpoint and both restores
        // interplay with the shared baked relay path, and the two restores'
        // rebinding must not race other creates. The VM API calls are
        // synchronous (~0.5-1 s); forks are rare next to creates, so they run
        // inline on the current worker.
        let _lock = tokio::task::spawn_blocking({
            let dir = base.clone();
            move || SnapshotDirLock::acquire(&dir).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| ProviderError::Unavailable(e.to_string()))?
        .map_err(ProviderError::Unavailable)?;

        // Checkpoint. On any failure below, the paused original and the
        // checkpoint dir must not leak: pool (the VM) drops with `pool`,
        // the dir with `checkpoint_guard`.
        let checkpoint_result = vm.create_snapshot(&vmstate_str, &mem_str).and_then(|()| {
            std::fs::write(checkpoint.join("identity.json"), snapshot_identity(&config)).map_err(
                |e| {
                    crate::firecracker::FirecrackerError::Protocol(format!(
                        "write fork identity: {e}"
                    ))
                },
            )
        });
        if let Err(error) = checkpoint_result {
            let _ = std::fs::remove_dir_all(&fork_dir);
            return Err(ProviderError::Unavailable(format!(
                "fork checkpoint failed: {error}"
            )));
        }
        // The checkpoint holds guest memory: a chmod that cannot be enforced
        // aborts the fork instead of publishing a readable checkpoint.
        if let Err(error) = harden_snapshot_permissions(&checkpoint) {
            let _ = std::fs::remove_dir_all(&fork_dir);
            return Err(ProviderError::Unavailable(format!(
                "fork checkpoint hardening failed: {error}"
            )));
        }
        // The original VM is now paused with its connections reset; tear it
        // down and restore two fresh children from the checkpoint instead.
        drop(pool);
        let checkpoint_guard = Arc::new(ForkCheckpoint {
            dir: fork_dir.clone(),
            _lock: fork_lock,
        });
        let mut restored = Vec::with_capacity(2);
        for _ in 0..2 {
            match restore_and_open(&config, &fork_dir).await {
                Ok(sessions) => {
                    restored.push(ZeroBootSandbox::from_pool(
                        Arc::clone(&config),
                        sessions,
                        Some(base.clone()),
                        Some(Arc::clone(&checkpoint_guard)),
                    ));
                }
                Err(error) => {
                    return Err(ProviderError::Unavailable(format!(
                        "fork restore failed: {error}"
                    )))
                }
            }
        }
        let mut sandboxes = restored.into_iter();
        let original = sandboxes.next().expect("two restores push two sandboxes");
        let fork = sandboxes.next().expect("two restores push two sandboxes");
        Ok((original, fork))
    }
}
impl ZeroBootSandbox {
    /// Build a sandbox around an already-connected session. Used by the
    /// provider after booting a VM and by mock-guest tests to exercise the
    /// sandbox routing surface without a real Firecracker runtime.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    #[doc(hidden)]
    pub fn from_session_for_test(config: Config, session: ZeroBootSession) -> Self {
        Self::from_sessions_for_test(config, vec![session])
    }

    /// Build a sandbox over an explicit session pool. Used by mock-guest tests
    /// that need several connections to one guest without booting a VM.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    #[doc(hidden)]
    pub fn from_sessions_for_test(config: Config, sessions: Vec<ZeroBootSession>) -> Self {
        assert!(!sessions.is_empty(), "a sandbox needs at least one session");
        Self::from_pool(
            Arc::new(config),
            sessions.into_iter().map(Arc::new).collect(),
            None,
            None,
        )
    }

    /// Test-only: OS process id of the backing Firecracker child.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    #[doc(hidden)]
    pub fn firecracker_pid(&self) -> Option<u32> {
        self.pool.primary().firecracker_pid()
    }

    /// Checkpoint dir this sandbox was forked from, when it was created by
    /// [`Self::fork`]. Diagnostics surface: the dir backs this sandbox's VM
    /// memory via CoW and disappears when the last fork sibling drops.
    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    pub fn fork_checkpoint_dir(&self) -> Option<&std::path::Path> {
        self.fork_checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.dir.as_path())
    }
}

/// Map the capability names the guest acked in its HelloAck onto typed
/// [`Capability`]s. The sandbox advertises exactly the negotiated set,
/// mirroring the provider surface: acking `filesystem` enables the full
/// `Ls`/`Find`/`Grep`/`ReadFile`/`WriteFile` group.
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
/// Derive the advertised capability set from the guest's HelloAck by inverting
/// [`capability_name`] — the wire vocabulary has exactly one table, so adding
/// a capability cannot desynchronize the two directions.
fn capabilities_from_negotiated(negotiated: &[String]) -> Vec<Capability> {
    use Capability::*;
    [
        Execute, Health, Stream, Cancel, Eval, Ls, Find, Grep, ReadFile, WriteFile,
    ]
    .into_iter()
    .filter(|capability| {
        capability_name(*capability).is_some_and(|name| negotiated.iter().any(|c| c == name))
    })
    .collect()
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
                check_supported(self.pool.primary(), Capability::Execute)?;
                let request = execute_request(&spec, self.config.timeout)
                    .map_err(|e| SandboxError::Execution(e.to_string()))?;
                // Claim a session for the whole command: concurrent execs run
                // on separate connections instead of queueing.
                let slot = self.pool.acquire().await.map_err(map_provider_error)?;
                slot.session()
                    .exec(request)
                    .await
                    .map_err(map_session_error)
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
                // Health stays on the first still-alive session: the guest
                // answers it directly off its runtime service (its workspace
                // executor is untouched — see
                // rfb-runtime/src/runtime_service/mod.rs `filesystem_rpc`),
                // so a probe interleaves with an active turn instead of
                // claiming a pool slot. Filesystem RPCs do NOT share that
                // property and route through `SessionPool::acquire` instead.
                let session = self.pool.primary_alive();
                check_supported(session, Capability::Health)?;
                session.health().await.map_err(map_session_error)
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
                check_supported(self.pool.primary(), Capability::Stream)?;
                let request = stream_to_execute(&spec, self.config.timeout)?;
                let slot = self.pool.acquire().await.map_err(map_provider_error)?;
                let session = Arc::clone(slot.session());
                let mut stream = session.stream(request).await.map_err(map_session_error)?;
                // The slot is held for as long as the stream occupies the
                // connection, and released when it discharges or drops.
                stream._slot = Some(slot);
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
                check_supported(self.pool.primary(), Capability::Cancel)?;
                // The target lives on whichever connection is running it. The
                // request's id is advisory/diagnostic only (it rides along as
                // the cancel reason): the provider cancels BROADLY — every
                // session that currently holds an active turn is targeted —
                // rather than trusting the caller's id to name the right
                // connection. Only connections with an in-flight request
                // actually send a Cancel; an idle session reports `false` so
                // an idempotent ack cannot claim success for a busy sibling,
                // and the fan-out never stops early on a first ack.
                let mut targeted = false;
                let mut last_error = None;
                for session in self.pool.all() {
                    match session.cancel_active(request.id.clone()).await {
                        Ok(sent) => targeted |= sent,
                        Err(error) => last_error = Some(error),
                    }
                }
                if let Some(error) = last_error {
                    return Err(map_session_error(error));
                }
                Ok(crate::guest::CancelResult {
                    cancelled: targeted,
                })
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
        Self::fs_rpc(
            self,
            Capability::Ls,
            fs_op::LS,
            request.path.clone(),
            request,
        )
    }
    fn find<'a>(
        &'a self,
        request: crate::guest::FindRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::FindResult, SandboxError>> {
        Self::fs_rpc(
            self,
            Capability::Find,
            fs_op::FIND,
            request.path.clone(),
            request,
        )
    }
    fn grep<'a>(
        &'a self,
        request: crate::guest::GrepRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::GrepResult, SandboxError>> {
        Self::fs_rpc(
            self,
            Capability::Grep,
            fs_op::GREP,
            request.path.clone(),
            request,
        )
    }
    fn read<'a>(
        &'a self,
        request: crate::guest::ReadRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::ReadResult, SandboxError>> {
        Self::fs_rpc(
            self,
            Capability::ReadFile,
            fs_op::READ,
            request.path.clone(),
            request,
        )
    }
    fn write<'a>(
        &'a self,
        request: crate::guest::WriteRequest,
    ) -> BoxFuture<'a, std::result::Result<crate::guest::WriteResult, SandboxError>> {
        Self::fs_rpc(
            self,
            Capability::WriteFile,
            fs_op::WRITE,
            request.path.clone(),
            request,
        )
    }
}

/// The filesystem request contract the ZBRT Fs channel serves: a serializable
/// payload that validates fail-closed. One generic RPC helper serves all five
/// ops (mirrors forkd's `tool()`); the guest path travels as its own argument.
trait FsRequest: serde::Serialize {
    fn validate(&self) -> std::result::Result<(), crate::ContractError>;
}

macro_rules! fs_request_impl {
    ($($ty:ty),* $(,)?) => {
        $(
            impl FsRequest for $ty {
                fn validate(&self) -> std::result::Result<(), crate::ContractError> {
                    // The inherent method wins over the trait method.
                    <$ty>::validate(self)
                }
            }
        )*
    };
}
fs_request_impl!(
    crate::guest::LsRequest,
    crate::guest::FindRequest,
    crate::guest::GrepRequest,
    crate::guest::ReadRequest,
    crate::guest::WriteRequest,
);
