//! Cancellable, bounded process execution for the workspace executor.

use super::WorkspaceGuestExecutor;
use crate::runtime_service::GuestEvent;
use crate::session::{TerminalEvent, TerminalStream};
use crate::utf8_boundary::Utf8ChunkDecoder;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Per-`read` chunk size for draining a child's stdout/stderr. 32 KiB keeps
/// the syscall count (and per-chunk live event overhead) low for chatty
/// children while staying far below the ~64 KiB pipe buffer, so the drain
/// loop still wakes promptly on partial output.
const READ_CHUNK: usize = 32 * 1024;

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
        // Reject zero timeouts before the builtin short-circuit too: the same
        // request must fail identically whether or not the applet is shadowed
        // by an in-process builtin.
        if matches!(value.get("timeout_ms").and_then(Value::as_u64), Some(0))
            || matches!(value.get("timeout_secs").and_then(Value::as_u64), Some(0))
        {
            return Err("timeout_secs/timeout_ms must be non-zero".into());
        }
        if let Some(kind) = crate::builtin::builtin(value) {
            // Same applets the forkd agent serves in-process. Forking them
            // would pay the guest's serialized spawn cost — measured at
            // ~0.7 ms per command and unaffected by vCPU count — for commands
            // that need no process at all.
            crate::builtin::validate_builtin_request(value).map_err(|e| e.to_string())?;
            // The working directory is still policy-checked even though the
            // builtin never uses it: an out-of-workspace cwd must fail closed
            // on every transport.
            let cwd = value.get("cwd").and_then(Value::as_str).unwrap_or(".");
            self.policy.workspace_path(cwd).map_err(|e| e.to_string())?;
            let result = crate::builtin::builtin_result(value, kind);
            let stdout = result.get("stdout").cloned().unwrap_or(Value::Null);
            if let Some(text) = stdout.as_str() {
                forward_chunk(&self.event_sink, 0, text.as_bytes());
            } else if let Some(items) = stdout.as_array() {
                // builtin 的二进制输出（字节数组形态）——逐字节前转。
                let bytes: Vec<u8> = items
                    .iter()
                    .filter_map(|v| v.as_u64().and_then(|v| u8::try_from(v).ok()))
                    .collect();
                forward_chunk(&self.event_sink, 0, &bytes);
            }
            let exit_code = result.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
            return Ok(json!({
                "stdout": stdout,
                "stderr": "",
                "exit_code": exit_code,
                "success": exit_code == 0,
            }));
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
        {
            // A dedicated process group is what lets cancel/timeout tear down
            // the whole descendant tree. `process_group` expresses exactly the
            // `setpgid(0, 0)` a `pre_exec` hook would, but leaves std free to
            // spawn through `posix_spawn` (vfork-based). A `pre_exec` hook
            // forces fork+exec instead, and every command forks the same large
            // parent (the guest runtime), so the parent's `mmap_lock` — held
            // for writing while the page tables are copied — serializes
            // concurrent commands regardless of how many vCPUs the guest has.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
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
        let sinks = [sink.clone(), sink];
        // Readers must drain stdout/stderr BEFORE stdin is written: a child
        // that fills its output pipe (~64 KiB) before consuming stdin would
        // otherwise block a synchronous write here forever, while the allowed
        // stdin size reaches ~16 MiB.
        let stdin = guard.child.stdin.take();
        // stdin arrives either base64-encoded (`stdin_b64`, the compact ZBRT
        // wire form — a JSON byte array of a 16 MiB payload allocates ~30x
        // its size inside the guest), as a plain string (legacy), or as a
        // JSON byte array (legacy; still accepted for wire compatibility).
        let input_bytes: Option<Vec<u8>> = match value.get("stdin_b64") {
            Some(Value::String(text)) => {
                use base64::Engine as _;
                Some(
                    base64::engine::general_purpose::STANDARD
                        .decode(text.as_bytes())
                        .map_err(|e| format!("stdin_b64 is not valid base64: {e}"))?,
                )
            }
            Some(_) => return Err("stdin_b64 must be a base64 string".into()),
            None => match value.get("stdin") {
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
            },
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

        // Unix drains both pipes from this thread with `poll`: every command
        // would otherwise spawn two reader threads, and thread creation
        // serializes on the shared address space's mmap_lock — which capped
        // concurrent commands at a few thousand per second no matter how many
        // vCPUs the guest had. Other platforms keep the reader-thread path.
        #[cfg(unix)]
        let (status, reader_results) = {
            let mut pipes: [Box<dyn ChildPipe>; 2] = [Box::new(stdout), Box::new(stderr)];
            for pipe in pipes.iter() {
                // SAFETY: fcntl on a pipe this process owns; O_NONBLOCK only
                // changes how our own reads behave.
                unsafe {
                    let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                }
            }
            let mut data: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
            let mut done = [false; 2];
            let mut failure: [Option<String>; 2] = [None, None];
            // One boundary-aware decoder per stream: a multi-byte character
            // straddling a READ_CHUNK boundary must not be split into U+FFFD.
            let mut decoders = [Utf8ChunkDecoder::new(), Utf8ChunkDecoder::new()];
            let mut buf = vec![0u8; READ_CHUNK];
            while !(done[0] && done[1]) {
                if cancel.load(Ordering::SeqCst) {
                    return Err("request cancelled".into());
                }
                if std::time::Instant::now() >= deadline {
                    return Err("command timed out".into());
                }
                let mut poll_fds: [libc::pollfd; 2] = [
                    libc::pollfd {
                        fd: -1,
                        events: 0,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: -1,
                        events: 0,
                        revents: 0,
                    },
                ];
                for (index, pipe) in pipes.iter().enumerate() {
                    if !done[index] {
                        poll_fds[index] = libc::pollfd {
                            fd: pipe.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                    }
                }
                // SAFETY: two initialized pollfds, bounded timeout.
                let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 20) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(format!("poll child pipes: {error}"));
                }
                for index in 0..2 {
                    if done[index] || poll_fds[index].revents == 0 {
                        continue;
                    }
                    loop {
                        match pipes[index].read(&mut buf) {
                            Ok(0) => {
                                // True EOF: emit any withheld partial sequence.
                                let tail = decoders[index].flush_bytes();
                                if !tail.is_empty() {
                                    forward_chunk(&sinks[index], index as u8, &tail);
                                }
                                done[index] = true;
                                break;
                            }
                            Ok(n) => capture_chunk(
                                &mut data[index],
                                &buf[..n],
                                capture_limit,
                                &sinks[index],
                                index as u8,
                                &mut decoders[index],
                            ),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue
                            }
                            Err(error) => {
                                done[index] = true;
                                failure[index] = Some(error.to_string());
                                break;
                            }
                        }
                    }
                }
            }
            // Both pipes hit EOF but the child may still be alive (it closed
            // its own stdout/stderr). Poll try_wait so cancel/deadline stay
            // reachable instead of blocking indefinitely inside wait().
            // Backoff escalates 1→2→4→…→20 ms: the child normally exits as
            // its stdio closes (first poll usually wins), but a child that
            // closed its own stdio and stays alive would otherwise spin at
            // ~1000 wakeups/s until the deadline.
            let mut poll_sleep = Duration::from_millis(1);
            let status = loop {
                match guard.child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {}
                    Err(error) => return Err(error.to_string()),
                }
                if cancel.load(Ordering::SeqCst) {
                    return Err("request cancelled".into());
                }
                if std::time::Instant::now() >= deadline {
                    return Err("command timed out".into());
                }
                poll_sleep = (poll_sleep * 2).min(Duration::from_millis(20));
                std::thread::sleep(poll_sleep);
            };
            let results = [
                match failure[0].take() {
                    Some(message) => Some(Err(std::io::Error::other(message))),
                    None => Some(Ok(std::mem::take(&mut data[0]))),
                },
                match failure[1].take() {
                    Some(message) => Some(Err(std::io::Error::other(message))),
                    None => Some(Ok(std::mem::take(&mut data[1]))),
                },
            ];
            (status, results)
        };

        #[cfg(not(unix))]
        let (status, mut reader_results) = capture_with_threads(
            stdout,
            stderr,
            &sinks,
            capture_limit,
            &mut guard.child,
            cancel,
            deadline,
        )?;

        // Success: the child has been waited on, so the error-path guard is
        // no longer needed.
        guard.disarm();
        #[cfg(unix)]
        let mut reader_results = reader_results;
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

/// Drain both child pipes with reader threads. Kept for platforms without
/// `poll`; the Unix path in [`WorkspaceGuestExecutor::exec`] reads inline.
#[cfg(not(unix))]
type CapturedStreams = [Option<std::io::Result<Vec<u8>>>; 2];

#[cfg(not(unix))]
fn capture_with_threads(
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    sinks: &[Option<Sink>; 2],
    capture_limit: usize,
    child: &mut std::process::Child,
    cancel: &AtomicBool,
    deadline: std::time::Instant,
) -> Result<(std::process::ExitStatus, CapturedStreams), String> {
    let (tx, rx) = std::sync::mpsc::channel::<(u8, std::io::Result<Vec<u8>>)>();
    let stderr_tx = tx.clone();
    let stdout_sink = sinks[0].clone();
    let stderr_sink = sinks[1].clone();
    std::thread::spawn(move || {
        // One boundary-aware decoder per stream so a multi-byte character
        // straddling a read boundary is not split into U+FFFD replacements.
        let mut decoder = Utf8ChunkDecoder::new();
        let result = read_stream(stdout, 0, capture_limit, stdout_sink, &mut decoder);
        let _ = tx.send((0, result));
    });
    std::thread::spawn(move || {
        let mut decoder = Utf8ChunkDecoder::new();
        let result = read_stream(stderr, 1, capture_limit, stderr_sink, &mut decoder);
        let _ = stderr_tx.send((1, result));
    });
    let mut reader_results: [Option<std::io::Result<Vec<u8>>>; 2] = [None, None];
    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok((stream, result)) => {
                reader_results[stream as usize] = Some(result);
                if reader_results.iter().all(Option::is_some) {
                    // Both pipes hit EOF but the child may still be alive (it
                    // closed its own stdout/stderr). Poll try_wait so
                    // cancel/deadline stay reachable instead of blocking
                    // indefinitely inside wait().
                    loop {
                        match child.try_wait() {
                            Ok(Some(status)) => return Ok((status, reader_results)),
                            Ok(None) => {}
                            Err(error) => return Err(error.to_string()),
                        }
                        if cancel.load(Ordering::SeqCst) {
                            return Err("request cancelled".into());
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err("command timed out".into());
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::SeqCst) {
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
    }
}

/// Forward one chunk to a live consumer when the transport attached one.
/// Bytes are carried EXACTLY: valid UTF-8 rides `data` (compact); anything
/// else rides `data_bytes` (the ZBRT frame layer forwards it raw — no
/// U+FFFD corruption on binary output).
fn forward_chunk(sink: &Option<Sink>, stream: u8, bytes: &[u8]) {
    if let Some(sink) = sink {
        if bytes.is_empty() {
            return;
        }
        let terminal = match std::str::from_utf8(bytes) {
            Ok(text) => TerminalEvent {
                stream: if stream == 0 {
                    TerminalStream::Stdout
                } else {
                    TerminalStream::Stderr
                },
                data: text.to_owned(),
                data_bytes: None,
            },
            Err(_) => TerminalEvent {
                stream: if stream == 0 {
                    TerminalStream::Stdout
                } else {
                    TerminalStream::Stderr
                },
                data: String::new(),
                data_bytes: Some(bytes.to_vec()),
            },
        };
        let payload = serde_json::to_vec(&terminal).unwrap_or_default();
        sink(GuestEvent::new("terminal.output", payload));
    }
}

/// One chunk of a child stream: capture it under the byte limit and forward
/// the boundary-safe decoded text to a live consumer.
fn capture_chunk(
    data: &mut Vec<u8>,
    chunk: &[u8],
    capture_limit: usize,
    sink: &Option<Sink>,
    stream: u8,
    decoder: &mut Utf8ChunkDecoder,
) {
    if data.len() < capture_limit {
        let take = (capture_limit - data.len()).min(chunk.len());
        data.extend_from_slice(&chunk[..take]);
    }
    // A multi-byte character straddling a read boundary must not be split:
    // the decoder withholds an incomplete trailing sequence and joins it with
    // the next chunk (byte-exact — binary output rides data_bytes).
    forward_chunk(sink, stream, &decoder.decode_bytes(chunk));
}

/// Minimal surface the Unix capture loop needs from a child pipe.
#[cfg(unix)]
trait ChildPipe: Read + std::os::fd::AsRawFd {}
#[cfg(unix)]
impl ChildPipe for std::process::ChildStdout {}
#[cfg(unix)]
impl ChildPipe for std::process::ChildStderr {}

/// Live consumer for streamed command output.
type Sink = Arc<dyn Fn(GuestEvent) + Send + Sync>;

/// Read one child stream to EOF, forwarding each chunk as a live
/// `terminal.output` event when a sink is attached, and returning the bounded
/// aggregated bytes for the terminal result payload.
#[cfg(not(unix))]
fn read_stream(
    mut reader: impl Read,
    stream: u8,
    capture_limit: usize,
    sink: Option<Sink>,
    decoder: &mut Utf8ChunkDecoder,
) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            // True EOF: emit any withheld partial sequence.
            let tail = decoder.flush_bytes();
            if !tail.is_empty() {
                forward_chunk(&sink, stream, &tail);
            }
            break;
        }
        capture_chunk(&mut data, &buf[..n], capture_limit, &sink, stream, decoder);
    }
    Ok(data)
}

fn bounded(bytes: &[u8], max: usize) -> Value {
    // 线契约（UNIFIED_API §4 / forkd_value_bytes）：有效 UTF-8 → 字符串；
    // 否则 → 字节数组（保真；U+FFFD 替换曾在 64KB 二进制上膨胀 2× 且毁数据）。
    // cap 切在码点中间 ≠ 二进制：先按边界回退重试，真二进制才走数组。
    let slice = &bytes[..bytes.len().min(max)];
    match std::str::from_utf8(slice) {
        Ok(text) => Value::String(text.to_owned()),
        Err(_) => {
            // cap 切在码点中间（悬空 lead/continuation）≠ 二进制：
            // utf8_safe_split 找稳定前缀重试；仍无效 = 真二进制才走数组。
            let mut decoder = crate::utf8_boundary::Utf8ChunkDecoder::new();
            let stable = decoder.decode_bytes(slice);
            match std::str::from_utf8(&stable) {
                Ok(text) => Value::String(text.to_owned()),
                Err(_) => Value::Array(slice.iter().map(|b| Value::Number((*b).into())).collect()),
            }
        }
    }
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
/// by `CommandExt::process_group(0)`, so the whole group (including
/// descendants) is killed. On platforms without process groups (e.g. Windows)
/// only the direct child is terminated; descendant-tree cancellation is
/// Unix-only.
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
    // Negative pid targets the process group; guard the cast so uids above
    // i32::MAX cannot wrap into a different group.
    if let Ok(group) = i32::try_from(pid) {
        // SAFETY: kill(-pgid, SIGKILL) with a validated positive pgid.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}
