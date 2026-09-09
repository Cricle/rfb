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
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
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
    let child = command.spawn()?;
    let child_id = child.id();
    let output = if let Some(limit) = timeout {
        match tokio::time::timeout(limit, child.wait_with_output()).await {
            Ok(output) => output?,
            Err(_) => {
                // `wait_with_output` owns the child while it is polled. Kill its
                // process group before dropping the cancelled future so a timed
                // out command cannot outlive the request.
                terminate_id(child_id).await;
                return Ok(json!({
                    "out": "",
                    "err": "process timeout",
                    "stdout": "",
                    "stderr": "process timeout",
                    "error": "process timeout",
                    "exit_code": null,
                    "timed_out": true
                }));
            }
        }
    } else {
        child.wait_with_output().await?
    };
    Ok(json!({
        "out": String::from_utf8_lossy(&output.stdout),
        "err": String::from_utf8_lossy(&output.stderr),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "exit_code": output.status.code(),
        "error": null,
        "timed_out": false
    }))
}

/// Kill the whole process group of a spawned child (used for timeouts). The
/// child is reaped by its own wait; this only terminates the group.
pub async fn terminate_id(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

pub async fn terminate(child: &mut Child) {
    terminate_id(child.id()).await;
    let _ = child.kill().await;
}
