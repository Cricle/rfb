//! Framed connection serving: read ControlMessage frames, run StartTurn on a
//! worker thread, and stream responses back while keeping Cancel/Shutdown
//! servable from other connections.

use crate::codec::{read_frame, write_frame, FrameCodec, MessageType};
use crate::runtime_service::{GuestEvent, GuestExecutor, RuntimeService};
use crate::session::{ControlMessage, RuntimeMessage};
use std::io;
use std::sync::Arc;
use tokio::sync::Mutex;

/// How long the post-disconnect wait for an in-flight turn's result may run.
/// The worker normally delivers as soon as the command finishes or the cancel
/// flag fires; if nothing arrives within this bound the worker is suspected
/// stuck (wedged child, lost delivery), so the turn is abandoned explicitly
/// instead of wedging the runtime on a phantom active turn forever.
const DETACHED_TURN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Completion of a turn that ran on a worker thread: (sequence, session,
/// request, result, executor handed back to the service).
type TurnResult = (
    u64,
    String,
    String,
    Result<Vec<GuestEvent>, String>,
    Box<dyn GuestExecutor>,
);

/// Serve one framed connection until EOF or a Shutdown frame.
pub(crate) async fn serve<R, W>(
    reader_stream: R,
    writer_stream: W,
    codec: FrameCodec,
    shared: Arc<Mutex<RuntimeService>>,
) -> io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut writer = tokio::io::BufWriter::new(writer_stream);
    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<TurnResult>(4);
    let mut worker_active = false;
    // The in-flight turn's worker handle: a panic kills the worker WITHOUT
    // ever sending on result_tx, so the channel alone cannot detect it while
    // this connection lives — the join result is the online panic detector.
    let mut worker_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Frame reading lives in a dedicated task (see `spawn_frame_reader` for
    // why: frame reads are not cancellation-safe, so the select! below must
    // never drop one mid-frame).
    let codec = Arc::new(codec);
    let mut frame_rx =
        crate::connection_util::spawn_frame_reader(tokio::io::BufReader::new(reader_stream), {
            let codec = Arc::clone(&codec);
            move |reader| {
                let codec = Arc::clone(&codec);
                Box::pin(async move { read_frame::<_, ControlMessage>(reader, &codec).await })
            }
        });

    // Every loop exit is a `break` carrying the I/O outcome — never a bare
    // `return`. An early return would drop `result_rx` while a turn is still
    // in flight: the worker's later delivery would fail, the executor would
    // be dropped, and the shared service would stay executor-less forever
    // (every future turn on this runtime bricked). The detached-turn
    // recovery after the loop therefore runs unconditionally.
    let io_result = loop {
        tokio::select! {
            biased;
            result = result_rx.recv(), if worker_active => {
                match result {
                    Some((sequence, session_id, request_id, result, executor)) => {
                        worker_active = false;
                        worker_handle = None;
                        let responses = {
                            let mut runtime = shared.lock().await;
                            runtime.complete_turn(session_id, request_id, result, executor)
                        };
                        if let Err(error) =
                            write_responses(&mut writer, &codec, sequence, &responses).await
                        {
                            break Err(error);
                        }
                    }
                    // 不可达分支（serve 自持 result_tx 直到函数结束，主循环
                    // 里 recv 不会返回 None）；保留只为穷尽性，语义上按
                    // 「结果永远不会来」兜底收回。
                    None => {
                        worker_active = false;
                        worker_handle = None;
                        let responses = {
                            let mut runtime = shared.lock().await;
                            runtime.abandon_active_turn()
                        };
                        if let Err(error) =
                            write_responses(&mut writer, &codec, 0, &responses).await
                        {
                            break Err(error);
                        }
                    }
                }
            }
            // Panic detector: the worker died without delivering a result.
            // (Biased after the result branch: a delivery always wins over
            // the join notification racing it.)
            joined = async {
                match worker_handle.as_mut() {
                    Some(handle) => handle.await,
                    None => std::future::pending().await,
                }
            }, if worker_active => {
                worker_active = false;
                worker_handle = None;
                match (joined, result_rx.try_recv()) {
                    // Panic: the executor can never come back — reclaim the
                    // in-flight turn so the runtime keeps answering.
                    (Err(_), _) => {
                        let responses = {
                            let mut runtime = shared.lock().await;
                            runtime.abandon_active_turn()
                        };
                        if let Err(error) =
                            write_responses(&mut writer, &codec, 0, &responses).await
                        {
                            break Err(error);
                        }
                    }
                    // The worker finished and its delivery is queued (the
                    // biased result branch lost the race): take it here.
                    (Ok(()), Ok((sequence, session_id, request_id, result, executor))) => {
                        let responses = {
                            let mut runtime = shared.lock().await;
                            runtime.complete_turn(session_id, request_id, result, executor)
                        };
                        if let Err(error) =
                            write_responses(&mut writer, &codec, sequence, &responses).await
                        {
                            break Err(error);
                        }
                    }
                    // The worker finished but never delivered (send failed):
                    // reclaim.
                    (Ok(()), Err(_)) => {
                        let responses = {
                            let mut runtime = shared.lock().await;
                            runtime.abandon_active_turn()
                        };
                        if let Err(error) =
                            write_responses(&mut writer, &codec, 0, &responses).await
                        {
                            break Err(error);
                        }
                    }
                }
            }
            frame = frame_rx.recv() => {
                let Some(frame) = frame else {
                    break Ok(());
                };
                let (frame, request) = match frame {
                    Ok(value) => value,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                    Err(error) => break Err(error),
                };
                let response_sequence = frame.sequence;
                let was_shutdown = shutdown_requested(&request);
                let responses = match &request {
                    ControlMessage::StartTurn(turn) if !worker_active => {
                        let turn = turn.clone();
                        // At-least-once replay: a repeated StartTurn whose
                        // result is cached (completed / cancelled / evicted)
                        // replays the terminal instead of re-running or
                        // hard-rejecting — mirrors the serial path.
                        let replay = {
                            let mut runtime = shared.lock().await;
                            runtime.replay_cached_turn(&turn)
                        };
                        if let Some(responses) = replay {
                            if let Err(error) = write_responses(
                                &mut writer,
                                &codec,
                                response_sequence,
                                &responses,
                            )
                            .await
                            {
                                break Err(error);
                            }
                            continue;
                        }
                        // Claim the turn under the shared lock, then release it
                        // immediately so other connections can Cancel while the
                        // worker runs.
                        let executor = {
                            let mut runtime = shared.lock().await;
                            match runtime.spawn_turn(&turn) {
                                Ok(executor) => executor,
                                Err(message) => {
                                    let _ = write_responses(
                                        &mut writer,
                                        &codec,
                                        response_sequence,
                                        &[message],
                                    )
                                    .await;
                                    continue;
                                }
                            }
                        };
                        let mut executor = executor;
                        let tx = result_tx.clone();
                        worker_handle = Some(tokio::task::spawn_blocking(move || {
                            let session_id = turn.session_id.clone();
                            let request_id = turn.request_id.clone();
                            let result = executor.start_turn(&turn);
                            let _ = tx.blocking_send((
                                response_sequence,
                                session_id,
                                request_id,
                                result,
                                executor,
                            ));
                        }));
                        worker_active = true;
                        vec![]
                    }
                    ControlMessage::Cancel { .. } => {
                        let mut runtime = shared.lock().await;
                        runtime.handle(request)
                    }
                    _ => {
                        let mut runtime = shared.lock().await;
                        runtime.handle(request)
                    }
                };
                if let Err(error) =
                    write_responses(&mut writer, &codec, response_sequence, &responses).await
                {
                    break Err(error);
                }
                if was_shutdown {
                    break Ok(());
                }
            }
        }
    };
    // If the connection ends while a turn is in flight (the client vanished,
    // a read error, or a failed response write), a detached task keeps the
    // result channel alive so the worker's delivery succeeds and the executor
    // is returned to the shared service exactly once. This recovery is
    // UNCONDITIONAL: any loop exit (graceful EOF or an I/O error) must run
    // it, because the in-flight worker's late `complete_turn` is the single
    // workspace executor's only way home — dropping the receiver here would
    // brick every future turn on this runtime. The FIRST wait is bounded: a
    // worker that neither delivers nor dies within DETACHED_TURN_WAIT stops
    // being treated as merely slow — the in-flight bookkeeping is abandoned
    // so Cancel/health answers stop lying. But the channel stays alive past
    // that point: a healthy worker with a long deadline (legal turns run up
    // to 1800s) delivers later, and its complete_turn is what returns the
    // single workspace executor.
    if worker_active {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            async fn wait_delivery(
                result_rx: &mut tokio::sync::mpsc::Receiver<TurnResult>,
            ) -> Option<TurnResult> {
                // `Some` = the worker delivered; `None` = the worker dropped
                // its sender without delivering (no result is ever coming).
                result_rx.recv().await
            }
            async fn deliver(shared: Arc<Mutex<RuntimeService>>, delivery: TurnResult) {
                let (_, session_id, request_id, result, executor) = delivery;
                let mut runtime = shared.lock().await;
                runtime.complete_turn(session_id, request_id, result, executor);
            }
            match tokio::time::timeout(DETACHED_TURN_WAIT, wait_delivery(&mut result_rx)).await {
                Ok(Some(delivery)) => deliver(shared, delivery).await,
                Ok(None) => {
                    let mut runtime = shared.lock().await;
                    runtime.abandon_active_turn();
                }
                // Bounded wait elapsed with a still-running worker: abandon
                // the bookkeeping once, then keep waiting for the late
                // delivery — it is the executor's only way home.
                Err(_elapsed) => {
                    {
                        let mut runtime = shared.lock().await;
                        runtime.abandon_active_turn();
                    }
                    // 第二段等待同样有界：worker 真正 wedged 时（历史教训：
                    // kill 后既不 deliver 也不 panic），不能永久泄漏一个
                    // task + channel。上限 = 合法 turn 的最大 deadline
                    // （1800s）加排空余量。
                    match tokio::time::timeout(
                        DETACHED_TURN_WAIT + std::time::Duration::from_secs(60),
                        wait_delivery(&mut result_rx),
                    )
                    .await
                    {
                        Ok(Some(delivery)) => deliver(shared, delivery).await,
                        _ => {}
                    }
                }
            }
        });
    }
    io_result
}

fn shutdown_requested(request: &ControlMessage) -> bool {
    matches!(request, ControlMessage::Shutdown)
}

pub(crate) async fn write_responses<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    codec: &FrameCodec,
    sequence: u64,
    responses: &[RuntimeMessage],
) -> io::Result<()> {
    for response in responses {
        let response_type = match response {
            RuntimeMessage::HelloAck { .. } => MessageType::HelloAck,
            RuntimeMessage::Capabilities { .. } => MessageType::Capabilities,
            RuntimeMessage::Event(_) => MessageType::Event,
            RuntimeMessage::Error { .. } => MessageType::Error,
            RuntimeMessage::ShutdownAck => MessageType::Shutdown,
            RuntimeMessage::FileContent { .. } => MessageType::FileContent,
            RuntimeMessage::WriteAck { .. } => MessageType::WriteAck,
        };
        write_frame(writer, codec, response_type, sequence, response).await?;
    }
    tokio::io::AsyncWriteExt::flush(writer).await?;
    Ok(())
}
