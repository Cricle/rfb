//! Connection-serving plumbing shared by the framed guest transports
//! (RFB1 [`crate::guest_connection`], ZeroBoot [`crate::zeroboot_connection`]).
//!
//! Both serve loops need the same frame-read discipline, so it lives here
//! once; the wire-specific handling stays with each connection.

use std::future::Future;
use std::io;
use std::pin::Pin;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;

/// Frame-reader channel capacity: enough in-flight frames for a pipelining
/// peer without letting the reader outrun the serve loop unboundedly.
pub(crate) const FRAME_READER_CHANNEL: usize = 8;

/// Spawn the dedicated frame-read task and return its channel receiver.
///
/// Frame reads are not cancellation-safe (a frame's header and payload are
/// separate reads), so the serve loops must never drop a read mid-frame — a
/// dropped read loses partially consumed bytes and desyncs the stream for the
/// rest of the connection. Receiving from the returned channel IS
/// cancellation-safe, which is what makes the serve loops' `select!` over
/// frames and worker messages legal. When the returned receiver is dropped,
/// the task sees a closed channel on its next send and exits; a read error is
/// delivered once as `Err` and ends the task.
pub(crate) fn spawn_frame_reader<R, T>(
    mut reader: R,
    mut read: impl for<'a> FnMut(&'a mut R) -> Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>
        + Send
        + 'static,
) -> mpsc::Receiver<io::Result<T>>
where
    R: AsyncRead + Unpin + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel(FRAME_READER_CHANNEL);
    tokio::spawn(async move {
        loop {
            match read(&mut reader).await {
                Ok(frame) => {
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    break;
                }
            }
        }
    });
    rx
}
