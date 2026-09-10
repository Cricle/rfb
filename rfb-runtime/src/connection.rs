//! Framed connection serving: read ControlMessage frames, run StartTurn on a
//! worker thread, and stream responses back while keeping Cancel/Shutdown
//! servable from other connections.

use rfb_runtime::codec::{read_frame, write_frame, Frame, FrameCodec, MessageType};
use rfb_runtime::runtime_service::{GuestEvent, GuestExecutor, RuntimeService};
use rfb_runtime::session::{ControlMessage, RuntimeMessage};
use std::io;
use std::sync::Arc;
use tokio::sync::Mutex;

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
pub(super) async fn serve<R, W>(
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

    // Frame reading lives in a dedicated task: read_frame is not
    // cancellation-safe, so the select! below must never drop it mid-frame —
    // a dropped read loses partially buffered bytes and desyncs the stream
    // for the rest of the connection. Receiving from the channel IS
    // cancellation-safe.
    let codec = Arc::new(codec);
    let (frame_tx, mut frame_rx) =
        tokio::sync::mpsc::channel::<io::Result<(Frame, ControlMessage)>>(8);
    let read_codec = Arc::clone(&codec);
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(reader_stream);
        loop {
            match read_frame::<_, ControlMessage>(&mut reader, &read_codec).await {
                Ok(value) => {
                    if frame_tx.send(Ok(value)).await.is_err() {
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

    loop {
        tokio::select! {
            biased;
            result = result_rx.recv(), if worker_active => {
                match result {
                    Some((sequence, session_id, request_id, result, executor)) => {
                        worker_active = false;
                        let responses = {
                            let mut runtime = shared.lock().await;
                            runtime.complete_turn(session_id, request_id, result, executor)
                        };
                        write_responses(&mut writer, &codec, sequence, &responses).await?;
                    }
                    // The worker died without delivering a result (panic):
                    // reclaim the in-flight turn bookkeeping so the runtime
                    // keeps answering instead of reporting a phantom active
                    // turn forever.
                    None => {
                        worker_active = false;
                        let responses = {
                            let mut runtime = shared.lock().await;
                            runtime.abandon_active_turn()
                        };
                        write_responses(&mut writer, &codec, 0, &responses).await?;
                    }
                }
            }
            frame = frame_rx.recv() => {
                let Some(frame) = frame else {
                    break;
                };
                let (frame, request) = match frame {
                    Ok(value) => value,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(error),
                };
                let response_sequence = frame.sequence;
                let was_shutdown = shutdown_requested(&request);
                let responses = match &request {
                    ControlMessage::StartTurn(turn) if !worker_active => {
                        let turn = turn.clone();
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
                        tokio::task::spawn_blocking(move || {
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
                        });
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
                write_responses(&mut writer, &codec, response_sequence, &responses).await?;
                if was_shutdown {
                    break;
                }
            }
        }
    }
    // If the connection ends while a turn is in flight (the client vanished),
    // a detached task keeps the result channel alive so the worker's delivery
    // succeeds and the executor is returned to the shared service exactly
    // once. A worker that died without delivering is abandoned explicitly so
    // the runtime keeps answering instead of wedging on a phantom turn.
    if worker_active {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            match result_rx.recv().await {
                Some((_, session_id, request_id, result, executor)) => {
                    let mut runtime = shared.lock().await;
                    runtime.complete_turn(session_id, request_id, result, executor);
                }
                None => {
                    let mut runtime = shared.lock().await;
                    runtime.abandon_active_turn();
                }
            }
        });
    }
    Ok(())
}

fn shutdown_requested(request: &ControlMessage) -> bool {
    matches!(request, ControlMessage::Shutdown)
}

pub(super) async fn write_responses<W: tokio::io::AsyncWrite + Unpin>(
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
