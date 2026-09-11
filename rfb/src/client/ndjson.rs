//! forkd guest NDJSON response mapping (`sdk/PROTOCOL.md` §2.2/§2.4). The
//! transport itself is the crate's existing [`ForkdGuestClient`]; this module
//! only turns terminal JSON values into facade types.

use serde_json::Value;

use super::types::{ExecResult, StreamEvent, StreamEventKind};
use super::RfbError;

/// Decode `out`/`stdout`-shaped values: UTF-8 string, byte array, or null.
pub(super) fn value_bytes(value: Option<&Value>) -> Result<Vec<u8>, RfbError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(text.as_bytes().to_vec()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| RfbError::decode("output must be a UTF-8 string or byte array"))
            })
            .collect(),
        Some(_) => Err(RfbError::decode(
            "output must be a UTF-8 string or byte array",
        )),
    }
}

/// Map an `exec` terminal line to the unified result.
pub(super) fn exec_result(value: &Value) -> Result<ExecResult, RfbError> {
    Ok(ExecResult {
        exit_code: value
            .get("exit_code")
            .and_then(Value::as_i64)
            // Clamp, never wrap: an out-of-range wire code maps to -1.
            .map(|code| i32::try_from(code).unwrap_or(-1))
            .unwrap_or(-1),
        // UNIFIED_API.md §4: the current keys win when both are present.
        stdout: value_bytes(value.get("stdout").or_else(|| value.get("out")))?,
        stderr: value_bytes(value.get("stderr").or_else(|| value.get("err")))?,
        timed_out: value
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Map an `eval` terminal line to the unified result; `output` becomes stdout.
pub(super) fn eval_result(value: &Value) -> Result<ExecResult, RfbError> {
    Ok(ExecResult {
        exit_code: value
            .get("exit_code")
            .or_else(|| value.get("status"))
            .and_then(Value::as_i64)
            .map(|code| i32::try_from(code).unwrap_or(0))
            .unwrap_or(0),
        stdout: value_bytes(value.get("output").or_else(|| value.get("out")))?,
        stderr: Vec::new(),
        timed_out: value
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Map a `ping` response to the healthy flag.
pub(super) fn ping_healthy(value: &Value) -> bool {
    value.get("pong").and_then(Value::as_bool).unwrap_or(false)
}

/// Map one stream event line to the unified event type. `Ok(None)` means the
/// peer closed the stream cleanly.
pub(super) fn stream_event(value: Value) -> Result<Option<StreamEvent>, RfbError> {
    if value.get("started").and_then(Value::as_bool) == Some(true)
        || value.get("stream").and_then(Value::as_str) == Some("started")
        || value.get("event").and_then(Value::as_str) == Some("started")
    {
        return Ok(Some(StreamEvent {
            kind: StreamEventKind::Started,
            data: Vec::new(),
            code: None,
        }));
    }
    if let Some(code) = value.get("exit_code").and_then(Value::as_i64) {
        return Ok(Some(StreamEvent {
            kind: StreamEventKind::Exit,
            data: Vec::new(),
            code: Some(i32::try_from(code).unwrap_or(-1)),
        }));
    }
    if value.get("done").and_then(Value::as_bool) == Some(true) {
        return Ok(Some(StreamEvent {
            kind: StreamEventKind::Exit,
            data: Vec::new(),
            code: None,
        }));
    }
    for (key, stderr) in [
        ("stdout", false),
        ("out", false),
        ("stderr", true),
        ("err", true),
    ] {
        if value.get(key).is_some() {
            return Ok(Some(StreamEvent {
                kind: if stderr {
                    StreamEventKind::Stderr
                } else {
                    StreamEventKind::Stdout
                },
                data: value_bytes(value.get(key))?,
                code: None,
            }));
        }
    }
    // PROTOCOL.md §2.5: unrecognized event keys are ignored (None) so future
    // frame additions do not break existing clients.
    Ok(None)
}
