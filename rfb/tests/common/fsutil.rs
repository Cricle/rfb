//! Collision-free temporary paths for the socket-backed mock tests.
//!
//! Every helper mixes pid + nanos + a process-local sequence counter, so two
//! tests in the same binary (same pid, possibly the same wall nanosecond on
//! coarse clocks) can never derive the same path — the old pid-only
//! `socket_path` variants could, once suites share labels.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn unique_stem(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{nanos}-{seq}", std::process::id())
}

/// A unique, not-yet-existing directory under the system temp dir. The caller
/// owns creation and cleanup; the path is unique across processes and across
/// repeated calls within one process.
pub fn unique_temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(unique_stem(prefix))
}

/// A unique, not-yet-existing Unix socket path (`UnixListener::bind` refuses
/// to overwrite, so the anti-collision suffix is load-bearing).
#[cfg(unix)]
pub fn socket_path(prefix: &str) -> PathBuf {
    unique_temp_dir(prefix)
}
