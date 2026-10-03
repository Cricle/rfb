//! Tool metadata, request decode/error helpers, and the shared structured-op
//! dispatch (`invoke`, `edit`, `stream`).

use super::capability::AdkCapability;
use super::execute::execute_schema;
use rfb::{guest, Sandbox, SandboxError};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub(super) fn name(c: AdkCapability) -> &'static str {
    match c {
        AdkCapability::Bash => "bash",
        AdkCapability::Edit => "edit",
        AdkCapability::Execute => "execute",
        AdkCapability::Ping => "ping",
        AdkCapability::Stream => "stream",
        AdkCapability::Read => "read",
        AdkCapability::Write => "write",
        AdkCapability::Grep => "grep",
        AdkCapability::Find => "find",
        AdkCapability::Ls => "ls",
        AdkCapability::Eval => "eval",
        AdkCapability::Cancel => "cancel",
    }
}

pub(super) fn description(c: AdkCapability) -> &'static str {
    match c {
        AdkCapability::Bash | AdkCapability::Execute => {
            "Execute a shell command in the live RFB sandbox and return its status, \
             stdout and stderr. The command runs through the guest shell \
             (`/bin/sh -c`), so pipelines, `&&`, and redirections work."
        }
        AdkCapability::Edit => {
            "Replace `old_text` with `new_text` in a guest file (exact match; \
             without replace_all the text must match exactly once)."
        }
        AdkCapability::Ping => "Check guest liveness.",
        AdkCapability::Stream => {
            "Run a command in the live sandbox and collect its streamed output \
             events (started/stdout/stderr/exit) until the command exits."
        }
        AdkCapability::Read => "Read a guest file (optionally offset + max_bytes). Returns bytes.",
        AdkCapability::Write => {
            "Write bytes to a guest file (append or overwrite; optional chmod \
             `mode` in octal, e.g. 493 = 0o755). Returns bytes written."
        }
        AdkCapability::Grep => {
            "Substring search over a guest directory recursively (or one file); \
             matches are per line ({path, line, text})."
        }
        AdkCapability::Find => "Find guest files by glob name pattern (recursive).",
        AdkCapability::Ls => "List a guest directory.",
        AdkCapability::Eval => "Evaluate a code snippet in the guest interpreter.",
        AdkCapability::Cancel => "Cancel the guest's in-flight operation.",
    }
}

pub(super) fn schema(c: AdkCapability) -> Value {
    match c {
        AdkCapability::Bash | AdkCapability::Execute => execute_schema(),
        AdkCapability::Edit => {
            json!({"type":"object","properties":{"path":{"type":"string","minLength":1},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_text","new_text"],"additionalProperties":false})
        }
        AdkCapability::Ping | AdkCapability::Cancel => {
            json!({"type":"object","additionalProperties":false})
        }
        AdkCapability::Stream => {
            json!({"type":"object","properties":{"command":{"type":"string","minLength":1},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"},"pty":{"type":"boolean"},"env":{"type":"array"},"timeout":{"type":"integer","minimum":1}},"required":["command"],"additionalProperties":false})
        }
        AdkCapability::Read => {
            json!({"type":"object","properties":{"path":{"type":"string","minLength":1},"offset":{"type":"integer","minimum":0},"max_bytes":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false})
        }
        AdkCapability::Write => {
            json!({"type":"object","properties":{"path":{"type":"string","minLength":1},"data":{"type":"array","items":{"type":"integer","minimum":0,"maximum":255}},"append":{"type":"boolean"},"mode":{"type":"integer","minimum":0}},"required":["path","data"],"additionalProperties":false})
        }
        AdkCapability::Find | AdkCapability::Grep => {
            json!({"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"},"max_results":{"type":"integer","minimum":1},"max_bytes":{"type":"integer","minimum":1}},"required":["pattern"],"additionalProperties":false})
        }
        AdkCapability::Ls => {
            json!({"type":"object","properties":{"path":{"type":"string"},"max_results":{"type":"integer","minimum":1}},"additionalProperties":false})
        }
        AdkCapability::Eval => {
            json!({"type":"object","properties":{"cwd":{"type":"string"},"code":{"type":"string","minLength":1},"timeout":{"type":"integer","minimum":1}},"required":["code"],"additionalProperties":false})
        }
    }
}

pub(super) fn decode<T: DeserializeOwned>(v: Value) -> Result<T, adk_rust::AdkError> {
    serde_json::from_value(v).map_err(|e| invalid(e.to_string()))
}

pub(super) fn invalid(s: impl Into<String>) -> adk_rust::AdkError {
    adk_rust::AdkError::new(
        adk_rust::ErrorComponent::Tool,
        adk_rust::ErrorCategory::InvalidInput,
        "rfb.tool.invalid_args",
        s,
    )
}

pub(super) fn unsupported(c: AdkCapability) -> adk_rust::AdkError {
    adk_rust::AdkError::new(
        adk_rust::ErrorComponent::Tool,
        adk_rust::ErrorCategory::Unsupported,
        "rfb.tool.unsupported_capability",
        format!("unsupported capability: {c:?}"),
    )
}

/// Map a sandbox error onto the ADK error vocabulary. The category decides the
/// framework's retry hint (Timeout/Unavailable are retryable), so the mapping
/// below is deliberate: a guest deadline miss reads as Timeout, an unreadable
/// backend as Unavailable, a spec violation as caller input.
///
/// Redaction property (carried over from the rig adapter): the model-visible
/// `message` is generic for Transport/Execution/Timeout — raw diagnostics ride
/// in `details.metadata["diagnostics"]` for operator-facing logs only. Spec
/// validation text stays in the message: it is the actionable feedback the
/// model needs to correct its arguments.
pub fn sandbox_error_to_adk(e: SandboxError) -> adk_rust::AdkError {
    use adk_rust::{ErrorCategory, ErrorComponent};
    let (category, code, message, diagnostics): (
        ErrorCategory,
        &'static str,
        String,
        Option<String>,
    ) = match e {
        SandboxError::Timeout => (
            ErrorCategory::Timeout,
            "rfb.tool.execution_timeout",
            "guest operation timed out".into(),
            None,
        ),
        SandboxError::InvalidSpec(x) => (
            ErrorCategory::InvalidInput,
            "rfb.tool.invalid_spec",
            x.to_string(),
            None,
        ),
        SandboxError::UnsupportedCapability(_) => (
            ErrorCategory::Unsupported,
            "rfb.tool.unsupported_capability",
            "unsupported capability".into(),
            None,
        ),
        SandboxError::Transport(x) => (
            ErrorCategory::Unavailable,
            "rfb.tool.transport",
            "the sandbox is unreachable".into(),
            Some(x),
        ),
        SandboxError::Execution(x) => (
            ErrorCategory::Internal,
            "rfb.tool.execution",
            "guest operation failed".into(),
            Some(x),
        ),
        SandboxError::NotReady => (
            ErrorCategory::Unavailable,
            "rfb.tool.not_ready",
            "sandbox is not ready".into(),
            None,
        ),
    };
    let mut error = adk_rust::AdkError::new(ErrorComponent::Tool, category, code, message);
    if let Some(diagnostics) = diagnostics {
        error
            .details
            .metadata
            .insert("diagnostics".into(), Value::String(diagnostics));
    }
    error
}

pub(super) async fn invoke(
    sb: Arc<dyn Sandbox>,
    c: AdkCapability,
    v: Value,
) -> Result<Value, adk_rust::AdkError> {
    // The capability gate fires at REGISTRATION time (SandboxTools only
    // registers advertised capabilities). The execute path skips it here:
    // the legacy `sandbox_execute` tool is registered explicitly by the
    // caller and must work on any sandbox that implements `exec`, whatever
    // its capability list claims (some backends advertise less than they
    // serve — the probe contract).
    if !matches!(c, AdkCapability::Bash | AdkCapability::Execute) && !c.available(sb.as_ref()) {
        return Err(unsupported(c));
    }
    let result: Result<serde_json::Value, adk_rust::AdkError> = match c {
        AdkCapability::Bash | AdkCapability::Execute => {
            return execute(sb, v).await;
        }
        AdkCapability::Edit => edit(sb, v).await,
        AdkCapability::Ping => sb
            .ping()
            .await
            .map(|p| serde_json::to_value(p).unwrap_or(Value::Bool(false)))
            .map_err(sandbox_error_to_adk),
        AdkCapability::Ls => {
            let x: guest::LsRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            sb.ls(x)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(sandbox_error_to_adk)
        }
        AdkCapability::Find => {
            let x: guest::FindRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            sb.find(x)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(sandbox_error_to_adk)
        }
        AdkCapability::Grep => {
            let x: guest::GrepRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            sb.grep(x)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(sandbox_error_to_adk)
        }
        AdkCapability::Read => {
            let x: guest::ReadRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            sb.read_file(x)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(sandbox_error_to_adk)
        }
        AdkCapability::Write => {
            let x: guest::WriteRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            sb.write_file(x)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(sandbox_error_to_adk)
        }
        AdkCapability::Eval => {
            let x: guest::EvalRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            sb.eval(x)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(sandbox_error_to_adk)
        }
        AdkCapability::Cancel => {
            let x: guest::CancelRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            sb.cancel(x)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(sandbox_error_to_adk)
        }
        AdkCapability::Stream => {
            let x: StreamArgs = decode(v)?;
            stream(sb, x).await
        }
    };
    result
}

async fn execute(sb: Arc<dyn Sandbox>, v: Value) -> Result<Value, adk_rust::AdkError> {
    let a: ExecuteArgs = decode(v)?;
    // The bash/execute tool speaks a shell-string contract: the command is
    // run through the guest shell (`/bin/sh -c`), so pipelines, `&&`, and
    // redirections work as the model expects. Structured `args` are
    // appended as positional arguments for direct-exec callers.
    let mut s = rfb::ExecSpec::new("/bin/sh");
    s.args = vec!["-c".into(), a.command.clone()];
    s.args.extend(a.args);
    s.stdin = a.stdin;
    s.cwd = a.cwd;
    s.timeout = a.timeout_ms.map(Duration::from_millis);
    s.validate().map_err(|e| invalid(e.to_string()))?;
    let r = sb.exec(s).await.map_err(sandbox_error_to_adk)?;
    if r.timed_out {
        return Err(adk_rust::AdkError::new(
            adk_rust::ErrorComponent::Tool,
            adk_rust::ErrorCategory::Timeout,
            "rfb.tool.execution_timeout",
            "command execution timed out",
        ));
    }
    Ok(
        json!({"status":r.status,"stdout":String::from_utf8_lossy(&r.stdout),"stderr":String::from_utf8_lossy(&r.stderr),"timed_out":false,"liveness":super::Liveness::CoreOnly.as_str()}),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecuteArgs {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: Option<Vec<u8>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

async fn edit(sb: Arc<dyn Sandbox>, value: Value) -> Result<Value, adk_rust::AdkError> {
    let args: EditArgs = decode(value)?;
    let read = guest::ReadRequest::new(args.path.clone());
    read.validate().map_err(|e| invalid(e.to_string()))?;
    let content = sb.read_file(read).await.map_err(sandbox_error_to_adk)?;
    // A truncated read means the backend hit its per-read byte cap: writing
    // the replacement back would destroy the unseen tail of the file, so
    // fail closed instead.
    if content.truncated {
        return Err(invalid(
            "file exceeds the backend read cap; edit is not supported for this file",
        ));
    }
    let text = String::from_utf8(content.data).map_err(|_| invalid("file is not valid UTF-8"))?;
    if args.old_text.is_empty() {
        return Err(invalid("old_text must not be empty"));
    }
    let count = text.match_indices(&args.old_text).count();
    if count == 0 {
        return Err(invalid("old_text was not found"));
    }
    if !args.replace_all && count != 1 {
        return Err(invalid("old_text must match exactly once"));
    }
    let updated = if args.replace_all {
        text.replace(&args.old_text, &args.new_text)
    } else {
        text.replacen(&args.old_text, &args.new_text, 1)
    };
    let write = guest::WriteRequest::new(args.path, updated.into_bytes());
    write.validate().map_err(|e| invalid(e.to_string()))?;
    sb.write_file(write)
        .await
        .map_err(sandbox_error_to_adk)
        .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamArgs {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    pty: Option<bool>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    timeout: Option<u64>,
}

async fn stream(sb: Arc<dyn Sandbox>, args: StreamArgs) -> Result<Value, adk_rust::AdkError> {
    let spec = guest::StreamSpec {
        command: args.command,
        args: args.args,
        cwd: args.cwd,
        pty: args.pty,
        env: args.env,
        timeout: args.timeout.map(Duration::from_millis),
    };
    spec.validate().map_err(|e| invalid(e.to_string()))?;
    let mut stream = sb.stream(spec).await.map_err(sandbox_error_to_adk)?;
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await.map_err(sandbox_error_to_adk)? {
        events.push(event);
    }
    Ok(serde_json::to_value(events).unwrap_or(Value::Null))
}
