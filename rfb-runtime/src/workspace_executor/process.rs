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
        if let Some(mut stdin) = child.stdin.take() {
            if let Some(input) = value.get("stdin").and_then(Value::as_str) {
                stdin
                    .write_all(input.as_bytes())
                    .map_err(|e| e.to_string())?;
            }
            drop(stdin);
        }
        let pid = child.id();
        let stdout = child.stdout.take().ok_or("stdout pipe unavailable")?;
        let stderr = child.stderr.take().ok_or("stderr pipe unavailable")?;
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
        // Reader completion arrives through the channel; the handles are
        // detached (they end at pipe EOF, including the kill-on-cancel path).
        let stderr_tx = tx.clone();
        std::thread::spawn(move || {
            let result = read_stream(stdout, 0, capture_limit, stdout_sink);
            let _ = tx.send((0, result));
        });
        std::thread::spawn(move || {
            let result = read_stream(stderr, 1, capture_limit, stderr_sink);
            let _ = stderr_tx.send((1, result));
        });
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
                        break child.wait().map_err(|e| e.to_string())?;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::SeqCst) {
                        terminate_and_reap(&mut child, pid);
                        // Do not block on the reader threads: on Linux the killed
                        // process group closes the pipes so they drain immediately; on
                        // Windows a surviving descendant may hold the pipe open. The
                        // threads end when this process exits.
                        return Err("request cancelled".into());
                    }
                    if std::time::Instant::now() >= deadline {
                        terminate_and_reap(&mut child, pid);
                        return Err("command timed out".into());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("stream reader failed".into());
                }
            }
        };
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
