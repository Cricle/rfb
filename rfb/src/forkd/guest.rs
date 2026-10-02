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

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Absolute ceiling on the JSON-encoded form of one guest tool response. The
/// contract limit (`MAX_GUEST_RESULT_BYTES`, 50 KiB) is enforced on raw
/// payload bytes; this 1 MiB encoded ceiling is the backstop against a
/// genuinely malformed or hostile response, not the contract limit.
const MAX_GUEST_RESULT_ENCODED_BYTES: usize = 1024 * 1024;

/// Extra client read budget beyond an exec's guest-side deadline, so the
/// guest's own timeout error (not the client's) is what surfaces.
pub(crate) const EXEC_READ_MARGIN: Duration = Duration::from_secs(5);

/// Environment variable carrying the forkd agent token. When set to a
/// non-empty value, every new TCP connection must start with one auth frame
/// before any business frame (same contract as the Python SDK's
/// `AGENT_TOKEN_ENV` and the agent side in rfb-runtime `agent/mod.rs`).
pub const AGENT_TOKEN_ENV: &str = "FORKD_AGENT_TOKEN";

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

/// 池条目：reader + writer 半边 + 最后使用时刻。
type PooledNdjsonConn = (BufReader<OwnedReadHalf>, OwnedWriteHalf, std::time::Instant);

/// TCP client for a forkd guest speaking newline-delimited JSON.
#[derive(Clone, Debug)]
pub struct ForkdGuestClient {
    /// Guest TCP address.
    pub address: String,
    /// Request timeout.
    pub timeout: Duration,
    /// 统一温连接池：agent 的 serve 循环可顺序承载多个请求，每操作新建
    /// TCP 连接的握手/拆除 ≈ 0.4ms/次。条目 = (reader, writer, 最后使用)。
    /// 并发 = 池中多条连接各服务一个操作（每连接同时一个请求）。
    pool: Arc<Mutex<Vec<PooledNdjsonConn>>>,
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
            pool: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Set the request timeout (builder style; the pool is untouched).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send a raw action JSON value and collect the response lines.
    ///
    /// The collected set is capped (aggregate bytes + line count) so a
    /// hostile or broken guest streaming non-terminal lines cannot grow host
    /// memory without bound; the loop otherwise ends only on a terminal key.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn request(&self, action: Value) -> Result<Vec<Value>, ForkdGuestError> {
        self.request_with_read_timeout(action, self.timeout).await
    }

    /// Like [`Self::request`] but with an explicit per-read deadline. The
    /// response read timeout must cover the guest-side work: an exec whose
    /// contract timeout is 60 s needs a client read budget beyond 60 s, or the
    /// client gives up before the guest's own deadline fires.
    async fn request_with_read_timeout(
        &self,
        action: Value,
        read_timeout: Duration,
    ) -> Result<Vec<Value>, ForkdGuestError> {
        // 统一温池：锁内只 pop；空闲 <1s 的连接直接用（agent 不关空闲连接，
        // 热路径零额外开销）；超龄连接直接丢弃重连（ndjson 无握手、TCP
        // connect 本身就是验证，~0.2ms）。借出的连接上失败 = 请求可能已在
        // guest 执行，绝不重试（连接丢弃，错误原样返回）。
        loop {
            let borrowed = {
                let mut pool = self.pool.lock().await;
                pool.pop()
            };
            match borrowed {
                Some((mut reader, mut write, last_used)) => {
                    if std::time::Instant::now().duration_since(last_used) < Duration::from_secs(1)
                    {
                        let result = self
                            .exchange(&mut reader, &mut write, action, read_timeout)
                            .await;
                        if result.is_ok() && reader.buffer().is_empty() {
                            let mut pool = self.pool.lock().await;
                            if pool.len() < 16 {
                                pool.push((reader, write, std::time::Instant::now()));
                            } // else: dropped = closed（缓冲有残余 = 不可复用）
                        }
                        // 失败或缓冲残留：连接丢弃（drop = 关闭）
                        return result;
                    }
                    // 超龄：shutdown 写半边（reader 随 drop 关闭），试下一条。
                    let _ = write.shutdown().await;
                    continue;
                }
                None => break,
            }
        }
        let stream = tokio::time::timeout(self.timeout, TcpStream::connect(&self.address))
            .await
            .map_err(|_| timed_out("guest connect timeout"))??;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        // The auth frame must be the first frame on a fresh connection, before
        // the business frame, or a token-gated agent answers
        // {"error":"authentication required"} and closes the connection.
        authenticate(&mut reader, &mut write, self.timeout).await?;
        let result = self
            .exchange(&mut reader, &mut write, action, read_timeout)
            .await?;
        if reader.buffer().is_empty() {
            let mut pool = self.pool.lock().await;
            if pool.len() < 16 {
                pool.push((reader, write, std::time::Instant::now()));
            }
        }
        Ok(result)
    }

    /// One request/response exchange over an established (authenticated)
    /// connection. Errors leave the connection unusable (caller drops it).
    async fn exchange(
        &self,
        reader: &mut BufReader<OwnedReadHalf>,
        write: &mut OwnedWriteHalf,
        action: Value,
        read_timeout: Duration,
    ) -> Result<Vec<Value>, ForkdGuestError> {
        const MAX_RESPONSE_BYTES: usize = crate::core::MAX_GUEST_PAYLOAD_BYTES;
        const MAX_RESPONSE_LINES: usize = 65_536;
        write_json(write, &action, self.timeout).await?;
        let mut responses = Vec::new();
        let mut collected_bytes = 0usize;
        loop {
            let value = match read_json_line(reader, read_timeout).await? {
                Some(value) => value,
                None => {
                    return Err(ForkdGuestError::Remote(
                        "guest closed before response".into(),
                    ));
                }
            };
            check_remote_error(&value)?;
            collected_bytes = collected_bytes.saturating_add(value.to_string().len());
            if collected_bytes > MAX_RESPONSE_BYTES || responses.len() >= MAX_RESPONSE_LINES {
                return Err(ForkdGuestError::Remote(
                    "guest response exceeded limit".into(),
                ));
            }
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn stream(
        &self,
        args: Vec<String>,
        cwd: Option<&str>,
        pty: Option<bool>,
        env: Option<Value>,
        event_deadline: Option<Duration>,
    ) -> Result<ForkdGuestStream, ForkdGuestError> {
        let tcp = tokio::time::timeout(self.timeout, TcpStream::connect(&self.address))
            .await
            .map_err(|_| timed_out("guest connect timeout"))??;
        let (read, mut writer) = tcp.into_split();
        // Same per-connection handshake as the request path; the reader is
        // created before the handshake so bytes read ahead of the auth
        // exchange cannot be lost, and the buffered reader is moved into the
        // stream below.
        let mut reader = BufReader::new(read);
        authenticate(&mut reader, &mut writer, self.timeout).await?;
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
        // The per-event read budget must cover the guest's own exec deadline
        // (mirrors the exec read budget): without it a long silent command
        // dies client-side before the guest's deadline fires.
        Ok(ForkdGuestStream {
            reader,
            writer,
            timeout: event_deadline.unwrap_or(self.timeout),
            stopped: false,
            terminal: false,
        })
    }

    /// Ping the guest and return its response value.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn ping(&self) -> Result<Value, ForkdGuestError> {
        self.request(serde_json::json!({"action":"ping"}))
            .await?
            .pop()
            .ok_or_else(|| ForkdGuestError::Remote("empty ping response".into()))
    }

    /// Execute one of the structured, read-only guest filesystem RPCs.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
        // P1-6: the wire budget is on the *raw* payload bytes, not the JSON
        // encoding — a `data: Vec<u8>` byte array inflates 3-4x when encoded
        // as numbers, so a guest's legal 50 KiB (truncated) read used to be
        // rejected here while ZBRT accepted the same payload. The encoded
        // length is still checked against a larger absolute ceiling as
        // protection against genuinely malformed/huge responses.
        if raw_response_bytes(&value) > MAX_GUEST_RESULT_BYTES
            || encoded.len() > MAX_GUEST_RESULT_ENCODED_BYTES
        {
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn exec(
        &self,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<Value, ForkdGuestError> {
        self.exec_in("/", args, timeout_secs).await
    }

    /// Execute a command in a path interpreted by the guest runtime.
    /// `guest_cwd` is opaque and is never converted to a host path.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn exec_in(
        &self,
        guest_cwd: &str,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<Value, ForkdGuestError> {
        // Guest-side deadline + margin: the response may legitimately take the
        // full exec timeout to arrive. saturating：validation 只保证 >0，
        // 超大 f64 饱和转 u64 后加法曾 panic —— 预算饱和到 MAX（不设限，
        // guest 死线权威）而不是 panic。
        let read_timeout = self
            .timeout
            .saturating_add(Duration::from_secs(timeout_secs))
            .saturating_add(EXEC_READ_MARGIN);
        self.request_with_read_timeout(
            serde_json::json!({"action":"exec","cwd":guest_cwd,"args":args,"timeout":timeout_secs}),
            read_timeout,
        )
        .await?
        .pop()
        .ok_or_else(|| ForkdGuestError::Remote("empty exec response".into()))
    }

    /// Evaluate code in the guest root directory.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn eval(&self, code: impl Into<String>) -> Result<Value, ForkdGuestError> {
        self.eval_in("/", code).await
    }

    /// Evaluate code in a guest working directory. `guest_cwd` remains opaque.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
            // Same ceil-to-seconds rounding as the provider's
            // duration_to_timeout_secs (single source).
            let seconds = crate::forkd::duration_to_timeout_secs(timeout);
            action["timeout"] = Value::Number(seconds.into());
        }
        let read_timeout = request.timeout.map_or(self.timeout, |timeout| {
            self.timeout
                .saturating_add(timeout)
                .saturating_add(EXEC_READ_MARGIN)
        });
        self.request_with_read_timeout(action, read_timeout)
            .await?
            .pop()
            .ok_or_else(|| ForkdGuestError::Remote("empty eval response".into()))
    }
}

impl ForkdGuestStream {
    /// Read the next protocol event. A terminal exit event is returned normally.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn stop(&mut self) -> Result<(), ForkdGuestError> {
        if self.terminal || self.stopped {
            return Ok(());
        }
        // Latch `stopped` only after the write succeeds (mirrors
        // ZbrtStream::stop): a failed write must leave the stream retryable,
        // not report a stop that never went out.
        write_json(
            &mut self.writer,
            &serde_json::json!({"action": "stop"}),
            self.timeout,
        )
        .await?;
        self.stopped = true;
        Ok(())
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
        if buf.ends_with(b"\n") {
            if buf.len() > MAX_LINE_BYTES {
                return Err(ForkdGuestError::TooLarge);
            }
        } else if buf.len() > MAX_LINE_BYTES {
            // 读取在 take 上限截断且未见换行 = 行超限（截断形态）。
            return Err(ForkdGuestError::TooLarge);
        } else {
            // 限内的无换行 EOF = 对端写半行后关闭——不是行超限（排障时会
            // 误导成"响应过大"），是 Remote 类的对端中断。
            return Err(ForkdGuestError::Remote("guest closed mid-line".into()));
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

/// The configured agent token, or `None` when auth is disabled. Blank values
/// are ignored, exactly like the agent side (`agent_token_from_env`) and the
/// Python SDK; the value is re-read per connection so a rotated token applies
/// to the next connection without dropping the client.
fn agent_token() -> Option<String> {
    std::env::var(AGENT_TOKEN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Run the agent-token handshake on a freshly connected stream: with a token
/// configured, `{"action":"auth","token":...}` is the first frame on the wire
/// and the agent must answer `{"action":"auth","ok":true}`. Anything else (a
/// rejection frame, a plain `{"error":...}`, a silent close) is a
/// [`ForkdGuestError::Remote`] so callers see the same classification as any
/// guest-reported failure. With no token configured the wire behavior is
/// unchanged; blank keepalive lines before the reply are skipped by
/// [`read_json_line`].
async fn authenticate<R, W>(
    reader: &mut R,
    writer: &mut W,
    timeout: Duration,
) -> Result<(), ForkdGuestError>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Some(token) = agent_token() else {
        return Ok(());
    };
    write_json(
        writer,
        &serde_json::json!({"action": "auth", "token": token}),
        timeout,
    )
    .await?;
    let value = read_json_line(reader, timeout)
        .await?
        .ok_or_else(|| ForkdGuestError::Remote("guest closed before auth response".into()))?;
    if value.get("action").and_then(Value::as_str) == Some("auth")
        && value.get("ok").and_then(Value::as_bool) == Some(true)
    {
        return Ok(());
    }
    let detail = value
        .get("error")
        .and_then(Value::as_str)
        .map(|error| format!(": {error}"))
        .unwrap_or_default();
    Err(ForkdGuestError::Remote(format!(
        "guest agent auth failed{detail}"
    )))
}

/// Estimate the raw payload size of a guest tool response (P1-6): a `data`
/// byte array counts one byte per element, string fields count their UTF-8
/// bytes, and each array item carries a small JSON-syntax allowance. This is
/// the same question ZBRT's 50 KiB guest-side cap answers for the identical
/// contract, so both transports accept/reject the same payloads.
fn raw_response_bytes(value: &Value) -> usize {
    /// JSON syntax + non-string field allowance per array item
    /// (`{"line":N,"is_dir":true,...}` shells around the counted strings).
    const PER_ITEM_ALLOWANCE: usize = 32;
    let mut total = 0usize;
    if let Some(object) = value.as_object() {
        for (key, field) in object {
            match field {
                // read: `data` is the raw payload; `total_bytes`/`truncated`
                // are negligible scalars.
                Value::Array(items) if key == "data" => {
                    total = total.saturating_add(items.len());
                }
                // ls `entries` ({name,is_dir,size}), find `matches`
                // (strings), grep `matches` ({path,line,text}): count the
                // string contents plus a fixed per-item allowance.
                Value::Array(items) => {
                    for item in items {
                        total = total.saturating_add(PER_ITEM_ALLOWANCE);
                        match item {
                            Value::String(text) => total = total.saturating_add(text.len()),
                            Value::Object(fields) => {
                                for field in fields.values() {
                                    if let Value::String(text) = field {
                                        total = total.saturating_add(text.len());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // Stdout/stderr-style string fields (e.g. eval `output`).
                Value::String(text) => total = total.saturating_add(text.len()),
                _ => {}
            }
        }
    }
    total
}
