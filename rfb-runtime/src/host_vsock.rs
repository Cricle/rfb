//! Host-side Firecracker vsock transport for the RFB1 framed protocol.
//!
//! Firecracker exposes the host side of a guest vsock as a Unix socket.  The
//! small `CONNECT <guest-port>\n` preamble is spoken on that socket; all bytes
//! after the `OK` response are the normal RFB1 protocol.  This intentionally
//! uses Tokio's UnixStream rather than issuing AF_VSOCK syscalls.

use std::path::PathBuf;
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(unix)]
use uuid::Uuid;

#[cfg(unix)]
use crate::codec::{read_frame, write_frame, Frame, FrameCodec, MessageType};
#[cfg(unix)]
use crate::session::{ControlMessage, RuntimeMessage};

/// A validated host-side vsock endpoint for one guest.
///
/// `new` enforces the Firecracker invariants up front: a CID greater than 2, a
/// non-zero port, and an absolute path to the host UDS relay. Identity pinning
/// via [`capture_identity`](Self::capture_identity) /
/// [`validate_current`](Self::validate_current) detects when the relay socket is
/// recreated between operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VsockEndpoint {
    /// Absolute path to the host-side Firecracker vsock UDS relay.
    pub host_uds: PathBuf,
    /// Guest CID used to identify the endpoint (not sent in the Firecracker UDS handshake).
    pub guest_cid: u32,
    /// Guest port used in the `CONNECT <guest-port>` preamble.
    pub guest_port: u32,
    /// Optional pinned identity of the relay socket.
    pub identity: Option<EndpointIdentity>,
}

/// Filesystem identity of the host UDS relay (device + inode + birth time).
///
/// Only populated on Unix where `stat` is available; on other platforms
/// identity capture always fails with [`VsockEndpointError::Unsupported`].
/// Birth time matters: ext4 can hand a freshly recreated socket the exact
/// inode of the deleted one, so (device, inode) alone misses replacements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointIdentity {
    /// Device number of the relay socket.
    #[cfg(unix)]
    pub device: u64,
    /// Inode of the relay socket.
    #[cfg(unix)]
    pub inode: u64,
    /// Birth time when the filesystem reports it (`None` otherwise — e.g.
    /// filesystems without `statx` birth-time support fall back to the
    /// device/inode pair).
    #[cfg(unix)]
    pub created: Option<std::time::SystemTime>,
}

/// Why a vsock endpoint could not be built or is no longer usable.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VsockEndpointError {
    /// Guest CID must be greater than 2 (0-2 are reserved).
    #[error("vsock CID must be greater than 2")]
    InvalidCid,
    /// Guest port 0 is reserved and cannot be dialed.
    #[error("vsock port must be non-zero")]
    InvalidPort,
    /// The host UDS path must be absolute.
    #[error("vsock UDS path must be absolute")]
    RelativePath,
    /// The relay socket is missing, not a socket, or otherwise unusable.
    #[error("vsock UDS endpoint is unavailable: {0}")]
    Unavailable(String),
    /// Vsock transport is not supported on this platform.
    #[error("vsock is unsupported on this platform")]
    Unsupported,
    /// The relay socket was recreated since identity was captured.
    #[error("vsock UDS endpoint identity changed")]
    Stale,
}

impl VsockEndpoint {
    /// Create an endpoint after validating its CID, port, and absolute UDS path.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn new(
        host_uds: impl Into<PathBuf>,
        guest_cid: u32,
        guest_port: u32,
    ) -> Result<Self, VsockEndpointError> {
        let host_uds = host_uds.into();
        if guest_cid <= 2 {
            return Err(VsockEndpointError::InvalidCid);
        }
        if guest_port == 0 {
            return Err(VsockEndpointError::InvalidPort);
        }
        if !host_uds.is_absolute() {
            return Err(VsockEndpointError::RelativePath);
        }
        Ok(Self {
            host_uds,
            guest_cid,
            guest_port,
            identity: None,
        })
    }

    /// Capture the current filesystem identity of the relay socket for pinning.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn capture_identity(&mut self) -> Result<(), VsockEndpointError> {
        self.identity = Some(read_identity(&self.host_uds)?);
        Ok(())
    }

    /// Verify that the relay still exists and matches the pinned identity.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate_current(&self) -> Result<(), VsockEndpointError> {
        let current = read_identity(&self.host_uds)?;
        if let Some(identity) = self.identity {
            if identity != current {
                return Err(VsockEndpointError::Stale);
            }
        }
        Ok(())
    }
}

/// Errors from the RFB1 vsock client transport and RPC surface.
#[derive(Debug, thiserror::Error)]
pub enum VsockClientError {
    /// Endpoint validation (CID/port/path/identity) failed.
    #[error("vsock endpoint validation failed: {0}")]
    Endpoint(#[from] VsockEndpointError),
    /// The underlying transport (socket, handshake, frame) failed.
    #[error("vsock transport failed: {0}")]
    Io(#[from] std::io::Error),
    /// The Firecracker relay rejected the `CONNECT` preamble.
    #[error("vsock CONNECT rejected: {0}")]
    ConnectRejected(String),
    /// An operation exceeded the configured timeout.
    #[error("vsock operation timed out")]
    Timeout,
    /// The session's byte stream is no longer frame-aligned because a
    /// timed-out operation dropped a partially-read frame; the session must
    /// be recreated.
    #[error("session stream is out of sync after a timed-out operation: {0}")]
    Desync(String),
    /// The guest returned an RPC-level error.
    #[error("guest RPC failed: {0}")]
    Remote(String),
    /// A caller-supplied argument was invalid (empty args, bad path, bad size).
    #[error("invalid guest RPC argument: {0}")]
    InvalidArgument(String),
    /// The guest reply was not valid JSON.
    #[error("guest RPC returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Eval is not supported over the RFB1 wire protocol.
    #[error("guest eval is unsupported over RFB1")]
    UnsupportedEval,
}

#[cfg(unix)]
use tokio::net::UnixStream;

/// An async RFB1 client over Firecracker's host-side vsock UDS relay.
#[cfg(unix)]
pub struct VsockClient {
    stream: UnixStream,
    codec: FrameCodec,
    timeout: Duration,
}

#[cfg(unix)]
impl VsockClient {
    /// Build a client around an existing stream for transport tests and
    /// embedded Unix-stream transports. The stream is assumed to be connected.
    #[doc(hidden)]
    pub fn from_stream_for_test(stream: UnixStream, timeout: Duration) -> Self {
        Self {
            stream,
            codec: FrameCodec::default(),
            timeout,
        }
    }

    fn remaining(deadline: Instant) -> Result<Duration, VsockClientError> {
        deadline
            .checked_duration_since(Instant::now())
            .ok_or(VsockClientError::Timeout)
    }

    /// Dial the relay UDS, speak the `CONNECT <guest-port>` preamble, and validate
    /// the `OK <host-port>` response, all bounded by `timeout`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn connect(
        endpoint: VsockEndpoint,
        codec: FrameCodec,
        timeout: Duration,
    ) -> Result<Self, VsockClientError> {
        endpoint.validate_current()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(VsockClientError::Timeout)?;
        let mut stream = tokio::time::timeout(
            Self::remaining(deadline)?,
            UnixStream::connect(&endpoint.host_uds),
        )
        .await
        .map_err(|_| VsockClientError::Timeout)??;
        crate::vsock_relay::perform_relay_handshake(&mut stream, endpoint.guest_port, deadline)
            .await
            .map_err(|error| match error {
                crate::vsock_relay::RelayHandshakeError::Timeout => VsockClientError::Timeout,
                other => VsockClientError::ConnectRejected(other.to_string()),
            })?;
        Ok(Self {
            stream,
            codec,
            timeout,
        })
    }

    /// Write one control message as a length-delimited RFB1 frame.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn send_control(
        &mut self,
        message: &ControlMessage,
        sequence: u64,
    ) -> Result<(), VsockClientError> {
        let fut = write_frame(
            &mut self.stream,
            &self.codec,
            control_type(message),
            sequence,
            message,
        );
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or(VsockClientError::Timeout)?;
        tokio::time::timeout(Self::remaining(deadline)?, fut)
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "vsock write timeout")
            })??;
        Ok(())
    }

    /// Read one length-delimited RFB1 frame, bounded by the session timeout.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn recv(&mut self) -> Result<(Frame, RuntimeMessage), VsockClientError> {
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or(VsockClientError::Timeout)?;
        Ok(tokio::time::timeout(
            Self::remaining(deadline)?,
            read_frame(&mut self.stream, &self.codec),
        )
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "vsock read timeout"))??)
    }

    /// Consume the client, returning the underlying Unix stream.
    pub fn into_inner(self) -> UnixStream {
        self.stream
    }
}

/// A connected, negotiated host-side RFB1 session.
///
/// `HostSession` is the single owner of a long-lived connection. It performs
/// `Hello` and `Capabilities` exactly once, allocates request sequences, and
/// provides the matching orderly shutdown operation. Callers should use this
/// type instead of repeating the handshake around [`VsockClient`].
///
/// ```no_run
/// # #[cfg(unix)] async fn example(endpoint: rfb_runtime::host_vsock::VsockEndpoint) {
/// use rfb_runtime::host_vsock::HostClient;
/// let mut session = HostClient::new(endpoint, rfb_runtime::codec::FrameCodec::default(),
///     std::time::Duration::from_secs(5)).connect().await.unwrap();
/// let sequence = session.next_sequence();
/// assert!(sequence > 2);
/// session.shutdown().await.unwrap();
/// # }
/// ```
#[cfg(unix)]
pub struct HostSession {
    client: VsockClient,
    next_sequence: u64,
    /// Set when a timed-out operation dropped a partially-read frame: the
    /// byte stream can no longer be frame-aligned, so every further operation
    /// fails closed instead of decoding garbage.
    poisoned: bool,
}

/// Shareable handle for a negotiated host session. Operations are serialized
/// through one mutex so sequence numbers and the underlying stream remain
/// consistent across runtime lifecycle and execution callers.
#[cfg(unix)]
#[derive(Clone)]
pub struct SharedHostSession(std::sync::Arc<tokio::sync::Mutex<HostSession>>);

#[cfg(unix)]
impl HostSession {
    /// Convert this session into a shareable, single-owner execution handle.
    pub fn shared(self) -> SharedHostSession {
        SharedHostSession(std::sync::Arc::new(tokio::sync::Mutex::new(self)))
    }
}

#[cfg(unix)]
impl SharedHostSession {
    /// Wrap an already-connected transport as a negotiated session without
    /// re-running the RFB1 handshake. Used by embedded transports and tests
    /// where the peer is known to be ready (e.g. a Unix stream pair).
    pub fn from_stream(stream: tokio::net::UnixStream) -> Self {
        Self::from_stream_with(stream, Duration::from_secs(10))
    }

    /// [`SharedHostSession::from_stream`] with an explicit per-request timeout.
    pub fn from_stream_with(stream: tokio::net::UnixStream, timeout: Duration) -> Self {
        let client = VsockClient {
            stream,
            codec: FrameCodec::default(),
            timeout,
        };
        SharedHostSession(std::sync::Arc::new(tokio::sync::Mutex::new(HostSession {
            client,
            next_sequence: 3,
            poisoned: false,
        })))
    }
}

#[cfg(unix)]
impl SharedHostSession {
    /// Shut down the shared negotiated connection.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn shutdown(&self) -> Result<(), VsockClientError> {
        self.0.lock().await.shutdown().await
    }

    /// Run a turn on the shared connection: send `StartTurn` and pump typed
    /// events until a terminal event, collecting `terminal.output` into the
    /// result. The mutex keeps sequence allocation and the stream consistent
    /// across runtime lifecycle and execution callers.
    async fn turn(&self, cwd: &str, prompt: String) -> Result<serde_json::Value, VsockClientError> {
        validate_guest_cwd(cwd)?;
        let mut session = self.0.lock().await;
        let sequence = session.next_sequence();
        let deadline = Instant::now()
            .checked_add(session.timeout())
            .ok_or(VsockClientError::Timeout)?;
        let session_id = format!("rfb-web-{}", Uuid::now_v7());
        let request_id = format!("rfb-web-{}", Uuid::now_v7());
        session
            .send(
                &ControlMessage::StartTurn(crate::session::SessionRequest {
                    session_id: session_id.clone(),
                    request_id: request_id.clone(),
                    prompt,
                }),
                sequence,
            )
            .await?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        loop {
            if Instant::now() >= deadline {
                return Err(VsockClientError::Timeout);
            }
            let (frame, message) = session.recv().await?;
            match &message {
                RuntimeMessage::Event(_) if frame.message_type != MessageType::Event => {
                    return Err(VsockClientError::Remote("invalid turn event frame".into()))
                }
                RuntimeMessage::Error { .. } if frame.message_type != MessageType::Error => {
                    return Err(VsockClientError::Remote("invalid turn error frame".into()))
                }
                _ if frame.message_type != MessageType::Event && frame.sequence != sequence => {
                    return Err(VsockClientError::Remote(
                        "invalid turn response sequence".into(),
                    ))
                }
                _ => {}
            }
            match message {
                RuntimeMessage::Error {
                    request_id: rid,
                    message,
                } if rid.is_empty() || rid == request_id => {
                    return Err(VsockClientError::Remote(message))
                }
                RuntimeMessage::Event(event)
                    if frame.message_type == MessageType::Event
                        && event.session_id == session_id =>
                {
                    if event.kind == "terminal.output" {
                        let terminal: crate::session::TerminalEvent =
                            serde_json::from_slice(&event.payload).map_err(|error| {
                                VsockClientError::Remote(format!(
                                    "invalid terminal event payload: {error}"
                                ))
                            })?;
                        match terminal.stream {
                            crate::session::TerminalStream::Stdout => {
                                append_capped(&mut stdout, &terminal.data)
                            }
                            crate::session::TerminalStream::Stderr => {
                                append_capped(&mut stderr, &terminal.data)
                            }
                        }
                    }
                    if matches!(
                        event.kind.as_str(),
                        "turn.completed" | "turn.failed" | "turn.cancelled"
                    ) {
                        if event.kind != "turn.completed" {
                            return Err(VsockClientError::Remote(
                                String::from_utf8_lossy(&event.payload).into_owned(),
                            ));
                        }
                        let mut result: serde_json::Value = serde_json::from_slice(&event.payload)?;
                        if let Some(object) = result.as_object_mut() {
                            object.insert("stdout".into(), serde_json::Value::String(stdout));
                            object.insert("stderr".into(), serde_json::Value::String(stderr));
                        }
                        return Ok(result);
                    }
                }
                _ => {}
            }
        }
    }

    /// Execute a command in the guest workspace on the shared connection.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn exec(
        &self,
        guest_cwd: &str,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<serde_json::Value, VsockClientError> {
        if args.is_empty() {
            return Err(VsockClientError::InvalidArgument(
                "exec args must not be empty".into(),
            ));
        }
        self.turn(
            guest_cwd,
            serde_json::to_string(&serde_json::json!({
                "op": "exec", "args": args, "cwd": guest_cwd, "timeout_secs": timeout_secs
            }))?,
        )
        .await
    }

    /// Shared-session eval is deliberately fail-closed until negotiated.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn eval(
        &self,
        _cwd: &str,
        _code: impl Into<String>,
    ) -> Result<serde_json::Value, VsockClientError> {
        Err(VsockClientError::UnsupportedEval)
    }

    /// Run a whitelisted structured guest tool (`ls`, `find`, `grep`) on the
    /// shared connection.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn execute_tool(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, VsockClientError> {
        match tool {
            "ls" | "find" | "grep" => {}
            other => {
                return Err(VsockClientError::InvalidArgument(format!(
                    "unsupported guest tool: {other}"
                )))
            }
        }
        validate_tool_args(tool, &args)?;
        self.turn(
            ".",
            serde_json::to_string(&serde_json::json!({"op": tool, "args": args}))?,
        )
        .await
    }

    /// Read a guest workspace file over the shared connection.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn read_workspace_file(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, VsockClientError> {
        validate_relative_path(path)?;
        if max_bytes == 0 || max_bytes > 16 * 1024 * 1024 {
            return Err(VsockClientError::InvalidArgument(
                "invalid file size limit".into(),
            ));
        }
        let request_id = format!("rfb-web-{}", Uuid::now_v7());
        let mut session = self.0.lock().await;
        let sequence = session.next_sequence();
        session
            .send(
                &ControlMessage::ReadWorkspaceFile(crate::session::FileReadRequest {
                    request_id: request_id.clone(),
                    path: path.into(),
                    max_bytes,
                }),
                sequence,
            )
            .await?;
        let (frame, message) = session.recv().await?;
        if frame.sequence != sequence {
            return Err(VsockClientError::Remote(
                "invalid file response sequence".into(),
            ));
        }
        match message {
            RuntimeMessage::FileContent {
                request_id: rid,
                path: response_path,
                content,
            } if frame.message_type == MessageType::FileContent
                && rid == request_id
                && response_path == path =>
            {
                Ok(content)
            }
            RuntimeMessage::Error {
                request_id: rid,
                message,
            } if frame.message_type == MessageType::Error
                && (rid.is_empty() || rid == request_id) =>
            {
                Err(VsockClientError::Remote(message))
            }
            _ => Err(VsockClientError::Remote(
                "invalid file content response frame".into(),
            )),
        }
    }

    /// Write a guest workspace file over the shared connection.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn write_workspace_file(
        &self,
        path: &str,
        content: Vec<u8>,
    ) -> Result<(), VsockClientError> {
        validate_relative_path(path)?;
        if content.len() > 16 * 1024 * 1024 {
            return Err(VsockClientError::InvalidArgument(
                "file content exceeds size limit".into(),
            ));
        }
        let request_id = format!("rfb-web-{}", Uuid::now_v7());
        let mut session = self.0.lock().await;
        let sequence = session.next_sequence();
        session
            .send(
                &ControlMessage::WriteWorkspaceFile(crate::session::FileWriteRequest {
                    request_id: request_id.clone(),
                    path: path.into(),
                    content,
                }),
                sequence,
            )
            .await?;
        loop {
            let (frame, message) = session.recv().await?;
            if frame.sequence != sequence {
                return Err(VsockClientError::Remote(
                    "invalid write response sequence".into(),
                ));
            }
            match message {
                RuntimeMessage::WriteAck {
                    request_id: rid,
                    path: response_path,
                } if frame.message_type == MessageType::WriteAck
                    && rid == request_id
                    && response_path == path =>
                {
                    return Ok(())
                }
                RuntimeMessage::Error {
                    request_id: rid,
                    message,
                } if frame.message_type == MessageType::Error
                    && (rid.is_empty() || rid == request_id) =>
                {
                    return Err(VsockClientError::Remote(message))
                }
                RuntimeMessage::WriteAck { .. } => {
                    return Err(VsockClientError::Remote(
                        "invalid write acknowledgement frame".into(),
                    ))
                }
                _ => {}
            }
        }
    }
}

/// Configuration and connector for a negotiated host RFB1 session.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct HostClient {
    /// Validated host-side vsock endpoint to connect to.
    pub endpoint: VsockEndpoint,
    /// Codec used for RFB1 frames after the handshake.
    pub codec: FrameCodec,
    /// Timeout applied to connection, handshake, and frame operations.
    pub timeout: Duration,
}

#[cfg(unix)]
impl HostClient {
    /// Create a connector. [`HostClient::connect`] performs the complete
    /// transport and protocol handshake on one connection.
    pub fn new(endpoint: VsockEndpoint, codec: FrameCodec, timeout: Duration) -> Self {
        Self {
            endpoint,
            codec,
            timeout,
        }
    }

    /// Connect and negotiate Hello plus Capabilities exactly once.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn connect(&self) -> Result<HostSession, VsockClientError> {
        let mut client =
            VsockClient::connect(self.endpoint.clone(), self.codec.clone(), self.timeout).await?;
        client
            .send_control(
                &ControlMessage::Hello {
                    protocol_version: crate::PROTOCOL_VERSION,
                },
                1,
            )
            .await?;
        let (frame, response) = client.recv().await?;
        if frame.message_type != MessageType::HelloAck
            || frame.sequence != 1
            || !matches!(response, RuntimeMessage::HelloAck { protocol_version } if protocol_version == crate::PROTOCOL_VERSION)
        {
            return Err(VsockClientError::Remote(format!(
                "guest rejected RFB1 Hello: {response:?}"
            )));
        }
        client
            .send_control(
                &ControlMessage::Capabilities {
                    session_per_vm: true,
                    writable_workspace: true,
                },
                2,
            )
            .await?;
        let (frame, response) = client.recv().await?;
        if frame.message_type != MessageType::Capabilities
            || frame.sequence != 2
            || !matches!(
                response,
                RuntimeMessage::Capabilities {
                    session_per_vm: true,
                    writable_workspace: true
                }
            )
        {
            return Err(VsockClientError::Remote(format!(
                "guest rejected RFB1 capabilities: {response:?}"
            )));
        }
        Ok(HostSession {
            client,
            next_sequence: 3,
            poisoned: false,
        })
    }
}

#[cfg(unix)]
const POISON_MESSAGE: &str = "a previous operation timed out mid-frame; recreate the session";

/// Append stream output up to a per-stream cap. A chatty guest must not make
/// the host buffer unbounded output for the whole turn deadline; later chunks
/// are dropped (the turn result stays valid).
#[cfg(unix)]
fn append_capped(target: &mut String, data: &str) {
    const CAP: usize = 1024 * 1024;
    if target.len() >= CAP {
        return;
    }
    let remaining = CAP - target.len();
    if data.len() <= remaining {
        target.push_str(data);
        return;
    }
    let mut cut = remaining;
    while cut > 0 && !data.is_char_boundary(cut) {
        cut -= 1;
    }
    target.push_str(&data[..cut]);
}

#[cfg(unix)]
fn is_stream_timeout(error: &VsockClientError) -> bool {
    matches!(error, VsockClientError::Io(io_error) if io_error.kind() == std::io::ErrorKind::TimedOut)
        || matches!(error, VsockClientError::Timeout)
}

#[cfg(unix)]
impl HostSession {
    /// Returns the per-request timeout for this session.
    pub fn timeout(&self) -> Duration {
        self.client.timeout
    }

    /// Allocate the next request sequence number.
    pub fn next_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        sequence
    }

    /// Send one control request using the supplied sequence.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn send(
        &mut self,
        message: &ControlMessage,
        sequence: u64,
    ) -> Result<(), VsockClientError> {
        if self.poisoned {
            return Err(VsockClientError::Desync(POISON_MESSAGE.into()));
        }
        if let Err(error) = self.client.send_control(message, sequence).await {
            if is_stream_timeout(&error) {
                // A write timeout can also leave the peer mid-frame, so the
                // stream is no longer trustworthy.
                self.poisoned = true;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Receive one response, bounded by the configured session timeout.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn recv(&mut self) -> Result<(Frame, RuntimeMessage), VsockClientError> {
        if self.poisoned {
            return Err(VsockClientError::Desync(POISON_MESSAGE.into()));
        }
        match self.client.recv().await {
            Err(error) if is_stream_timeout(&error) => {
                // The timeout dropped a possibly partially-read frame; the
                // stream can no longer be frame-aligned.
                self.poisoned = true;
                Err(error)
            }
            other => other,
        }
    }

    /// Request orderly shutdown and validate its acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn shutdown(&mut self) -> Result<(), VsockClientError> {
        let sequence = self.next_sequence();
        self.send(&ControlMessage::Shutdown, sequence).await?;
        let (frame, response) = self.recv().await?;
        if frame.message_type != MessageType::Shutdown
            || frame.sequence != sequence
            || !matches!(response, RuntimeMessage::ShutdownAck)
        {
            return Err(VsockClientError::Remote(format!(
                "invalid shutdown response: frame_type={:?}, sequence={}, message={response:?}",
                frame.message_type, frame.sequence
            )));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn control_type(message: &ControlMessage) -> MessageType {
    match message {
        ControlMessage::Hello { .. } => MessageType::Hello,
        ControlMessage::Capabilities { .. } => MessageType::Capabilities,
        ControlMessage::StartTurn(_) => MessageType::StartTurn,
        ControlMessage::Cancel { .. } => MessageType::CancelTurn,
        ControlMessage::ReadWorkspaceFile(_) => MessageType::ReadWorkspaceFile,
        ControlMessage::ReadHostFile(_) => MessageType::ReadHostFile,
        ControlMessage::WriteWorkspaceFile(_) => MessageType::WriteWorkspaceFile,
        ControlMessage::Shutdown => MessageType::Shutdown,
    }
}

/// A cloneable, one-operation-per-connection RFB1 guest RPC client.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct VsockGuestClient {
    /// Validated host-side vsock endpoint to connect to.
    pub endpoint: VsockEndpoint,
    /// Codec used for RFB1 frames after the handshake.
    pub codec: FrameCodec,
    /// Timeout applied to each one-operation connection.
    pub timeout: Duration,
}

#[cfg(unix)]
impl VsockGuestClient {
    /// Build a one-operation-per-connection guest RPC client.
    pub fn new(endpoint: VsockEndpoint, codec: FrameCodec, timeout: Duration) -> Self {
        Self {
            endpoint,
            codec,
            timeout,
        }
    }

    async fn begin(&self) -> Result<VsockClient, VsockClientError> {
        let mut client =
            VsockClient::connect(self.endpoint.clone(), self.codec.clone(), self.timeout).await?;
        client
            .send_control(
                &ControlMessage::Hello {
                    protocol_version: crate::PROTOCOL_VERSION,
                },
                1,
            )
            .await?;
        let (frame, response) = client.recv().await?;
        match (frame.message_type, frame.sequence, response) {
            (MessageType::HelloAck, 1, RuntimeMessage::HelloAck { protocol_version })
                if protocol_version == crate::PROTOCOL_VERSION =>
            {
                Ok(client)
            }
            (_, _, response) => Err(VsockClientError::Remote(format!(
                "guest rejected RFB1 handshake: {response:?}"
            ))),
        }
    }

    async fn finish(
        &self,
        client: &mut VsockClient,
        sequence: u64,
    ) -> Result<(), VsockClientError> {
        client
            .send_control(&ControlMessage::Shutdown, sequence)
            .await?;
        let (frame, response) = client.recv().await?;
        if frame.message_type != MessageType::Shutdown
            || frame.sequence != sequence
            || !matches!(response, RuntimeMessage::ShutdownAck)
        {
            return Err(VsockClientError::Remote(format!(
                "invalid shutdown response: frame_type={:?}, sequence={}, message={response:?}",
                frame.message_type, frame.sequence
            )));
        }
        Ok(())
    }

    async fn finish_result<T>(
        &self,
        client: &mut VsockClient,
        result: Result<T, VsockClientError>,
    ) -> Result<T, VsockClientError> {
        match result {
            Ok(value) => {
                self.finish(client, 3).await?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.finish(client, 3).await;
                Err(error)
            }
        }
    }

    async fn turn(&self, cwd: &str, prompt: String) -> Result<serde_json::Value, VsockClientError> {
        validate_guest_cwd(cwd)?;
        let mut client = self.begin().await?;
        let session_id = format!("rfb-web-{}", Uuid::now_v7());
        let request_id = format!("rfb-web-{}", Uuid::now_v7());
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or(VsockClientError::Timeout)?;
        let result = async {
            client
                .send_control(
                    &ControlMessage::StartTurn(crate::session::SessionRequest {
                        session_id: session_id.clone(),
                        request_id: request_id.clone(),
                        prompt,
                    }),
                    2,
                )
                .await?;
            let mut stdout = String::new();
            let mut stderr = String::new();
            loop {
                if Instant::now() >= deadline {
                    return Err(VsockClientError::Timeout);
                }
                let (frame, message) = client.recv().await?;
                match &message {
                    RuntimeMessage::Event(_) if frame.message_type != MessageType::Event => {
                        return Err(VsockClientError::Remote("invalid turn event frame".into()))
                    }
                    RuntimeMessage::Error { .. } if frame.message_type != MessageType::Error => {
                        return Err(VsockClientError::Remote("invalid turn error frame".into()))
                    }
                    _ if frame.message_type != MessageType::Event && frame.sequence != 2 => {
                        return Err(VsockClientError::Remote(
                            "invalid turn response sequence".into(),
                        ))
                    }
                    _ => {}
                }
                match message {
                    RuntimeMessage::Error {
                        request_id: rid,
                        message,
                    } if rid.is_empty() || rid == request_id => {
                        return Err(VsockClientError::Remote(message))
                    }
                    RuntimeMessage::Event(event)
                        if frame.message_type == MessageType::Event
                            && event.session_id == session_id =>
                    {
                        if event.kind == "terminal.output" {
                            let terminal: crate::session::TerminalEvent =
                                serde_json::from_slice(&event.payload).map_err(|error| {
                                    VsockClientError::Remote(format!(
                                        "invalid terminal event payload: {error}"
                                    ))
                                })?;
                            match terminal.stream {
                                crate::session::TerminalStream::Stdout => {
                                    append_capped(&mut stdout, &terminal.data)
                                }
                                crate::session::TerminalStream::Stderr => {
                                    append_capped(&mut stderr, &terminal.data)
                                }
                            }
                        }
                        if matches!(
                            event.kind.as_str(),
                            "turn.completed" | "turn.failed" | "turn.cancelled"
                        ) {
                            if event.kind != "turn.completed" {
                                return Err(VsockClientError::Remote(
                                    String::from_utf8_lossy(&event.payload).into_owned(),
                                ));
                            }
                            let mut result: serde_json::Value =
                                serde_json::from_slice(&event.payload)?;
                            if let Some(object) = result.as_object_mut() {
                                object.insert("stdout".into(), serde_json::Value::String(stdout));
                                object.insert("stderr".into(), serde_json::Value::String(stderr));
                            }
                            return Ok(result);
                        }
                    }
                    _ => {}
                }
            }
        }
        .await;
        self.finish_result(&mut client, result).await
    }

    /// Execute a command in the guest workspace. `args` must be non-empty and
    /// `guest_cwd` must be a relative workspace path.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn exec(
        &self,
        guest_cwd: &str,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<serde_json::Value, VsockClientError> {
        if args.is_empty() {
            return Err(VsockClientError::InvalidArgument(
                "exec args must not be empty".into(),
            ));
        }
        self.turn(guest_cwd, serde_json::to_string(&serde_json::json!({"op":"exec","args":args,"cwd":guest_cwd,"timeout_secs":timeout_secs}))?).await
    }

    /// Evaluate code in the guest. Always unsupported over the RFB1 wire
    /// protocol; services use the structured exec surface instead.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn eval(
        &self,
        _guest_cwd: &str,
        _code: impl Into<String>,
    ) -> Result<serde_json::Value, VsockClientError> {
        Err(VsockClientError::UnsupportedEval)
    }

    /// Run a whitelisted structured guest tool (`ls`, `find`, `grep`).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn execute_tool(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, VsockClientError> {
        match tool {
            "ls" | "find" | "grep" => {}
            other => {
                return Err(VsockClientError::InvalidArgument(format!(
                    "unsupported guest tool: {other}"
                )))
            }
        }
        validate_tool_args(tool, &args)?;
        self.turn(
            ".",
            serde_json::to_string(&serde_json::json!({"op":tool,"args":args}))?,
        )
        .await
    }

    /// Read a workspace file with a strict size limit and a relative-path check.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn read_workspace_file(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, VsockClientError> {
        validate_relative_path(path)?;
        if max_bytes == 0 || max_bytes > 16 * 1024 * 1024 {
            return Err(VsockClientError::InvalidArgument(
                "invalid file size limit".into(),
            ));
        }
        let request_id = format!("rfb-web-{}", Uuid::now_v7());
        let mut client = self.begin().await?;
        let result = async {
            client
                .send_control(
                    &ControlMessage::ReadWorkspaceFile(crate::session::FileReadRequest {
                        request_id: request_id.clone(),
                        path: path.into(),
                        max_bytes,
                    }),
                    2,
                )
                .await?;
            let (frame, message) = client.recv().await?;
            if frame.sequence != 2 {
                return Err(VsockClientError::Remote(
                    "invalid file response sequence".into(),
                ));
            }
            match message {
                RuntimeMessage::FileContent {
                    request_id: rid,
                    path: response_path,
                    content,
                } if frame.message_type == MessageType::FileContent
                    && rid == request_id
                    && response_path == path =>
                {
                    Ok(content)
                }
                RuntimeMessage::Error {
                    request_id: rid,
                    message,
                } if frame.message_type == MessageType::Error
                    && (rid.is_empty() || rid == request_id) =>
                {
                    Err(VsockClientError::Remote(message))
                }
                _ => Err(VsockClientError::Remote(
                    "invalid file content response frame".into(),
                )),
            }
        }
        .await;
        self.finish_result(&mut client, result).await
    }

    /// Write a workspace file (bounded at 16 MiB, workspace-relative path).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn write_workspace_file(
        &self,
        path: &str,
        content: Vec<u8>,
    ) -> Result<(), VsockClientError> {
        validate_relative_path(path)?;
        if content.len() > 16 * 1024 * 1024 {
            return Err(VsockClientError::InvalidArgument(
                "file content exceeds size limit".into(),
            ));
        }
        let request_id = format!("rfb-web-{}", Uuid::now_v7());
        let mut client = self.begin().await?;
        let result = async {
            client
                .send_control(
                    &ControlMessage::WriteWorkspaceFile(crate::session::FileWriteRequest {
                        request_id: request_id.clone(),
                        path: path.into(),
                        content,
                    }),
                    2,
                )
                .await?;
            loop {
                let (frame, message) = client.recv().await?;
                if frame.sequence != 2 {
                    return Err(VsockClientError::Remote(
                        "invalid write response sequence".into(),
                    ));
                }
                match message {
                    RuntimeMessage::WriteAck {
                        request_id: rid,
                        path: response_path,
                    } if frame.message_type == MessageType::WriteAck
                        && frame.sequence == 2
                        && rid == request_id
                        && response_path == path =>
                    {
                        return Ok(())
                    }
                    RuntimeMessage::Error {
                        request_id: rid,
                        message,
                    } if frame.message_type == MessageType::Error
                        && (rid.is_empty() || rid == request_id) =>
                    {
                        return Err(VsockClientError::Remote(message))
                    }
                    RuntimeMessage::WriteAck { .. } => {
                        return Err(VsockClientError::Remote(
                            "invalid write acknowledgement frame".into(),
                        ))
                    }
                    _ => {}
                }
            }
        }
        .await;
        self.finish_result(&mut client, result).await
    }
}

#[cfg(unix)]
fn validate_guest_cwd(path: &str) -> Result<(), VsockClientError> {
    if path.is_empty()
        || path.as_bytes().contains(&0)
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.split(['/', '\\']).any(|p| p == "..")
        || (path.len() >= 2 && path.as_bytes()[1] == b':')
    {
        return Err(VsockClientError::InvalidArgument(
            "guest cwd must be a relative path without .., drive prefix, or NUL".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_relative_path(path: &str) -> Result<(), VsockClientError> {
    if path.is_empty()
        || path.len() > 4096
        || path.as_bytes().contains(&0)
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.split(['/', '\\']).any(|p| p == "..")
        || (path.len() >= 2 && path.as_bytes()[1] == b':')
    {
        return Err(VsockClientError::InvalidArgument(
            "workspace path must be relative without .., drive prefix, or NUL".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_tool_args(tool: &str, args: &serde_json::Value) -> Result<(), VsockClientError> {
    let object = args.as_object().ok_or_else(|| {
        VsockClientError::InvalidArgument("tool args must be a JSON object".into())
    })?;
    for key in object.keys() {
        let allowed = match tool {
            "ls" => ["path", "max_results"].as_slice(),
            "find" => ["path", "pattern", "max_results"].as_slice(),
            _ => ["path", "pattern", "max_results", "max_bytes"].as_slice(),
        };
        if !allowed.contains(&key.as_str()) {
            return Err(VsockClientError::InvalidArgument(format!(
                "unknown {tool} argument: {key}"
            )));
        }
    }
    if let Some(path) = object.get("path").and_then(|v| v.as_str()) {
        validate_relative_path(path)?;
    }
    for key in ["pattern"] {
        if let Some(value) = object.get(key) {
            let s = value.as_str().ok_or_else(|| {
                VsockClientError::InvalidArgument(format!("{key} must be a string"))
            })?;
            if s.is_empty() || s.len() > 1024 {
                return Err(VsockClientError::InvalidArgument(format!("invalid {key}")));
            }
        }
    }
    for key in ["max_results", "max_bytes"] {
        if let Some(value) = object.get(key) {
            let n = value.as_u64().ok_or_else(|| {
                VsockClientError::InvalidArgument(format!("{key} must be an integer"))
            })?;
            if n == 0 || n > if key == "max_bytes" { 50 * 1024 } else { 1000 } {
                return Err(VsockClientError::InvalidArgument(format!("invalid {key}")));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_identity(path: &std::path::Path) -> Result<EndpointIdentity, VsockEndpointError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata =
        std::fs::metadata(path).map_err(|e| VsockEndpointError::Unavailable(e.to_string()))?;
    if !metadata.file_type().is_socket() {
        return Err(VsockEndpointError::Unavailable(
            "endpoint is not a Unix socket".into(),
        ));
    }
    Ok(EndpointIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        created: metadata.created().ok(),
    })
}

#[cfg(not(unix))]
fn read_identity(_path: &std::path::Path) -> Result<EndpointIdentity, VsockEndpointError> {
    Err(VsockEndpointError::Unsupported)
}
