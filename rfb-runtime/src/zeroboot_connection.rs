//! ZeroBoot V1 (ZBRT) guest connection: ZBRT frames driven through the RFB1
//! [`crate::runtime_service::RuntimeService`] and
//! [`crate::workspace_executor::WorkspaceGuestExecutor`] semantics.
//!
//! The connection speaks the ZBRT binary wire protocol (magic `ZBRT`, version
//! 1, 28-byte header) and maps its request types onto the existing
//! `rfb-runtime` execution/state-machine machinery:
//!
//! - `Hello` -> RFB1 `Hello` handshake and a ZBRT `HelloAck` reply.
//! - `Execute` -> a structured workspace `exec` turn that streams `Output`
//!   frames live as the child writes, then a single terminal `Exit` frame
//!   (exactly once, even on cancellation or failure).
//! - `Cancel` -> the runtime's active-turn cancellation: the shared flag makes
//!   the workspace executor terminate the child's process group, and the
//!   worker emits exactly one terminal frame for the request.
//! - `Health` -> `HealthAck`.
//! - `Fs` -> the structured workspace filesystem RPC (`FsResult` or `Error`).
//! - Unknown kinds fail closed with a ZBRT `Error` frame.
//!
//! Each connection tracks its submitted requests in a request map keyed by the
//! 128-bit wire request id so duplicate Execute ids are rejected, Cancel can
//! target the exact active request, and every request receives exactly one
//! terminal frame.
//!
//! No TCP, serial, or V2 wire is introduced.

use crate::runtime_service::{GuestEvent, GuestExecutor, RuntimeService};
use crate::session::{
    ControlMessage, RuntimeMessage, SessionRequest, TerminalEvent, TerminalStream,
};
use crate::zeroboot_protocol::{
    read_frame_async, write_frame_async, Cancel, Error as ZbrtError, Execute, Exit, Frame, Health,
    Hello, HelloAck, Kind, Output,
};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex};

/// ZeroBoot V1 capabilities this guest implements end-to-end. Execute, stream,
/// deadline, health, cancel and filesystem are all serviced by the workspace
/// executor behind [`RuntimeService`]; nothing advertised here fails closed in
/// practice.
///
/// Single source of truth lives in [`crate::zeroboot_protocol::
/// ZBRT_V1_CAPABILITIES`]; the host Hello, the rootfs marker file, and
/// `rfb-cli zeroboot verify` all reference the same list.
pub use crate::zeroboot_protocol::ZBRT_V1_CAPABILITIES;

/// Session identity used for every ZBRT connection. Each connection owns its
/// own [`RuntimeService`], so sharing one session label does not collide.
const SESSION_ID: &str = "zeroboot";

/// Capacity of the ordered worker message channel shared by the streaming
/// output sink and the terminal result.
const WORKER_CHANNEL_CAPACITY: usize = 64;

/// Upper bound on the disconnect teardown wait for an in-flight turn's
/// terminal result. The executor tears the process group down once it observes
/// the cancellation flag, so this only guards against a pathological executor
/// stall; the VM teardown reclaims anything still alive after this expires.
const ACTIVE_TEARDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type TurnResult = (
    [u8; 16],
    String,
    String,
    Result<Vec<GuestEvent>, String>,
    Box<dyn GuestExecutor>,
);

/// Ordered worker messages for one active turn. Every `Output` chunk queued by
/// the sink strictly precedes the single `Terminal`, so the connection can
/// forward live output and then emit exactly one terminal frame. Output
/// messages carry the originating request id so straggler chunks from a
/// cancelled turn can never be forwarded under a later request.
enum WorkerMessage {
    Output {
        request_id: [u8; 16],
        event: GuestEvent,
    },
    Terminal(TurnResult),
}

/// One submitted Execute request.
struct RequestEntry {
    /// Hex request id used with the runtime service identity validation.
    request_id: String,
    /// Whether a terminal frame has already been sent for this request.
    terminated: bool,
}

/// How many terminated request entries are retained for idempotent-cancel
/// replay detection. Beyond this bound the oldest terminated entries are
/// evicted; a cancel for an evicted request degrades to the normal
/// acknowledge path (documented in `handle_cancel`).
const TERMINATED_RETENTION: usize = 64;

/// Serve one already-accepted ZBRT connection with its own [`RuntimeService`]
/// (workspace executor). Generic over the stream so tests can use in-memory
/// transports and the vsock listener can hand real split stream halves.
pub async fn serve<R, W>(
    reader: R,
    writer: W,
    service: Arc<Mutex<RuntimeService>>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = reader;
    let mut writer = writer;
    // The ZBRT host provider/CLI never send a wire Hello before Execute, so
    // auto-negotiate protocol readiness with the runtime service itself.
    {
        let mut runtime = service.lock().await;
        let _ = runtime.handle(ControlMessage::Hello {
            protocol_version: 1,
        });
    }

    let (worker_tx, mut worker_rx) = mpsc::channel::<WorkerMessage>(WORKER_CHANNEL_CAPACITY);
    let mut worker_active = false;
    let mut active_id: Option<[u8; 16]> = None;
    let mut requests: HashMap<[u8; 16], RequestEntry> = HashMap::new();
    // Insertion order of terminated entries, for oldest-first eviction past
    // TERMINATED_RETENTION so the bookkeeping stays bounded per connection.
    let mut terminated_order: std::collections::VecDeque<[u8; 16]> =
        std::collections::VecDeque::new();

    let io_result = loop {
        tokio::select! {
            message = worker_rx.recv(), if worker_active => {
                match message {
                    Some(WorkerMessage::Output { request_id, event }) => {
                        // Only forward output that belongs to the currently
                        // active request; straggler chunks from a cancelled
                        // turn are dropped once its terminal has been sent.
                        if active_id == Some(request_id) {
                            forward_event(&mut writer, request_id, &event).await?;
                        }
                    }
                    Some(WorkerMessage::Terminal((request_id, session_id, request_id_str, result, executor))) => {
                        worker_active = false;
                        active_id = None;
                        let responses = {
                            let mut runtime = service.lock().await;
                            runtime.complete_turn(session_id, request_id_str, result, executor)
                        };
                        if let Some(frame) = terminal_frame(request_id, &responses) {
                            write_frame_async(&mut writer, &frame).await?;
                        }
                        if let Some(entry) = requests.get_mut(&request_id) {
                            entry.terminated = true;
                            terminated_order.push_back(request_id);
                            while terminated_order.len() > TERMINATED_RETENTION {
                                if let Some(oldest) = terminated_order.pop_front() {
                                    requests.remove(&oldest);
                                }
                            }
                        }
                    }
                    None => break Ok(()),
                }
            }
            frame = read_frame_async(&mut reader) => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                    Err(error) => break Err(error),
                };
                let reply = match frame.kind {
                    Kind::Hello => handle_hello(frame.request_id, &frame.payload),
                    Kind::Execute => {
                        handle_execute(
                            &service,
                            frame.request_id,
                            &frame.payload,
                            &worker_tx,
                            &mut worker_active,
                            &mut active_id,
                            &mut requests,
                        )
                        .await?
                    }
                    Kind::Cancel => {
                        handle_cancel(
                            &service,
                            frame.request_id,
                            &frame.payload,
                            active_id,
                            &requests,
                        )
                        .await?
                    }
                    Kind::Health => handle_health(frame.request_id, &frame.payload),
                    Kind::Fs => handle_fs(&service, frame.request_id, &frame.payload).await?,
                    _ => Some(fail_closed_error(
                        frame.request_id,
                        "unsupported request kind",
                    )),
                };
                if let Some(frame) = reply {
                    write_frame_async(&mut writer, &frame).await?;
                }
            }
        }
    };

    // Connection is gone (peer closed or I/O error): cancel the in-flight turn
    // so the worker tears down the child's process group promptly instead of
    // letting the command run to completion, then wait a bounded time for the
    // worker to finish. The per-request exactly-once terminal contract is
    // irrelevant here because there is no peer left to receive a frame.
    teardown_active(&service, active_id, &requests, &mut worker_rx).await;

    io_result
}

/// Best-effort disconnect cleanup: point the active turn's cancellation flag
/// and drain the worker channel (bounded) until its single terminal result.
async fn teardown_active(
    service: &Arc<Mutex<RuntimeService>>,
    active_id: Option<[u8; 16]>,
    requests: &HashMap<[u8; 16], RequestEntry>,
    worker_rx: &mut mpsc::Receiver<WorkerMessage>,
) {
    let Some(id) = active_id else {
        return;
    };
    let Some(entry) = requests.get(&id) else {
        return;
    };
    {
        let mut runtime = service.lock().await;
        let _ = runtime.cancel_active(SESSION_ID, &entry.request_id);
    }
    let _ = tokio::time::timeout(ACTIVE_TEARDOWN_TIMEOUT, async {
        loop {
            match worker_rx.recv().await {
                Some(WorkerMessage::Terminal(_)) | None => break,
                Some(_) => continue,
            }
        }
    })
    .await;
}

/// Forward a live `terminal.output` event as a ZBRT `Output` frame for the
/// given request. Non-output events (e.g. `turn.started`) have no ZBRT
/// equivalent and are ignored.
async fn forward_event<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request_id: [u8; 16],
    event: &GuestEvent,
) -> io::Result<()> {
    if event.kind != "terminal.output" {
        return Ok(());
    }
    let Ok(terminal) = serde_json::from_slice::<TerminalEvent>(&event.payload) else {
        return Ok(());
    };
    let stream = match terminal.stream {
        TerminalStream::Stdout => 0,
        TerminalStream::Stderr => 1,
    };
    let payload = Output {
        stream,
        data: terminal.data.into_bytes(),
    }
    .encode()?;
    write_frame_async(
        writer,
        &Frame {
            kind: Kind::Output,
            flags: 0,
            request_id,
            payload,
        },
    )
    .await
}

fn handle_hello(request_id: [u8; 16], payload: &[u8]) -> Option<Frame> {
    let hello = Hello::decode(payload).ok()?;
    if hello.client.trim().is_empty() {
        return Some(fail_closed_error(request_id, "client is required"));
    }
    let hello_ack = HelloAck {
        server: "rfb-zeroboot-guest".into(),
        capabilities: ZBRT_V1_CAPABILITIES.iter().map(|s| s.to_string()).collect(),
    };
    let bytes = match hello_ack.encode() {
        Ok(bytes) => bytes,
        Err(_) => return Some(fail_closed_error(request_id, "encode HelloAck failed")),
    };
    Some(Frame {
        kind: Kind::HelloAck,
        flags: 0,
        request_id,
        payload: bytes,
    })
}

async fn handle_execute(
    service: &Arc<Mutex<RuntimeService>>,
    request_id: [u8; 16],
    payload: &[u8],
    worker_tx: &mpsc::Sender<WorkerMessage>,
    worker_active: &mut bool,
    active_id: &mut Option<[u8; 16]>,
    requests: &mut HashMap<[u8; 16], RequestEntry>,
) -> io::Result<Option<Frame>> {
    let exec = match Execute::decode(payload) {
        Ok(exec) => exec,
        Err(_) => {
            return Ok(Some(fail_closed_error(
                request_id,
                "invalid Execute payload",
            )))
        }
    };
    if exec.argv.is_empty() {
        return Ok(Some(fail_closed_error(request_id, "argv is empty")));
    }
    if *worker_active {
        return Ok(Some(fail_closed_error(
            request_id,
            "a turn is already active",
        )));
    }
    if requests.contains_key(&request_id) {
        return Ok(Some(fail_closed_error(request_id, "duplicate request id")));
    }
    let turn = execute_to_turn(request_id, exec);
    let request_id_str = turn.request_id.clone();
    let mut executor = {
        let mut runtime = service.lock().await;
        match runtime.spawn_turn(&turn) {
            Ok(executor) => executor,
            Err(message) => return Ok(Some(runtime_error_to_frame(request_id, message))),
        }
    };
    // Attach a live output sink that queues into the same ordered worker
    // channel as the terminal result, so Output frames always precede the
    // single terminal frame for this request.
    let tx = worker_tx.clone();
    let rid = request_id;
    executor.attach_event_sink(Some(Arc::new(move |event| {
        // A send error means the serve loop (and its channel) is gone; the
        // output event is dropped. Loud enough to be diagnosable, quiet
        // enough to stay out of the frame path.
        if tx
            .blocking_send(WorkerMessage::Output {
                request_id: rid,
                event,
            })
            .is_err()
        {
            eprintln!("rfb zeroboot: worker channel closed; dropping output event");
        }
    })));
    requests.insert(
        request_id,
        RequestEntry {
            request_id: request_id_str.clone(),
            terminated: false,
        },
    );
    let tx = worker_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = executor.start_turn(&turn);
        if tx
            .blocking_send(WorkerMessage::Terminal((
                request_id,
                turn.session_id.clone(),
                turn.request_id.clone(),
                result,
                executor,
            )))
            .is_err()
        {
            eprintln!("rfb zeroboot: worker channel closed; terminal result dropped");
        }
    });
    *worker_active = true;
    *active_id = Some(request_id);
    Ok(None)
}

async fn handle_fs(
    service: &Arc<Mutex<RuntimeService>>,
    request_id: [u8; 16],
    payload: &[u8],
) -> io::Result<Option<Frame>> {
    let fs = match crate::zeroboot_protocol::Fs::decode(payload) {
        Ok(v) => v,
        Err(_) => return Ok(Some(fail_closed_error(request_id, "invalid Fs payload"))),
    };
    // filesystem_rpc performs blocking filesystem work (tree walks, recursive
    // size accounting, file copies). The guest runtime is current_thread, so
    // running it inline would stall every other session and all timers for
    // the duration of the walk — move it to the blocking pool instead.
    let result = {
        let normalized = normalize_workspace_path(&fs.path);
        let service = Arc::clone(service);
        let (op, data) = (fs.op, fs.data);
        match tokio::task::spawn_blocking(move || {
            let mut guard = service.blocking_lock();
            guard.filesystem_rpc(op, &normalized, &data)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => {
                return Ok(Some(fail_closed_error(
                    request_id,
                    &format!("fs rpc task failed: {error}"),
                )));
            }
        }
    };
    match result {
        Ok(payload) => Ok(Some(Frame {
            kind: Kind::FsResult,
            flags: 0,
            request_id,
            payload,
        })),
        Err(message) => Ok(Some(fail_closed_error(request_id, &message))),
    }
}

async fn handle_cancel(
    service: &Arc<Mutex<RuntimeService>>,
    request_id: [u8; 16],
    payload: &[u8],
    active_id: Option<[u8; 16]>,
    requests: &HashMap<[u8; 16], RequestEntry>,
) -> io::Result<Option<Frame>> {
    let cancel = match Cancel::decode(payload) {
        Ok(cancel) => cancel,
        Err(_) => {
            return Ok(Some(fail_closed_error(
                request_id,
                "invalid Cancel payload",
            )))
        }
    };
    let target = cancel.target;

    // V1 single-active contract: at most one request is active per connection.
    // An explicit target must name exactly that active request; a legacy
    // payload carries no target and cancels whatever is active. Cancelling a
    // request that already discharged its single terminal is acknowledged
    // idempotently, so a racing stream.stop after the Exit frame was emitted
    // never spuriously fails.
    if let Some(id) = target {
        if requests
            .get(&id)
            .map(|entry| entry.terminated)
            .unwrap_or(false)
        {
            return Ok(Some(cancel_ack(request_id)));
        }
    }
    // Sandbox cancel semantics are idempotent: cancelling when nothing is
    // active (or the request already terminated) acknowledges successfully.
    let Some(id) = active_id else {
        return Ok(Some(cancel_ack(request_id)));
    };
    if let Some(target) = target {
        if id != target {
            return Ok(Some(fail_closed_error(
                request_id,
                "cancel target does not match active request",
            )));
        }
    }
    let Some(entry) = requests.get(&id) else {
        return Ok(Some(cancel_ack(request_id)));
    };
    let responses = {
        let mut runtime = service.lock().await;
        runtime.cancel_active(SESSION_ID, &entry.request_id)
    };
    let message = responses.iter().find_map(|m| match m {
        RuntimeMessage::Error { message, .. } => Some(message.clone()),
        _ => None,
    });
    if let Some(message) = message {
        return Ok(Some(fail_closed_error(request_id, &message)));
    }
    Ok(Some(cancel_ack(request_id)))
}

fn cancel_ack(request_id: [u8; 16]) -> Frame {
    Frame {
        kind: Kind::CancelAck,
        flags: 0,
        request_id,
        payload: Vec::new(),
    }
}

fn handle_health(request_id: [u8; 16], payload: &[u8]) -> Option<Frame> {
    let _ = Health::decode(payload).ok()?;
    let health = Health {
        healthy: true,
        message: Some("ready".into()),
    };
    let bytes = match health.encode() {
        Ok(bytes) => bytes,
        Err(_) => return Some(fail_closed_error(request_id, "encode HealthAck failed")),
    };
    Some(Frame {
        kind: Kind::HealthAck,
        flags: 0,
        request_id,
        payload: bytes,
    })
}

/// Normalize a ZBRT request path onto the workspace executor's relative-path
/// policy: `/workspace` (the host-side contract root, as used by forkd and the
/// web tool surface) maps to `.`, `/workspace/x` maps to `x`. Anything else
/// — including other absolute paths — is passed through unchanged so the
/// workspace `PathPolicy` still rejects it with `path escapes allowed root`.
fn normalize_workspace_path(path: &str) -> String {
    if path == "/workspace" {
        return ".".to_string();
    }
    if let Some(rest) = path.strip_prefix("/workspace/") {
        return rest.to_string();
    }
    path.to_string()
}

/// Build a [`SessionRequest`] the workspace executor can run from a ZBRT
/// `Execute` mapped to the shell-free workspace executor, including stdin and
/// the exact millisecond deadline.
fn execute_to_turn(request_id: [u8; 16], exec: Execute) -> SessionRequest {
    // stdin travels as a JSON byte array: the wire payload is arbitrary
    // bytes, and a lossy UTF-8 conversion would corrupt binary input.
    let mut prompt = serde_json::json!({
        "op": "exec",
        "args": exec.argv,
        "cwd": normalize_workspace_path(&exec.cwd.unwrap_or_else(|| ".".to_string())),
        "stdin": exec.stdin.iter().map(|b| serde_json::Value::from(*b)).collect::<Vec<_>>(),
    });
    if exec.timeout_ms > 0 {
        prompt["timeout_ms"] = serde_json::json!(exec.timeout_ms);
    }
    SessionRequest {
        session_id: SESSION_ID.to_string(),
        request_id: hex_id(request_id),
        prompt: prompt.to_string(),
    }
}

/// Convert the terminal runtime responses into exactly one ZBRT terminal frame
/// per request: `Exit` for a completed or cancelled turn, `Error` for a
/// terminal runtime error. Ordering is deterministic: the first terminal
/// response wins, so the connection never emits two terminals for one request.
fn terminal_frame(request_id: [u8; 16], responses: &[RuntimeMessage]) -> Option<Frame> {
    for message in responses {
        match message {
            RuntimeMessage::Error { message, .. } => {
                return Some(fail_closed_error(request_id, message));
            }
            RuntimeMessage::Event(event) if event.kind == "turn.cancelled" => {
                return Some(exit_frame(request_id, -1));
            }
            RuntimeMessage::Event(event) if event.kind == "turn.completed" => {
                let value = serde_json::from_slice::<serde_json::Value>(&event.payload)
                    .unwrap_or(serde_json::Value::Null);
                let code = value
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|v| i32::try_from(v).ok())
                    .unwrap_or(-1);
                return Some(exit_frame(request_id, code));
            }
            _ => {}
        }
    }
    Some(fail_closed_error(
        request_id,
        "turn finished without a terminal event",
    ))
}

fn exit_frame(request_id: [u8; 16], code: i32) -> Frame {
    let payload = Exit { code, signal: None }.encode().unwrap_or_default();
    Frame {
        kind: Kind::Exit,
        flags: 0,
        request_id,
        payload,
    }
}

fn fail_closed_error(request_id: [u8; 16], message: &str) -> Frame {
    let payload = ZbrtError {
        code: 1,
        message: message.to_string(),
    }
    .encode()
    .unwrap_or_default();
    Frame {
        kind: Kind::Error,
        flags: 0,
        request_id,
        payload,
    }
}

fn runtime_error_to_frame(request_id: [u8; 16], message: RuntimeMessage) -> Frame {
    if let RuntimeMessage::Error { message, .. } = message {
        fail_closed_error(request_id, &message)
    } else {
        fail_closed_error(request_id, "request rejected")
    }
}

fn hex_id(id: [u8; 16]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(32);
    for byte in id {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
