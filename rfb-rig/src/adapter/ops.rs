//! Tool metadata, request decode/error helpers, and the shared structured-op
//! dispatch (`invoke`, `edit`, `stream`).

use super::capability::RigCapability;
use super::execute::SandboxExecuteAdapter;
use rfb::{guest, Sandbox, SandboxError};
use rig_core::tool::{ToolErrorKind, ToolExecutionError, ToolOutput};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub(super) fn metadata(c: RigCapability) -> (&'static str, &'static str, Value) {
    (
        match c {
            RigCapability::Bash => "bash",
            RigCapability::Edit => "edit",
            RigCapability::Execute => "execute",
            RigCapability::Ping => "ping",
            RigCapability::Stream => "stream",
            RigCapability::Read => "read",
            RigCapability::Write => "write",
            RigCapability::Grep => "grep",
            RigCapability::Find => "find",
            RigCapability::Ls => "ls",
            RigCapability::Eval => "eval",
            RigCapability::Cancel => "cancel",
        },
        "RFB guest operation.",
        schema(c),
    )
}

fn schema(c: RigCapability) -> Value {
    match c {
        RigCapability::Bash | RigCapability::Execute => super::execute::execute_schema(),
        RigCapability::Edit => {
            json!({"type":"object","properties":{"path":{"type":"string","minLength":1},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_text","new_text"],"additionalProperties":false})
        }
        RigCapability::Ping | RigCapability::Cancel => {
            json!({"type":"object","additionalProperties":false})
        }
        RigCapability::Stream => {
            json!({"type":"object","properties":{"command":{"type":"string","minLength":1},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"},"pty":{"type":"boolean"},"env":{"type":"array"},"timeout":{"type":"integer","minimum":1}},"required":["command"],"additionalProperties":false})
        }
        RigCapability::Read => {
            json!({"type":"object","properties":{"path":{"type":"string","minLength":1},"offset":{"type":"integer","minimum":0},"max_bytes":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false})
        }
        RigCapability::Write => {
            json!({"type":"object","properties":{"path":{"type":"string","minLength":1},"data":{"type":"array","items":{"type":"integer","minimum":0,"maximum":255}},"append":{"type":"boolean"},"mode":{"type":"integer","minimum":0}},"required":["path","data"],"additionalProperties":false})
        }
        RigCapability::Find | RigCapability::Grep => {
            json!({"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"},"max_results":{"type":"integer","minimum":1},"max_bytes":{"type":"integer","minimum":1}},"required":["pattern"],"additionalProperties":false})
        }
        RigCapability::Ls => {
            json!({"type":"object","properties":{"path":{"type":"string"},"max_results":{"type":"integer","minimum":1}},"additionalProperties":false})
        }
        RigCapability::Eval => {
            json!({"type":"object","properties":{"cwd":{"type":"string"},"code":{"type":"string","minLength":1},"timeout":{"type":"integer","minimum":1}},"required":["code"],"additionalProperties":false})
        }
    }
}

pub(super) fn decode<T: DeserializeOwned>(v: Value) -> Result<T, ToolExecutionError> {
    serde_json::from_value(v).map_err(|e| invalid(e.to_string()))
}
pub(super) fn invalid(s: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::new(ToolErrorKind::InvalidArgs, s.into())
}
pub(super) fn unsupported(c: RigCapability) -> ToolExecutionError {
    ToolExecutionError::permission_denied(format!("unsupported capability: {c:?}"))
        .with_code("unsupported_capability")
        .redact_model_feedback()
}

pub(super) async fn invoke(
    sb: Arc<dyn Sandbox>,
    c: RigCapability,
    v: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    if !c.available(sb.as_ref()) {
        return Err(unsupported(c));
    }
    match c {
        RigCapability::Bash | RigCapability::Execute => {
            SandboxExecuteAdapter::from_arc(sb).execute(v).await
        }
        RigCapability::Edit => edit(sb, v).await,
        RigCapability::Ping => output(sb.ping().await),
        RigCapability::Ls => {
            let x: guest::LsRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            output(sb.ls(x).await)
        }
        RigCapability::Find => {
            let x: guest::FindRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            output(sb.find(x).await)
        }
        RigCapability::Grep => {
            let x: guest::GrepRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            output(sb.grep(x).await)
        }
        RigCapability::Read => {
            let x: guest::ReadRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            output(sb.read_file(x).await)
        }
        RigCapability::Write => {
            let x: guest::WriteRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            output(sb.write_file(x).await)
        }
        RigCapability::Eval => {
            let x: guest::EvalRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            output(sb.eval(x).await)
        }
        RigCapability::Cancel => {
            let x: guest::CancelRequest = decode(v)?;
            x.validate().map_err(|e| invalid(e.to_string()))?;
            output(sb.cancel(x).await)
        }
        RigCapability::Stream => {
            let x: StreamArgs = decode(v)?;
            stream(sb, x).await
        }
    }
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

async fn edit(sb: Arc<dyn Sandbox>, value: Value) -> Result<ToolOutput, ToolExecutionError> {
    let args: EditArgs = decode(value)?;
    let read = guest::ReadRequest::new(args.path.clone());
    read.validate().map_err(|e| invalid(e.to_string()))?;
    let content = sb.read_file(read).await.map_err(map_sandbox_error)?;
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
    output(sb.write_file(write).await)
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

async fn stream(sb: Arc<dyn Sandbox>, args: StreamArgs) -> Result<ToolOutput, ToolExecutionError> {
    let spec = guest::StreamSpec {
        command: args.command,
        args: args.args,
        cwd: args.cwd,
        pty: args.pty,
        env: args.env,
        timeout: args.timeout.map(Duration::from_millis),
    };
    spec.validate().map_err(|e| invalid(e.to_string()))?;
    let mut stream = sb.stream(spec).await.map_err(map_sandbox_error)?;
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await.map_err(map_sandbox_error)? {
        events.push(event);
    }
    Ok(ToolOutput::json(serde_json::to_value(events).unwrap()))
}

fn output<T: serde::Serialize>(
    r: Result<T, SandboxError>,
) -> Result<ToolOutput, ToolExecutionError> {
    Ok(ToolOutput::json(
        serde_json::to_value(r.map_err(map_sandbox_error)?).unwrap(),
    ))
}
pub(super) fn map_sandbox_error(e: SandboxError) -> ToolExecutionError {
    match e {
        SandboxError::Timeout => ToolExecutionError::timeout("guest operation timed out")
            .with_code("execution_timeout")
            .redact_model_feedback(),
        SandboxError::InvalidSpec(x) => invalid(x.to_string()).with_code("invalid_spec"),
        SandboxError::UnsupportedCapability(_) => {
            ToolExecutionError::permission_denied("unsupported capability")
                .with_code("unsupported_capability")
                .redact_model_feedback()
        }
        SandboxError::Transport(x) => ToolExecutionError::network(x)
            .with_code("transport")
            .redact_model_feedback(),
        SandboxError::Execution(x) => ToolExecutionError::other(x)
            .with_code("execution")
            .redact_model_feedback(),
        SandboxError::NotReady => ToolExecutionError::other("sandbox is not ready")
            .with_code("not_ready")
            .redact_model_feedback(),
    }
}
