//! Spawned-command execution for the forkd agent, including the shell-free
//! echo/true/false builtins and bounded `exec` semantics.

use super::builtin::{builtin, builtin_result, validate_builtin_request};
use super::transport::guest_path;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io;
use std::time::Duration;
use tokio::process::{Child, Command};

/// Build a tokio `Command` from the request's `args`/`cwd`/`env`.
pub fn command_from(request: &Value) -> io::Result<Command> {
    let args = request
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "args must be an array"))?;
    let strings: Vec<&str> = args
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "args must be strings"))
        })
        .collect::<Result<_, _>>()?;
    let (program, rest) = strings
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "args must not be empty"))?;
    let mut command = Command::new(program);
    command.args(rest);
    let cwd = guest_path(request.get("cwd"), true)?;
    command.current_dir(cwd);
    if let Some(env) = request.get("env") {
        let map = env
            .as_object()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "env must be an object"))?;
        let vars: HashMap<&str, &str> = map
            .iter()
            .map(|(k, v)| {
                v.as_str().map(|s| (k.as_str(), s)).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "env values must be strings")
                })
            })
            .collect::<Result<_, _>>()?;
        command.envs(vars);
    }
    Ok(command)
}

/// Configure a spawned child: kill-on-drop, piped or null stdio, and (on Unix)
/// a fresh process group so the agent can terminate a timed-out command's
/// descendants too.
pub fn prepare_process(command: &mut Command, piped: bool) {
    use std::process::Stdio;
    command
        .kill_on_drop(true)
        .stdin(if piped { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Same contract as a `pre_exec` `setpgid(0, 0)`, but expressed through
        // `process_group` so std can keep using `posix_spawn`: a `pre_exec`
        // hook forces fork+exec, and concurrent commands all fork the same
        // large parent, which serializes them on the parent's `mmap_lock`.
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

/// Per-stream capture cap for `exec`: the response carries each stream twice
/// (out/stdout, err/stderr aliases), so 128 KiB per stream keeps the
/// serialized response far below the 1 MiB wire limit. Output beyond the cap
/// is drained but dropped, and `truncated` is set.
const MAX_EXEC_STREAM_BYTES: usize = 128 * 1024;

/// Read a child stream to EOF, keeping at most `limit` bytes. Draining past
/// the cap keeps the child from blocking on a full pipe.
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> io::Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if data.len() < limit {
            let take = (limit - data.len()).min(n);
            data.extend_from_slice(&buf[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((data, truncated))
}

pub async fn execute(request: &Value) -> io::Result<Value> {
    let timeout = request
        .get("timeout")
        .and_then(Value::as_u64)
        .map(Duration::from_secs);
    if let Some(kind) = builtin(request) {
        // Validate the complete request (including cwd/env and argument types)
        // even though the builtin does not spawn a process.
        validate_builtin_request(request)?;
        return Ok(builtin_result(request, kind));
    }
    let mut command = command_from(request)?;
    prepare_process(&mut command, false);
    let mut child = command.spawn()?;
    let child_id = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;
    // Drain both pipes concurrently and wait for exit in one future: a
    // sequential read would deadlock against a child that fills the pipe we
    // are not reading. Captures are bounded, so a runaway writer cannot grow
    // host memory before the wire-size check runs.
    // Boxed: the joined future is large (three concurrent readers plus the
    // child), and an unboxed one bloats every caller frame holding it.
    let wait = Box::pin(async {
        let (out, err, status) = tokio::try_join!(
            read_bounded(&mut stdout, MAX_EXEC_STREAM_BYTES),
            read_bounded(&mut stderr, MAX_EXEC_STREAM_BYTES),
            child.wait(),
        )?;
        Ok::<_, io::Error>((out, err, status))
    });
    let (out, err, truncated, status) = match timeout {
        Some(limit) => match tokio::time::timeout(limit, wait).await {
            Ok(result) => {
                let ((out, out_truncated), (err, err_truncated), status) = result?;
                (out, err, out_truncated || err_truncated, status)
            }
            Err(_) => {
                // The wait future owns the child while it is polled. Kill its
                // process group before dropping the cancelled future so a timed
                // out command cannot outlive the request.
                terminate_id(child_id).await;
                // PROTOCOL.md §2.4: the exec terminal shape is
                // exit_code/stdout/stderr/timed_out — a timeout is a normal
                // terminal outcome, NOT an `error` string (the host classifies
                // any `error` as a fatal Remote failure and would never expose
                // `timed_out`).
                return Ok(json!({
                    "out": "",
                    "err": "process timeout",
                    "stdout": "",
                    "stderr": "process timeout",
                    "exit_code": null,
                    "timed_out": true,
                    "truncated": false
                }));
            }
        },
        None => {
            let ((out, out_truncated), (err, err_truncated), status) = wait.await?;
            (out, err, out_truncated || err_truncated, status)
        }
    };
    Ok(json!({
        "out": String::from_utf8_lossy(&out),
        "err": String::from_utf8_lossy(&err),
        "stdout": String::from_utf8_lossy(&out),
        "stderr": String::from_utf8_lossy(&err),
        "exit_code": status.code(),
        "error": null,
        "timed_out": false,
        "truncated": truncated
    }))
}

/// Kill the whole process group of a spawned child (used for timeouts). The
/// child is reaped by its own wait; this only terminates the group.
pub async fn terminate_id(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // Kill the process GROUP (negative pid). Guard the cast: uids above
        // i32::MAX would wrap and target a different group.
        if let Ok(group) = i32::try_from(pid) {
            // SAFETY: kill(-pgid, SIGKILL) with a validated positive pgid;
            // no memory is shared with the kernel for this call.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

pub async fn terminate(child: &mut Child) {
    terminate_id(child.id()).await;
    let _ = child.kill().await;
}
