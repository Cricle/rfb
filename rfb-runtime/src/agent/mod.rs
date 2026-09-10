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
fn ping_response() -> Value {
    json!({
        "pong": true,
        "numpy_version": "not-installed",
        "pid": std::process::id(),
        "agent_lang": "rust",
        "warmup_ready": false,
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
pub async fn run(addr: &str) -> io::Result<()> {
    std::fs::create_dir_all(transport::workspace_root())?;
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = handle_connection(stream).await;
        });
    }
}

async fn handle_connection(stream: TcpStream) -> io::Result<()> {
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut writer = BufWriter::new(write);
    let mut line = Vec::new();
    loop {
        line.clear();
        // Bounded line read: read_until would buffer the entire stream before
        // the size check could run, so a newline-free stream grows `line`
        // without bound. Scan with fill_buf instead, stop storing once the
        // cap is hit, and close the connection on overflow.
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
        if !complete || overflow {
            if complete || overflow {
                write_json(&mut writer, json!({"error":"line too large"})).await?;
            }
            return Ok(());
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            continue;
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
            "exec" => execute(&request).await,
            "stream" => {
                stream_process(&request, &mut reader, &mut writer).await?;
                return Ok(());
            }
            "ls" | "find" | "grep" | "read" | "write" | "eval" => structured(&request).await,
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
