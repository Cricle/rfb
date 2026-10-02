//! ZeroBoot V1 (ZBRT) guest connection: ZBRT frames driven through the RFB1
//! [`crate::runtime_service::RuntimeService`] and
//! [`crate::workspace_executor::WorkspaceGuestExecutor`] semantics.
//!
//! The connection speaks the ZBRT binary wire protocol (magic `ZBRT`, version
//! 1, 28-byte header) and maps its request types onto the existing
//! `rfb-runtime` execution/state-machine machinery:
//!
//! - `Hello` -> RFB1 `Hello` handshake and a ZBRT `HelloAck` reply. The
//!   handshake is mandatory: any other frame sent first fails closed with a
//!   ZBRT `Error` frame ("protocol handshake required").
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
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn serve<R, W>(
    reader: R,
    writer: W,
    service: Arc<Mutex<RuntimeService>>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send,
{
    let mut writer = writer;
    // ZBRT handshake is enforced, never auto-negotiated: the peer must send a
    // wire `Hello` before any other frame. Auto-Hello here would silently
    // establish protocol readiness for every connection and void the
    // runtime-service guards that reject pre-handshake work.
    let mut handshaked = false;

    let (worker_tx, mut worker_rx) = mpsc::channel::<WorkerMessage>(WORKER_CHANNEL_CAPACITY);
    let mut worker_active = false;
    // The in-flight turn's worker handle: a panic kills the worker WITHOUT
    // ever sending on the worker channel, so the channel alone cannot detect
    // it while this connection lives — the join result is the online panic
    // detector (same design as the RFB1 framed connection).
    let mut worker_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut active_id: Option<[u8; 16]> = None;
    let mut requests: HashMap<[u8; 16], RequestEntry> = HashMap::new();
    // Insertion order of terminated entries, for oldest-first eviction past
    // TERMINATED_RETENTION so the bookkeeping stays bounded per connection.
    let mut terminated_order: std::collections::VecDeque<[u8; 16]> =
        std::collections::VecDeque::new();

    // Frame reading lives in a dedicated task: `read_frame_async` is not
    // cancellation-safe (header and payload are separate reads), so the
    // select! below must never drop it mid-frame — a dropped read loses
    // partially consumed bytes and desyncs the stream for the rest of the
    // connection. Receiving from the channel IS cancellation-safe.
    let (frame_tx, mut frame_rx) = mpsc::channel::<io::Result<Frame>>(8);
    tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match read_frame_async(&mut reader).await {
                Ok(frame) => {
                    if frame_tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = frame_tx.send(Err(error)).await;
                    break;
                }
            }
        }
    });

    let io_result = loop {
        tokio::select! {
            biased;
            message = worker_rx.recv(), if worker_active => {
                match message {
                    Some(WorkerMessage::Output { request_id, event }) => {
                        // Only forward output that belongs to the currently
                        // active request; straggler chunks from a cancelled
                        // turn are dropped once its terminal has been sent.
                        if active_id == Some(request_id) {
                            if let Err(error) =
                                forward_event(&mut writer, request_id, &event).await
                            {
                                break Err(error);
                            }
                        }
                    }
                    Some(WorkerMessage::Terminal((request_id, session_id, request_id_str, result, executor))) => {
                        worker_active = false;
                        worker_handle = None;
                        active_id = None;
                        let responses = {
                            let mut runtime = service.lock().await;
                            runtime.complete_turn(session_id, request_id_str, result, executor)
                        };
                        if let Some(frame) = terminal_frame(request_id, &responses) {
                            if let Err(error) = write_frame_async(&mut writer, &frame).await {
                                break Err(error);
                            }
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
            // Panic detector: the worker died without ever sending its
            // terminal result. (Biased after the worker branch: a delivery
            // always wins over the join notification racing it — the worker
            // sends before it exits, so a finished worker's Terminal is
            // already queued when this branch is polled.)
            joined = async {
                match worker_handle.as_mut() {
                    Some(handle) => handle.await,
                    None => std::future::pending().await,
                }
            }, if worker_active => {
                match joined {
                    // Panic: the executor can never come back, and the worker
                    // will never send the terminal that clears `worker_active`
                    // — the connection (and, with the shared workspace
                    // executor gone, every later FS RPC) would report "turn in
                    // progress" forever. Abandon the in-flight turn
                    // bookkeeping and fail THIS request closed with an Error
                    // frame. The connection itself stays servable for control
                    // RPCs (Health, Cancel) only: with the executor lost, every
                    // later Execute stays rejected with an actionable
                    // "still finishing" error instead of hanging.
                    Err(_) => {
                        worker_active = false;
                        worker_handle = None;
                        let responses = {
                            let mut runtime = service.lock().await;
                            runtime.abandon_active_turn()
                        };
                        let frame = active_id
                            .take()
                            .and_then(|id| terminal_frame(id, &responses));
                        if let Some(frame) = frame {
                            if let Err(error) = write_frame_async(&mut writer, &frame).await {
                                break Err(error);
                            }
                        }
                    }
                    // The worker finished normally; its Terminal delivery is
                    // queued (or the delivery already won the race and cleared
                    // the handle). The receiver lives in this loop, so the
                    // delivery cannot have failed: the worker branch above
                    // consumes it. Only retire the join handle.
                    Ok(()) => {
                        worker_handle = None;
                    }
                }
            }
            frame = frame_rx.recv() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                    Some(Err(error)) => break Err(error),
                    None => break Ok(()),
                };
                let reply = if !handshaked && frame.kind != Kind::Hello {
                    // Fail closed: every non-Hello frame before the handshake
                    // is rejected instead of reaching the runtime service.
                    Some(fail_closed_error(
                        frame.request_id,
                        "protocol handshake required",
                    ))
                } else {
                    match frame.kind {
                    Kind::Hello => {
                        // Establish protocol readiness in the runtime service
                        // FIRST, then answer the wire Hello. The handshake is
                        // only open once a HelloAck is actually produced.
                        let reply = {
                            let mut runtime = service.lock().await;
                            let _ = runtime.handle(ControlMessage::Hello {
                                protocol_version: 1,
                            });
                            handle_hello(frame.request_id, &frame.payload)
                        };
                        if matches!(&reply, Some(replied) if replied.kind == Kind::HelloAck) {
                            handshaked = true;
                        }
                        reply
                    }
                    Kind::Execute => {
                        match handle_execute(
                            &service,
                            frame.request_id,
                            &frame.payload,
                            &worker_tx,
                            &mut worker_active,
                            &mut active_id,
                            &mut requests,
                            &mut terminated_order,
                            &mut writer,
                        )
                        .await
                        {
                            Ok((reply, handle)) => {
                                if let Some(handle) = handle {
                                    worker_handle = Some(handle);
                                }
                                reply
                            }
                            Err(error) => break Err(error),
                        }
                    }
                    Kind::Cancel => {
                        match handle_cancel(
                            &service,
                            frame.request_id,
                            &frame.payload,
                            active_id,
                            &requests,
                        )
                        .await
                        {
                            Ok(reply) => reply,
                            Err(error) => break Err(error),
                        }
                    }
                    Kind::Health => handle_health(frame.request_id, &frame.payload),
                    Kind::Fs => match handle_fs(&service, frame.request_id, &frame.payload).await {
                        Ok(reply) => reply,
                        Err(error) => break Err(error),
                    },
                    _ => Some(fail_closed_error(
                        frame.request_id,
                        "unsupported request kind",
                    )),
                }
                };
                if let Some(frame) = reply {
                    if let Err(error) = write_frame_async(&mut writer, &frame).await {
                        break Err(error);
                    }
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
    let data = match terminal.data_bytes {
        Some(bytes) => bytes,
        None => terminal.data.into_bytes(),
    };
    let payload = Output { stream, data }.encode()?;
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
    let hello = match Hello::decode(payload) {
        Ok(hello) => hello,
        // Fail closed like Execute/Fs/Cancel: a peer that sent garbage gets a
        // ZBRT Error frame instead of hanging until its I/O timeout.
        Err(_) => return Some(fail_closed_error(request_id, "invalid Hello payload")),
    };
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

#[allow(clippy::too_many_arguments)] // 与 stream.rs 的六参数聚合同理由：拆聚合结构会让每个调用点的借用图更乱
async fn handle_execute<W: AsyncWrite + Unpin>(
    service: &Arc<Mutex<RuntimeService>>,
    request_id: [u8; 16],
    payload: &[u8],
    worker_tx: &mpsc::Sender<WorkerMessage>,
    worker_active: &mut bool,
    active_id: &mut Option<[u8; 16]>,
    requests: &mut HashMap<[u8; 16], RequestEntry>,
    terminated_order: &mut std::collections::VecDeque<[u8; 16]>,
    writer: &mut W,
) -> io::Result<(Option<Frame>, Option<tokio::task::JoinHandle<()>>)> {
    let exec = match Execute::decode(payload) {
        Ok(exec) => exec,
        Err(_) => {
            return Ok((
                Some(fail_closed_error(request_id, "invalid Execute payload")),
                None,
            ))
        }
    };
    if exec.argv.is_empty() {
        return Ok((Some(fail_closed_error(request_id, "argv is empty")), None));
    }
    if *worker_active {
        return Ok((
            Some(fail_closed_error(request_id, "a turn is already active")),
            None,
        ));
    }
    if requests.contains_key(&request_id) {
        return Ok((
            Some(fail_closed_error(request_id, "duplicate request id")),
            None,
        ));
    }
    let turn = execute_to_turn(request_id, exec);
    let request_id_str = turn.request_id.clone();
    let mut executor = {
        let mut runtime = service.lock().await;
        match runtime.spawn_turn(&turn) {
            Ok(executor) => executor,
            Err(message) => {
                return Ok((Some(runtime_error_to_frame(request_id, message)), None));
            }
        }
    };
    requests.insert(
        request_id,
        RequestEntry {
            request_id: request_id_str.clone(),
            terminated: false,
        },
    );
    // **builtin 快路径**：in-process 的内建命令（echo/ls/true/…）在
    // start_turn 里同步完成（亚 100μs CPU）——塞进 spawn_blocking = 每
    // turn 多付一次阻塞线程池的入池/唤醒往返（本平台 ~0.3-0.5ms，UDS
    // 并发扩展的实测差距与之一致）。inline：事件经本地收集器直出
    // forward_event（**绝不走 worker 通道**——serve 此刻阻塞在本调用里，
    // 通道的 blocking_send 会自己堵自己）；读完在独立 task 里继续缓冲，
    // serve 只被内建命令的微秒级 CPU 短暂占用。真实进程 spawn 仍走
    // spawn_blocking（可能阻塞数秒）。
    if crate::builtin::builtin(
        &serde_json::from_str::<serde_json::Value>(&turn.prompt).unwrap_or(serde_json::Value::Null),
    )
    .is_some()
    {
        let collected: Arc<std::sync::Mutex<Vec<_>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let sink = collected.clone();
            executor.attach_event_sink(Some(Arc::new(move |event| {
                sink.lock().unwrap_or_else(|p| p.into_inner()).push(event);
            })));
        }
        // **panic 边界必须保留**：executor 的实现可能 panic（测试注入的
        // PanickingExecutor 就是）——inline 的 panic 若炸穿 serve 循环 =
        // 整条连接死 + executor 滞留（worker 路径靠 join 探测防这个）。
        // catch_unwind 同步捕获 → 走同款 fail-closed（abandon + Error
        // 帧），且比 join 探测**更快**（无 5s 窗口）。
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            executor.start_turn(&turn)
        })) {
            Ok(result) => result,
            Err(_) => {
                let responses = {
                    let mut runtime = service.lock().await;
                    runtime.abandon_active_turn()
                };
                if let Some(frame) = terminal_frame(request_id, &responses) {
                    write_frame_async(writer, &frame).await?;
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
                return Ok((None, None));
            }
        };
        // 先 collect 再循环：lock 的 guard 不得跨 await（MutexGuard 非
        // Send，会传染整个 serve future）。
        let collected_events: Vec<_> = collected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect();
        for event in collected_events {
            if let Err(error) = forward_event(writer, request_id, &event).await {
                // inline 的写失败 = 连接级故障：executor 还没归还，先
                // 归还再上抛（否则工作区执行器永久滞留）。
                {
                    let mut runtime = service.lock().await;
                    runtime.complete_turn(
                        turn.session_id.clone(),
                        turn.request_id.clone(),
                        Err("connection failed mid-turn".into()),
                        executor,
                    );
                }
                return Err(error);
            }
        }
        let responses = {
            let mut runtime = service.lock().await;
            runtime.complete_turn(
                turn.session_id.clone(),
                request_id_str.clone(),
                result,
                executor,
            )
        };
        if let Some(frame) = terminal_frame(request_id, &responses) {
            write_frame_async(writer, &frame).await?;
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
        return Ok((None, None));
    }
    // Attach a live output sink that queues into the same ordered worker
    // channel as the terminal result, so Output frames always precede the
    // single terminal frame for this request.
    //
    // try_send, never blocking_send: a full channel means the serve loop is
    // backpressured writing to a slow vsock peer. Blocking here would park
    // the exec thread past every cancel/deadline check (the cancel flag is
    // only observed between chunks), turning a slow consumer into an
    // uncancellable turn. Overflow drops the chunk instead — live streaming
    // is lossy under extreme backpressure, the terminal result is not: it is
    // sent with blocking_send below, after start_turn has returned.
    let tx = worker_tx.clone();
    let rid = request_id;
    executor.attach_event_sink(Some(Arc::new(move |event| {
        if let Err(send_error) = tx.try_send(WorkerMessage::Output {
            request_id: rid,
            event,
        }) {
            if matches!(
                send_error,
                tokio::sync::mpsc::error::TrySendError::Closed(_)
            ) {
                eprintln!("rfb zeroboot: worker channel closed; dropping output event");
            }
            // Full(...) = serve loop backpressured; drop the chunk silently
            // (a stderr line per dropped chunk would itself feed the firehose).
        }
    })));
    requests.insert(
        request_id,
        RequestEntry {
            request_id: request_id_str.clone(),
            terminated: false,
        },
    );
    // Keep the handle in the serve loop: it is the panic detector for this
    // worker (a panic never delivers a Terminal, so the channel alone cannot
    // clear `worker_active`).
    let tx = worker_tx.clone();
    let handle = tokio::task::spawn_blocking(move || {
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
    Ok((None, Some(handle)))
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
    if Health::decode(payload).is_err() {
        return Some(fail_closed_error(request_id, "invalid Health payload"));
    };
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
///
/// stdin travels base64-encoded (`stdin_b64`): the wire payload is arbitrary
/// bytes, and both the historical JSON byte array (up to ~30x allocation
/// amplification inside the guest) and a lossy UTF-8 conversion are wrong.
fn execute_to_turn(request_id: [u8; 16], exec: Execute) -> SessionRequest {
    let mut prompt = serde_json::json!({
        "op": "exec",
        "args": exec.argv,
        "cwd": normalize_workspace_path(&exec.cwd.unwrap_or_else(|| ".".to_string())),
    });
    if !exec.stdin.is_empty() {
        use base64::Engine as _;
        prompt["stdin_b64"] =
            serde_json::Value::from(base64::engine::general_purpose::STANDARD.encode(&exec.stdin));
    }
    if exec.timeout_ms > 0 {
        prompt["timeout_ms"] = serde_json::json!(exec.timeout_ms);
    }
    SessionRequest {
        session_id: SESSION_ID.to_string(),
        request_id: hex_id(request_id),
        prompt: prompt.to_string(),
    }
}

/// Only the `exit_code` field of a `turn.completed` payload. The payload also
/// carries the full bounded stdout/stderr; a typed deserialize skips those
/// string fields without materializing them into a JSON tree, so building the
/// Exit frame costs no allocation proportional to the command output. The
/// RFB1 wire shape is unchanged.
#[derive(serde::Deserialize)]
struct TurnCompletedExit {
    #[serde(default)]
    exit_code: Option<i64>,
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
                let code = serde_json::from_slice::<TurnCompletedExit>(&event.payload)
                    .ok()
                    .and_then(|exit| exit.exit_code)
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
