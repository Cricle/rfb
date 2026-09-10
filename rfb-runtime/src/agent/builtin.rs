//! Shell-free builtins (echo/true/false/netprobe) for the forkd agent. The
//! guest runs images without a shell, so these are handled in-process.

use super::transport::guest_path;
use serde_json::{json, Value};
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A shell-free command implemented directly by the guest agent.
#[derive(Clone, Copy)]
pub enum Builtin {
    /// Write the request arguments to standard output.
    Echo,
    /// Complete successfully without producing output.
    True,
    /// Complete unsuccessfully without producing output.
    False,
    /// Probe internet egress: exits 0 only when a TCP connection succeeds.
    NetProbe,
}

/// Return a builtin for a bare command name (no PATH is consulted) or an
/// absolute path whose basename matches. A relative path like `tools/echo` is
/// a workspace file the caller addressed explicitly, so it is spawned instead
/// of being shadowed by the builtin.
///
/// ```ignore
/// use rfb_runtime::agent::builtin::{builtin, Builtin};
/// let request = serde_json::json!({"args": ["/bin/echo", "hello"]});
/// assert!(matches!(builtin(&request), Some(Builtin::Echo)));
/// assert!(builtin(&serde_json::json!({"args": ["echoish"]})).is_none());
/// assert!(builtin(&serde_json::json!({"args": ["tools/echo"]})).is_none());
/// ```
pub fn builtin(request: &Value) -> Option<Builtin> {
    let program = request
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| args.first())
        .and_then(Value::as_str)?;
    if program.contains('/') && !program.starts_with('/') {
        return None;
    }
    match program.rsplit('/').next()? {
        "echo" => Some(Builtin::Echo),
        "true" => Some(Builtin::True),
        "false" => Some(Builtin::False),
        "netprobe" => Some(Builtin::NetProbe),
        _ => None,
    }
}

/// Validate the argument, working-directory, and environment fields of a builtin request.
pub fn validate_builtin_request(request: &Value) -> io::Result<()> {
    let args = request
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "args must be an array"))?;
    if args.is_empty() || args.iter().any(|value| !value.is_string()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "args must be non-empty strings",
        ));
    }
    if let Some(cwd) = request.get("cwd") {
        // Apply the same path policy when a cwd is explicitly supplied.
        let _ = guest_path(Some(cwd), true)?;
    }
    if let Some(env) = request.get("env") {
        let map = env
            .as_object()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "env must be an object"))?;
        if map.values().any(|value| !value.is_string()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "env values must be strings",
            ));
        }
    }
    Ok(())
}

/// Produce the protocol result for a validated builtin invocation.
///
/// ```ignore
/// use rfb_runtime::agent::builtin::{builtin_result, Builtin};
/// let result = builtin_result(&serde_json::json!({"args": ["echo", "hello", "world"]}), Builtin::Echo);
/// assert_eq!(result["out"], "hello world\n");
/// assert_eq!(result["exit_code"], 0);
/// ```
pub fn builtin_result(request: &Value, builtin: Builtin) -> Value {
    let args = request["args"].as_array().expect("validated builtin args");
    let (out, exit_code) = match builtin {
        Builtin::Echo => (
            args.iter()
                .skip(1)
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
                + "\n",
            0,
        ),
        Builtin::True => (String::new(), 0),
        Builtin::False => (String::new(), 1),
        // Sandbox no-egress contract probe: connect to an internet target
        // (default 1.1.1.1:80, overridable) with a bounded timeout. Exit 0
        // means the guest HAS egress — the E2E layer asserts this never
        // happens; exit 1 means egress is blocked as contracted.
        Builtin::NetProbe => {
            let target = args.get(1).and_then(Value::as_str).unwrap_or("1.1.1.1:80");
            let reachable = target
                .to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next())
                .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok())
                .is_some();
            if reachable {
                (format!("connected to {target}\n"), 0)
            } else {
                (format!("no route to {target}\n"), 1)
            }
        }
    };
    // Provide both the RFB-internal `out`/`err` aliases and the official
    // `stdout`/`stderr` field names used by the forkd Python agent's exec wire.
    json!({
        "out": out.clone(),
        "err": "",
        "stdout": out.clone(),
        "stderr": "",
        "exit_code": exit_code,
        "error": null,
        "timed_out": false
    })
}
