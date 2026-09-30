//! Newline-delimited JSON guest agent used by forkd.
#![allow(clippy::possible_missing_else)]

mod builtin;
mod process_exec;
mod search;
mod stream;
mod transport;

use process_exec::execute;
use search::structured;
use serde_json::{json, Value};
use std::io;
use std::time::Duration;
use stream::stream_process;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

/// Default TCP address the forkd agent binds when `FORKD_AGENT_ADDR` is unset.
pub const DEFAULT_ADDR: &str = "0.0.0.0:8888";
pub(super) const MAX_LINE: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;

/// Effective guest PATH reported by the official `ping` `path` field.
const DEFAULT_GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Build the `ping` response with the official forkd field set.
///
/// The official Python agent reports `pong`, `numpy_version`, `pid`,
/// `agent_lang`, `warmup_ready`, and `path`. The Rust replacement keeps the
/// same keys so callers that depend on the official field semantics still see
/// them. Values that only the Python/Node interpreter can produce stay at
/// their honest absent state: this replacement does not embed numpy, does not
/// run a Node warmup bridge, and reports its own language as `rust`.
///
/// `protocol_version` is the additive NDJSON protocol revision of this agent
/// (1 = the original forkd NDJSON contract plus agent-token auth). Clients
/// must treat unknown response keys as opaque.
fn ping_response() -> Value {
    json!({
        "pong": true,
        "numpy_version": "not-installed",
        "pid": std::process::id(),
        "agent_lang": "rust",
        "warmup_ready": false,
        "protocol_version": 1,
        "path": container_path(),
    })
}

/// The effective guest PATH reported by `path`, mirroring the official agent's
/// `/etc/environment` handling: the pam_env-style file is the canonical source,
/// and the official default is used when the file is missing or has no `PATH=`
/// line. The agent's own process environment is deliberately not consulted —
/// the official agent treats a leaked host PATH as untrusted.
fn container_path() -> String {
    if let Ok(contents) = std::fs::read_to_string("/etc/environment") {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(value) = line.strip_prefix("PATH=") {
                return value.trim().to_owned();
            }
        }
    }
    DEFAULT_GUEST_PATH.to_owned()
}

/// Run the forkd NDJSON guest agent on the given TCP address.
///
/// The agent-token gate is sourced from the same environment the bind address
/// comes from: `FORKD_AGENT_TOKEN`. Unset/blank means open access with zero
/// behavior change; a configured token requires every new connection to
/// authenticate before any action is dispatched.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn run(addr: &str) -> io::Result<()> {
    run_with_token(addr, agent_token_from_env().as_deref()).await
}

/// The effective `FORKD_AGENT_TOKEN` value. Blank values are ignored, exactly
/// like the other `FORKD_*`/`RFB_*` string toggles.
fn agent_token_from_env() -> Option<String> {
    std::env::var("FORKD_AGENT_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Run the forkd NDJSON guest agent with an explicit token gate (`None` keeps
/// the historical open-access behavior). Host-side contract tests use this to
/// exercise both modes without mutating process-wide state.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn run_with_token(addr: &str, token: Option<&str>) -> io::Result<()> {
    std::fs::create_dir_all(transport::workspace_root())?;
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let expected = token.map(str::to_owned);
        tokio::spawn(async move {
            let _ = Box::pin(handle_connection(stream, expected)).await;
        });
    }
}

/// Deadline for the first (auth) frame when a token is configured.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// One parsed first frame plus how it terminated.
enum Incoming {
    /// A complete line (terminator or EOF trimmed) with at least one byte.
    Line(Vec<u8>),
    /// Clean EOF with no buffered bytes.
    Eof,
    /// The line exceeded `MAX_LINE` and was not buffered.
    Overflow,
    /// A blank line (no content between newlines).
    Blank,
}

/// Read one bounded NDJSON line using the same fill_buf discipline as the
/// main dispatch loop: a newline-free stream can never grow `line` past
/// `MAX_LINE` because bytes past the cap are dropped, not stored.
async fn read_incoming(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> io::Result<Incoming> {
    let mut line = Vec::new();
    let mut overflow = false;
    let complete = loop {
        let available = match reader.fill_buf().await {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break !line.is_empty();
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                let take = pos + 1;
                if line.len() + take > MAX_LINE {
                    overflow = true;
                } else {
                    line.extend_from_slice(&available[..take]);
                }
                reader.consume(take);
                break true;
            }
            None => {
                if line.len() + available.len() > MAX_LINE {
                    overflow = true;
                }
                if !overflow {
                    line.extend_from_slice(available);
                }
                let take = available.len();
                reader.consume(take);
            }
        }
    };
    if overflow {
        // Overflow wins over EOF so the caller still reports "line too large"
        // when a hostile stream ends without a newline past the cap.
        return Ok(Incoming::Overflow);
    }
    if !complete {
        return Ok(Incoming::Eof);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    if line.is_empty() {
        return Ok(Incoming::Blank);
    }
    Ok(Incoming::Line(line))
}

/// Constant-time byte comparison (XOR accumulation). The running OR of
/// `a[i] ^ b[i]` never short-circuits, so comparison time depends only on the
/// input lengths — a leaked token-length oracle is accepted, a timing oracle
/// over the content is not.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u64;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = u64::from(*a.get(i).unwrap_or(&0));
        let y = u64::from(*b.get(i).unwrap_or(&0));
        diff |= x ^ y;
    }
    diff == 0
}

/// Enforce the agent-token handshake on a freshly accepted connection.
///
/// Returns `true` when the connection may proceed to normal dispatch. On any
/// failure the response is written and the caller must close the connection.
/// The first frame must arrive within [`AUTH_TIMEOUT`]; a silent client is
/// disconnected without a response.
async fn authenticate(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: &mut BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    expected: &str,
) -> io::Result<bool> {
    let first = match tokio::time::timeout(AUTH_TIMEOUT, read_incoming(reader)).await {
        Ok(Ok(Incoming::Line(line))) => line,
        // Timeout, EOF, or line overflow: fail closed, no error frame.
        _ => return Ok(false),
    };
    let request: Value = match serde_json::from_slice(&first) {
        Ok(v) => v,
        Err(_) => {
            write_json(writer, json!({"error":"authentication required"})).await?;
            return Ok(false);
        }
    };
    let is_auth = request.get("action").and_then(Value::as_str) == Some("auth");
    if !is_auth {
        write_json(writer, json!({"error":"authentication required"})).await?;
        return Ok(false);
    }
    let provided = request
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if constant_time_eq(expected.as_bytes(), provided.as_bytes()) {
        write_json(writer, json!({"action":"auth","ok":true})).await?;
        Ok(true)
    } else {
        write_json(
            writer,
            json!({"action":"auth","ok":false,"error":"authentication failed"}),
        )
        .await?;
        Ok(false)
    }
}

async fn handle_connection(stream: TcpStream, token: Option<String>) -> io::Result<()> {
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut writer = BufWriter::new(write);
    if let Some(expected) = token.as_deref() {
        if !authenticate(&mut reader, &mut writer, expected).await? {
            return Ok(());
        }
    }
    let mut line;
    loop {
        // Bounded line read: read_until would buffer the entire stream before
        // the size check could run, so a newline-free stream grows `line`
        // without bound (the fill_buf scan lives in `read_incoming`).
        let incoming = read_incoming(&mut reader).await?;
        match incoming {
            Incoming::Eof => return Ok(()),
            Incoming::Overflow => {
                write_json(&mut writer, json!({"error":"line too large"})).await?;
                return Ok(());
            }
            Incoming::Blank => continue,
            Incoming::Line(bytes) => line = bytes,
        }
        let request: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(e) => {
                write_json(&mut writer, json!({"error":format!("invalid JSON: {e}")})).await?;
                continue;
            }
        };
        let action = request.get("action").and_then(Value::as_str).unwrap_or("");
        let result = match action {
            "ping" => Ok(ping_response()),
            "exec" => Box::pin(execute(&request)).await,
            "stream" => {
                stream_process(&request, &mut reader, &mut writer).await?;
                return Ok(());
            }
            "ls" | "find" | "grep" | "read" | "write" | "eval" => {
                Box::pin(structured(&request)).await
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown action: {action}"),
            )),
        };
        write_json(
            &mut writer,
            result.unwrap_or_else(|e| json!({"error":e.to_string(),"exit_code":1})),
        )
        .await?;
    }
}

pub(super) async fn write_json<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: Value,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE {
        bytes =
            serde_json::to_vec(&json!({"error":"response too large"})).map_err(io::Error::other)?;
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}
