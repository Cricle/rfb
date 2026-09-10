use serde_json::Value;
use std::time::Duration;

// Compatibility aliases for the former forkd-local DTO names. The canonical
// definitions live in `crate::guest`, so aliases preserve identical serde
// fields and defaults without maintaining a second wire contract.
pub use crate::guest::{
    FindRequest as GuestFindRequest, GrepRequest as GuestGrepRequest, LsRequest as GuestLsRequest,
};
pub use crate::guest::{
    MAX_GUEST_PATH_BYTES, MAX_GUEST_PATTERN_BYTES, MAX_GUEST_RESULTS, MAX_GUEST_RESULT_BYTES,
};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
/// Errors returned by the forkd guest client.
pub enum ForkdGuestError {
    /// Guest transport I/O failure.
    #[error("guest transport failed: {0}")]
    Io(#[from] std::io::Error),
    /// Guest response exceeded the maximum line size.
    #[error("guest response exceeded {MAX_LINE_BYTES} bytes")]
    TooLarge,
    /// Invalid JSON received from the guest.
    #[error("invalid guest JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The guest reported an error.
    #[error("guest returned error: {0}")]
    Remote(String),
    /// A guest path failed validation.
    #[error("invalid guest path")]
    InvalidPath,
    /// The guest result exceeded the configured limit.
    #[error("guest result limit exceeded")]
    LimitExceeded,
    /// The requested RPC tool is not supported by the guest.
    #[error("unsupported guest RPC tool: {0}")]
    UnsupportedGuestRpc(String),
}

/// TCP client for a forkd guest speaking newline-delimited JSON.
#[derive(Clone, Debug)]
pub struct ForkdGuestClient {
    /// Guest TCP address.
    pub address: String,
    /// Request timeout.
    pub timeout: Duration,
}

/// A bidirectional newline-delimited JSON forkd guest session.
/// The connection remains open while output events are consumed and input is sent.
pub struct ForkdGuestStream {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    timeout: Duration,
    stopped: bool,
    terminal: bool,
}

impl ForkdGuestClient {
    /// Create a client for the given guest TCP address.
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            timeout: Duration::from_secs(10),
        }
    }

    /// Send a raw action JSON value and collect the response lines.
    pub async fn request(&self, action: Value) -> Result<Vec<Value>, ForkdGuestError> {
        let stream = tokio::time::timeout(self.timeout, TcpStream::connect(&self.address))
            .await
            .map_err(|_| timed_out("guest connect timeout"))??;
        let (read, mut write) = stream.into_split();
        write_json(&mut write, &action, self.timeout).await?;
        let mut reader = BufReader::new(read);
        let mut responses = Vec::new();
        loop {
            let value = match read_json_line(&mut reader, self.timeout).await? {
                Some(value) => value,
                None => {
                    return Err(ForkdGuestError::Remote(
                        "guest closed before response".into(),
                    ));
                }
            };
            check_remote_error(&value)?;
            responses.push(value);
            if responses.last().is_some_and(|v| {
                v.get("exit_code").is_some()
                    || v.get("pong").is_some()
                    || v.get("results").is_some()
                    || v.get("entries").is_some()
                    || v.get("matches").is_some()
                    || v.get("data").is_some()
                    || v.get("content").is_some()
                    || v.get("output").is_some()
                    || v.get("status").is_some()
                    || v.get("ok").is_some()
                    || v.get("healthy").is_some()
                    || v.get("done").is_some()
                    || v.get("cancelled").is_some()
                    || v.get("bytes_written").is_some()
            }) {
                break;
            }
        }
        Ok(responses)
    }

    /// Start a stream on one TCP connection. `cwd` is an opaque guest path.
    pub async fn stream(
        &self,
        args: Vec<String>,
        cwd: Option<&str>,
        pty: Option<bool>,
        env: Option<Value>,
    ) -> Result<ForkdGuestStream, ForkdGuestError> {
        let tcp = tokio::time::timeout(self.timeout, TcpStream::connect(&self.address))
            .await
            .map_err(|_| timed_out("guest connect timeout"))??;
        let (read, mut writer) = tcp.into_split();
        let mut action = serde_json::json!({"action": "stream", "args": args});
        if let Some(cwd) = cwd {
            action["cwd"] = Value::String(cwd.to_owned());
        }
        if let Some(pty) = pty {
            action["pty"] = Value::Bool(pty);
        }
        if let Some(env) = env {
            action["env"] = env;
        }
        write_json(&mut writer, &action, self.timeout).await?;
        Ok(ForkdGuestStream {
            reader: BufReader::new(read),
            writer,
            timeout: self.timeout,
            stopped: false,
            terminal: false,
        })
    }

    /// Ping the guest and return its response value.
    pub async fn ping(&self) -> Result<Value, ForkdGuestError> {
        self.request(serde_json::json!({"action":"ping"}))
            .await?
            .pop()
            .ok_or_else(|| ForkdGuestError::Remote("empty ping response".into()))
    }

    /// Execute one of the structured, read-only guest filesystem RPCs.
    pub async fn execute_tool(&self, tool: &str, args: Value) -> Result<Value, ForkdGuestError> {
        let request = match tool {
            "ls" => {
                let req: GuestLsRequest = serde_json::from_value(args)?;
                req.validate()
                    .map_err(|error| ForkdGuestError::Remote(error.to_string()))?;
                serde_json::to_value(req)?
            }
            "find" => {
                let req: GuestFindRequest = serde_json::from_value(args)?;
                req.validate()
                    .map_err(|error| ForkdGuestError::Remote(error.to_string()))?;
                serde_json::to_value(req)?
            }
            "grep" => {
                let req: GuestGrepRequest = serde_json::from_value(args)?;
                req.validate()
                    .map_err(|error| ForkdGuestError::Remote(error.to_string()))?;
                serde_json::to_value(req)?
            }
            "read" => {
                let req: crate::guest::ReadRequest = serde_json::from_value(args)?;
                req.validate()
                    .map_err(|e| ForkdGuestError::Remote(e.to_string()))?;
                serde_json::to_value(req)?
            }
            "write" => {
                let req: crate::guest::WriteRequest = serde_json::from_value(args)?;
                req.validate()
                    .map_err(|e| ForkdGuestError::Remote(e.to_string()))?;
                serde_json::to_value(req)?
            }
            "eval" => {
                let req: crate::guest::EvalRequest = serde_json::from_value(args)?;
                req.validate()
                    .map_err(|e| ForkdGuestError::Remote(e.to_string()))?;
                serde_json::to_value(req)?
            }
            other => return Err(ForkdGuestError::UnsupportedGuestRpc(other.to_owned())),
        };
        let mut action = request;
        action["action"] = Value::String(tool.to_owned());
        let value = self
            .request(action)
            .await?
            .pop()
            .ok_or_else(|| ForkdGuestError::Remote("empty tool response".into()))?;
        let encoded = serde_json::to_vec(&value)?;
        if encoded.len() > MAX_GUEST_RESULT_BYTES {
            return Err(ForkdGuestError::TooLarge);
        }
        if let Some(results) = value.get("results").and_then(Value::as_array) {
            if results.len() > MAX_GUEST_RESULTS {
                return Err(ForkdGuestError::LimitExceeded);
            }
        }
        Ok(value)
    }

    /// Execute a command in the guest root directory.
    pub async fn exec(
        &self,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<Value, ForkdGuestError> {
        self.exec_in("/", args, timeout_secs).await
    }

    /// Execute a command in a path interpreted by the guest runtime.
    /// `guest_cwd` is opaque and is never converted to a host path.
    pub async fn exec_in(
        &self,
        guest_cwd: &str,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<Value, ForkdGuestError> {
        self.request(
            serde_json::json!({"action":"exec","cwd":guest_cwd,"args":args,"timeout":timeout_secs}),
        )
        .await?
        .pop()
        .ok_or_else(|| ForkdGuestError::Remote("empty exec response".into()))
    }

    /// Evaluate code in the guest root directory.
    pub async fn eval(&self, code: impl Into<String>) -> Result<Value, ForkdGuestError> {
        self.eval_in("/", code).await
    }

    /// Evaluate code in a guest working directory. `guest_cwd` remains opaque.
    pub async fn eval_in(
        &self,
        guest_cwd: &str,
        code: impl Into<String>,
    ) -> Result<Value, ForkdGuestError> {
        self.request(serde_json::json!({"action":"eval","cwd":guest_cwd,"code":code.into()}))
            .await?
            .pop()
            .ok_or_else(|| ForkdGuestError::Remote("empty eval response".into()))
    }

    /// Send the typed eval request while preserving its wire-level fields.
    /// RFB durations are milliseconds, whereas forkd's eval timeout is seconds.
    pub async fn eval_request(
        &self,
        request: crate::guest::EvalRequest,
    ) -> Result<Value, ForkdGuestError> {
        request
            .validate()
            .map_err(|error| ForkdGuestError::Remote(error.to_string()))?;
        let mut action = serde_json::json!({"action": "eval", "code": request.code});
        if let Some(cwd) = request.cwd {
            action["cwd"] = Value::String(cwd);
        }
        if let Some(timeout) = request.timeout {
            let seconds = timeout
                .as_secs()
                .saturating_add(u64::from(timeout.subsec_nanos() != 0))
                .max(1);
            action["timeout"] = Value::Number(seconds.into());
        }
        self.request(action)
            .await?
            .pop()
            .ok_or_else(|| ForkdGuestError::Remote("empty eval response".into()))
    }
}

impl ForkdGuestStream {
    /// Read the next protocol event. A terminal exit event is returned normally.
    pub async fn next_event(&mut self) -> Result<Option<Value>, ForkdGuestError> {
        let value = read_json_line(&mut self.reader, self.timeout).await?;
        if let Some(ref value) = value {
            check_remote_error(value)?;
            if value.get("exit_code").is_some() {
                self.terminal = true;
            }
        }
        Ok(value)
    }

    /// Send input to the running guest stream.
    pub async fn send_input(&mut self, input: impl Into<String>) -> Result<(), ForkdGuestError> {
        if self.terminal || self.stopped {
            return Err(ForkdGuestError::Remote(
                "guest stream is no longer running".into(),
            ));
        }
        write_json(
            &mut self.writer,
            &serde_json::json!({"in": input.into()}),
            self.timeout,
        )
        .await
    }

    /// Ask the guest stream to terminate.
    pub async fn stop(&mut self) -> Result<(), ForkdGuestError> {
        if self.terminal || self.stopped {
            return Ok(());
        }
        self.stopped = true;
        write_json(
            &mut self.writer,
            &serde_json::json!({"action": "stop"}),
            self.timeout,
        )
        .await
    }
}

fn timed_out(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, message)
}

async fn write_json<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
    timeout: Duration,
) -> Result<(), ForkdGuestError> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    tokio::time::timeout(timeout, writer.write_all(&line))
        .await
        .map_err(|_| timed_out("guest write timeout"))??;
    tokio::time::timeout(timeout, writer.flush())
        .await
        .map_err(|_| timed_out("guest flush timeout"))??;
    Ok(())
}

async fn read_json_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    timeout: Duration,
) -> Result<Option<Value>, ForkdGuestError> {
    loop {
        let mut buf = Vec::new();
        let mut limited = reader.take((MAX_LINE_BYTES + 1) as u64);
        let n = tokio::time::timeout(timeout, limited.read_until(b'\n', &mut buf))
            .await
            .map_err(|_| timed_out("guest response timeout"))??;
        if n == 0 {
            return Ok(None);
        }
        if buf.len() > MAX_LINE_BYTES || !buf.ends_with(b"\n") {
            return Err(ForkdGuestError::TooLarge);
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        if buf.is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_slice(&buf)?));
    }
}

fn check_remote_error(value: &Value) -> Result<(), ForkdGuestError> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(ForkdGuestError::Remote(error.to_owned()));
    }
    Ok(())
}
