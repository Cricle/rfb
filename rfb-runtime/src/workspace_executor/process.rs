//! Cancellable, bounded process execution for the workspace executor.

use super::WorkspaceGuestExecutor;
use crate::runtime_service::GuestEvent;
use crate::session::{TerminalEvent, TerminalStream};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

impl WorkspaceGuestExecutor {
    pub(super) fn exec(&self, value: &Value, cancel: &AtomicBool) -> Result<Value, String> {
        let args = value
            .get("args")
            .and_then(Value::as_array)
            .ok_or("args must be an array")?;
        if args.is_empty() || args.iter().any(|v| !v.is_string()) {
            return Err("args must be a non-empty string array".into());
        }
        if cancel.load(Ordering::SeqCst) {
            return Err("request cancelled".into());
        }
        let cwd = value.get("cwd").and_then(Value::as_str).unwrap_or(".");
        let cwd = self.policy.workspace_path(cwd).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(args[0].as_str().unwrap());
        cmd.args(args.iter().skip(1).map(|v| v.as_str().unwrap()))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        // The child can create, modify, or delete workspace files, so the
        // cached workspace size cannot survive an exec.
        self.invalidate_workspace_size();
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            // ZBRT V1 execute contract (acceptance-matrix): a command the
            // guest cannot spawn returns a normal Result with exit=-1 and the
            // OS error on stderr, never an Error frame.
            Err(error) => {
                let message = format!("rfb-runtime: failed to spawn command: {error}");
                return Ok(json!({
                    "stdout": "",
                    "stderr": bounded(message.as_bytes(), self.limits.max_event_bytes),
                    "exit_code": -1,
                    "success": false,
                }));
            }
        };
        let pid = child.id();
        let stdout = match child.stdout.take() {
            Some(pipe) => pipe,
            // Last setup failures before the guard below exists; make sure a
            // successfully spawned child never leaks on them.
            None => {
                terminate_and_reap(&mut child, pid);
                return Err("stdout pipe unavailable".into());
            }
        };
        let stderr = match child.stderr.take() {
            Some(pipe) => pipe,
            None => {
                terminate_and_reap(&mut child, pid);
                return Err("stderr pipe unavailable".into());
            }
        };
        // From here on every error path (validation, cancel, deadline, reader
        // failure) must terminate and reap the spawned child; the guard does
        // so on drop and is disarmed on the success path.
        let mut guard = ChildGuard {
            child,
            pid,
            armed: true,
        };
        let capture_limit = self.limits.max_event_bytes;
        let sink = self.event_sink.clone();
        let stdout_sink = sink.clone();
        let stderr_sink = sink;

        // Reader completion is event-driven: the child's exit closes the
        // pipes, both readers hit EOF and send immediately, so a fast command
        // never pays a poll quantum between exit and terminal frame. The
        // recv_timeout below only bounds cancel/deadline checks, which are
        // latency-insensitive.
        let (tx, rx) = std::sync::mpsc::channel::<(u8, std::io::Result<Vec<u8>>)>();
        // Unbounded channel is safe by construction: each reader sends exactly
        // once (two sends total), so the queue cannot grow and senders never
        // block.
        let stderr_tx = tx.clone();
        std::thread::spawn(move || {
            let result = read_stream(stdout, 0, capture_limit, stdout_sink);
            let _ = tx.send((0, result));
        });
        std::thread::spawn(move || {
            let result = read_stream(stderr, 1, capture_limit, stderr_sink);
            let _ = stderr_tx.send((1, result));
        });
        // Readers must drain stdout/stderr BEFORE stdin is written: a child
        // that fills its output pipe (~64 KiB) before consuming stdin would
        // otherwise block a synchronous write here forever, while the allowed
        // stdin size reaches ~16 MiB.
        let stdin = guard.child.stdin.take();
        // stdin arrives either as a string or as a JSON byte array (ZBRT wire
        // stdin is arbitrary bytes and must not pass through UTF-8 lossy
        // conversion).
        let input_bytes: Option<Vec<u8>> = match value.get("stdin") {
            Some(Value::String(text)) => Some(text.clone().into_bytes()),
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|v| {
                        v.as_u64()
                            .and_then(|n| u8::try_from(n).ok())
                            .ok_or("stdin must be bytes in 0..=255".to_owned())
                    })
                    .collect::<Result<Vec<_>, String>>()?,
            ),
            Some(_) => return Err("stdin must be a string or byte array".into()),
            None => None,
        };
        if let (Some(mut stdin), Some(input)) = (stdin, input_bytes) {
            // Dedicated writer thread: a blocking pipe write cannot stall the
            // cancel/deadline loop below. Write errors (e.g. broken pipe after
            // the child exited early) are non-fatal — stop writing silently;
            // the exit code already reflects the child.
            std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            });
        }
        // No input: the taken handle dropped above, closing the pipe as EOF.
        let timeout = value
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .or_else(|| {
                value
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .map(Duration::from_secs)
            })
            .unwrap_or_else(|| Duration::from_secs(self.limits.max_runtime_seconds));
        if timeout.is_zero() {
            return Err("timeout_secs/timeout_ms must be non-zero".into());
        }
        let timeout = timeout.min(Duration::from_secs(self.limits.max_runtime_seconds));
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        let mut reader_results: [Option<std::io::Result<Vec<u8>>>; 2] = [None, None];
        let status = loop {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok((stream, result)) => {
                    reader_results[stream as usize] = Some(result);
                    if reader_results.iter().all(Option::is_some) {
                        // Both pipes hit EOF but the child may still be alive
                        // (it closed its own stdout/stderr). Use try_wait with
                        // a bounded loop so cancel/deadline remain reachable.
                        for _ in 0..50 {
                            match guard.child.try_wait() {
                                Ok(Some(status)) => break status,
                                Ok(None) => {}
                                Err(e) => break Err(e.to_string()),
                            }
                            if cancel.load(Ordering::SeqCst) {
                                return Err("request cancelled".into());
                            }
                            if std::time::Instant::now() >= deadline {
                                return Err("command timed out".into());
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        break guard
                            .child
                            .wait()
                            .map_err(|e| e.to_string());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::SeqCst) {
                        // The guard kills the process group and reaps on the
                        // way out. Do not block on the reader threads: on Linux
                        // the killed process group closes the pipes so they
                        // drain immediately; on Windows a surviving descendant
                        // may hold the pipe open. The threads end when this
                        // process exits.
                        return Err("request cancelled".into());
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err("command timed out".into());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("stream reader failed".into());
                }
            }
        };
        // Success: the child has been waited on, so the error-path guard is
        // no longer needed.
        guard.disarm();
        let stdout = reader_results[0]
            .take()
            .ok_or("stdout reader failed")?
            .map_err(|e| e.to_string())?;
        let stderr = reader_results[1]
            .take()
            .ok_or("stderr reader failed")?
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"stdout": bounded(&stdout, self.limits.max_event_bytes), "stderr": bounded(&stderr, self.limits.max_event_bytes), "exit_code": status.code(), "success": status.success()}),
        )
    }
}

/// Read one child stream to EOF, forwarding each chunk as a live
/// `terminal.output` event when a sink is attached, and returning the bounded
/// aggregated bytes for the terminal result payload.
fn read_stream(
    mut reader: impl Read,
    stream: u8,
    capture_limit: usize,
    sink: Option<Arc<dyn Fn(GuestEvent) + Send + Sync>>,
) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if data.len() < capture_limit {
            let take = (capture_limit - data.len()).min(chunk.len());
            data.extend_from_slice(&chunk[..take]);
        }
        if let Some(sink) = &sink {
            let terminal = TerminalEvent {
                stream: if stream == 0 {
                    TerminalStream::Stdout
                } else {
                    TerminalStream::Stderr
                },
                data: String::from_utf8_lossy(chunk).into_owned(),
            };
            let payload = serde_json::to_vec(&terminal).unwrap_or_default();
            sink(GuestEvent::new("terminal.output", payload));
        }
    }
    Ok(data)
}

fn bounded(bytes: &[u8], max: usize) -> String {
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(max)]).into_owned();
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Owns a spawned child for the duration of `exec` and guarantees it is
/// terminated and reaped when the function exits through any error path after
/// spawn (validation failure, cancel, deadline, reader failure). Disarmed on
/// the success path once the child has been waited on.
struct ChildGuard {
    child: std::process::Child,
    pid: u32,
    armed: bool,
}

impl ChildGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            terminate_and_reap(&mut self.child, self.pid);
        }
    }
}

/// Terminate a child and reap it, bounded so a stuck kill cannot block the
/// cancel path forever. On Unix the child is placed in its own process group
/// by `pre_exec(setpgid)`, so the whole group (including descendants) is
/// killed. On platforms without process groups (e.g. Windows) only the direct
/// child is terminated; descendant-tree cancellation is Unix-only.
#[cfg_attr(not(unix), allow(unused_variables))]
fn terminate_and_reap(child: &mut std::process::Child, pid: u32) {
    #[cfg(unix)]
    kill_group(pid);
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}
