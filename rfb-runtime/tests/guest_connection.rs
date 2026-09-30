//! RFB1 framed serve loop: error-path recovery contract (integration tests).
//!
//! The serve loop must never exit on an I/O error without running the
//! detached-turn recovery: the in-flight worker's late delivery is the single
//! workspace executor's only way back to the shared
//! [`rfb_runtime::runtime_service::RuntimeService`], and losing it bricks the
//! runtime (every future turn rejected with "still finishing").

#![cfg(all(feature = "guest", target_os = "linux"))]

use rfb_runtime::codec::{write_frame_blocking, FrameCodec, MessageType};
use rfb_runtime::guest_vsock::dispatcher;
use rfb_runtime::resources::RuntimeLimits;
use rfb_runtime::runtime_service::{GuestEvent, GuestExecutor, RuntimeService};
use rfb_runtime::session::{ControlMessage, SessionRequest};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::AsyncRead;
use tokio::sync::Mutex;

/// Executor whose `start_turn` blocks until the test releases it, then
/// completes the turn normally — simulating a long-running legal turn.
struct GatedExecutor {
    /// Set once the worker has entered `start_turn`.
    started: Arc<AtomicBool>,
    /// The worker spins on this until the test releases it.
    release: Arc<AtomicBool>,
}

impl GuestExecutor for GatedExecutor {
    fn start_turn(&mut self, _request: &SessionRequest) -> Result<Vec<GuestEvent>, String> {
        self.started.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(vec![GuestEvent::new("turn.completed", b"done")])
    }
    fn cancel(&mut self, _session_id: &str, _request_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Serves the prepared frame bytes, then fails every subsequent read with a
/// non-EOF I/O error — exactly the serve-loop error path that used to `return`
/// and skip the detached-turn recovery.
struct FramesThenReset {
    bytes: io::Cursor<Vec<u8>>,
}

impl AsyncRead for FramesThenReset {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let pos = self.bytes.position() as usize;
        let data = self.bytes.get_ref();
        if pos >= data.len() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection reset after the turn started",
            )));
        }
        let count = buf.remaining().min(data.len() - pos);
        buf.put_slice(&data[pos..pos + count]);
        self.bytes.set_position((pos + count) as u64);
        Poll::Ready(Ok(()))
    }
}

fn encoded_frames() -> Vec<u8> {
    let codec = FrameCodec::default();
    let mut bytes = Vec::new();
    write_frame_blocking(
        &mut bytes,
        &codec,
        MessageType::Hello,
        1,
        &ControlMessage::Hello {
            protocol_version: 1,
        },
    )
    .unwrap();
    write_frame_blocking(
        &mut bytes,
        &codec,
        MessageType::StartTurn,
        2,
        &ControlMessage::StartTurn(SessionRequest {
            session_id: "s".into(),
            request_id: "r1".into(),
            prompt: "prompt".into(),
        }),
    )
    .unwrap();
    bytes
}

#[tokio::test]
async fn serve_error_exit_while_turn_in_flight_still_returns_the_executor() {
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Mutex::new(RuntimeService::with_executor_impl(
        RuntimeLimits::default(),
        GatedExecutor {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        },
    )));

    let codec = FrameCodec::default();
    let serve_task = tokio::spawn(dispatcher::serve(
        FramesThenReset {
            bytes: io::Cursor::new(encoded_frames()),
        },
        tokio::io::sink(),
        codec,
        Arc::clone(&shared),
    ));

    // Wait until the worker is inside start_turn: the turn is in flight and
    // its executor is out of the shared service.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "worker never entered start_turn");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The non-EOF read error exits serve() while the turn is still in flight.
    let outcome = serve_task.await.unwrap();
    assert_eq!(
        outcome.unwrap_err().kind(),
        io::ErrorKind::ConnectionReset,
        "serve must exit through the read-error path"
    );

    // Release the worker: its late delivery must reach the detached recovery
    // task, which hands the executor back to the shared service exactly once.
    release.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error;
    loop {
        let turn = SessionRequest {
            session_id: "s".into(),
            request_id: "r2".into(),
            prompt: "prompt".into(),
        };
        match shared.lock().await.spawn_turn(&turn) {
            Ok(_executor) => {
                // The executor came home: a follow-up turn is claimable again.
                return;
            }
            Err(message) => {
                last_error = format!("{message:?}");
            }
        }
        assert!(
            Instant::now() < deadline,
            "executor must be returned to the shared service after the \
             error-path exit; last spawn_turn error: {last_error}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
