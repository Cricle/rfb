//! [`RfbClient`] and [`Sandbox`] — the public facade over the three internal
//! protocol adapters (`sdk/UNIFIED_API.md` §2–§6).

use std::time::Duration;

use serde_json::{json, Value};

use super::error::{transport_timeout, RfbError};
use super::ndjson;
use super::types::{
    default_guest_timeout, CreateOptions, ExecResult, StreamEvent, StreamEventKind,
};
use super::validation;
#[cfg(feature = "zeroboot")]
use super::zbrt;
use super::{DirEntry, FileRead, GrepMatch, GuestTransport, SandboxInfo, Snapshot};
use crate::guest::{
    FindRequest, FindResult, GrepRequest, GrepResult, LsRequest, LsResult, ReadRequest,
    WriteRequest, WriteResult, MAX_GUEST_RESULTS, MAX_GUEST_RESULT_BYTES,
};
use crate::BoxFuture;

/// The single public client type. Wraps the forkd controller HTTP client;
/// guest operations are reached through the returned [`Sandbox`] facades.
#[derive(Clone)]
pub struct RfbClient {
    pub(super) http: crate::controller::ForkdClient,
    timeout: Duration,
}

impl RfbClient {
    /// Create a client for the given controller base URL.
    ///
    /// `token` becomes a `Authorization: Bearer <token>` header on every
    /// request when set.
    pub fn new(
        base_url: impl Into<String>,
        token: Option<String>,
        timeout: Duration,
    ) -> Result<Self, RfbError> {
        Ok(Self {
            http: crate::controller::ForkdClient::new(base_url, token, timeout)?,
            timeout,
        })
    }

    /// Create a client from the environment: `FORKD_URL`
    /// (default `http://127.0.0.1:8889`), `FORKD_TOKEN` (non-empty enables the
    /// bearer header), 10 second default timeout.
    pub fn from_env() -> Result<Self, RfbError> {
        Ok(Self {
            http: crate::controller::ForkdClient::from_env()?,
            timeout: default_guest_timeout(),
        })
    }

    /// `GET /v1/snapshots`.
    pub async fn list_snapshots(&self) -> Result<Vec<Snapshot>, RfbError> {
        Ok(self.http.list_snapshots().await?)
    }

    /// Snapshot detail with the `/info` → legacy endpoint fallback chain;
    /// both endpoints returning 404 yields `Ok(None)`.
    pub async fn snapshot(&self, tag: &str) -> Result<Option<Snapshot>, RfbError> {
        Ok(self.http.snapshot_info(tag).await?)
    }

    /// Poll `list_snapshots` every 100 ms until `tag` is `ready` and
    /// `bootable`. A `failed` status raises [`RfbError::Remote`] immediately;
    /// the deadline raises [`RfbError::Transport`] (timed out).
    pub async fn wait_snapshot(&self, tag: &str, timeout_s: u64) -> Result<Snapshot, RfbError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
        loop {
            let snapshots = self.list_snapshots().await?;
            if let Some(snapshot) = snapshots.iter().find(|s| s.tag == tag) {
                if snapshot.status.eq_ignore_ascii_case("failed") {
                    return Err(RfbError::Remote(format!("snapshot `{tag}` failed")));
                }
                if snapshot.status.eq_ignore_ascii_case("ready") && snapshot.bootable {
                    return Ok(snapshot.clone());
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(transport_timeout(
                    "snapshot did not become ready before timeout",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100).min(deadline - now)).await;
        }
    }

    /// `POST /v1/sandboxes` with [`CreateOptions`]; returns one [`Sandbox`]
    /// facade per created sandbox.
    pub async fn create_sandbox(
        &self,
        snapshot_tag: &str,
        options: CreateOptions,
    ) -> Result<Vec<Sandbox>, RfbError> {
        let request = crate::controller::CreateSandboxRequest {
            snapshot_tag,
            n: options.n,
            per_child_netns: options.per_child_netns,
            memory_limit_mib: options.memory_limit_mib,
            prewarm: options.prewarm,
            live_fork: options.live_fork,
            hugepages: options.hugepages,
        };
        let infos = self.http.create_sandbox(&request).await?;
        Ok(infos
            .into_iter()
            .map(|info| Sandbox::new(self.http.clone(), info, options.transport, self.timeout))
            .collect())
    }

    /// Convenience: create exactly one sandbox with default options.
    pub async fn create_sandbox1(&self, snapshot_tag: &str) -> Result<Sandbox, RfbError> {
        self.create_sandbox(snapshot_tag, CreateOptions::default())
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| RfbError::Decode("controller returned no sandbox".to_owned()))
    }

    /// `GET /v1/sandboxes` — the live sandbox pool.
    pub async fn list_sandboxes(&self) -> Result<Vec<Sandbox>, RfbError> {
        Ok(self
            .http
            .list_sandboxes()
            .await?
            .into_iter()
            .map(|info| {
                Sandbox::new(
                    self.http.clone(),
                    info,
                    GuestTransport::default(),
                    self.timeout,
                )
            })
            .collect())
    }

    /// Attach a facade to an existing sandbox: pass an owned/borrowed
    /// [`Sandbox`] (attached as-is) or an id `&str` (resolved through
    /// `list_sandboxes`). Uses the default transport; see
    /// [`connect_id_with`](Self::connect_id_with) for an explicit choice.
    pub async fn connect(&self, sandbox_or_id: impl ConnectTarget) -> Result<Sandbox, RfbError> {
        sandbox_or_id.connect_to(self).await
    }

    /// Attach by sandbox id: validates the id, resolves the sandbox through
    /// `list_sandboxes`, and returns a facade with the default transport.
    pub async fn connect_id(&self, id: &str) -> Result<Sandbox, RfbError> {
        self.connect_id_with(id, GuestTransport::default()).await
    }

    /// Attach by sandbox id with an explicit [`GuestTransport`].
    pub async fn connect_id_with(
        &self,
        id: &str,
        transport: GuestTransport,
    ) -> Result<Sandbox, RfbError> {
        validation::sandbox_id(id)?;
        let sandboxes = self.list_sandboxes().await?;
        sandboxes
            .into_iter()
            .find(|s| s.info.id == id)
            .map(|s| s.with_transport(transport))
            .ok_or_else(|| RfbError::Remote(format!("sandbox `{id}` not found")))
    }

    /// `POST /v1/sandboxes/{id}/ping`; returns the controller JSON value.
    pub async fn ping_sandbox(&self, id: &str) -> Result<Value, RfbError> {
        validation::sandbox_id(id)?;
        Ok(self.http.ping(id).await?)
    }

    /// `DELETE /v1/sandboxes/{id}`; 2xx and 404 are both success.
    pub async fn delete_sandbox(&self, id: &str) -> Result<(), RfbError> {
        validation::sandbox_id(id)?;
        Ok(self.http.delete_sandbox(id).await?)
    }
}

/// Types attachable to a client: a [`Sandbox`] facade or an id string.
pub trait ConnectTarget {
    fn connect_to(self, client: &RfbClient) -> BoxFuture<'static, Result<Sandbox, RfbError>>;
}

impl ConnectTarget for &Sandbox {
    fn connect_to(self, _client: &RfbClient) -> BoxFuture<'static, Result<Sandbox, RfbError>> {
        let sandbox = self.clone();
        Box::pin(async move { Ok(sandbox) })
    }
}

impl ConnectTarget for &str {
    fn connect_to(self, client: &RfbClient) -> BoxFuture<'static, Result<Sandbox, RfbError>> {
        let client = client.clone();
        let id = self.to_owned();
        Box::pin(async move { client.connect_id(&id).await })
    }
}

/// Facade over one live sandbox. All guest operations are available in
/// identical shapes on both [`GuestTransport`]s.
#[derive(Clone)]
pub struct Sandbox {
    http: crate::controller::ForkdClient,
    info: SandboxInfo,
    transport: GuestTransport,
    timeout: Duration,
}

impl Sandbox {
    pub(super) fn new(
        http: crate::controller::ForkdClient,
        info: SandboxInfo,
        transport: GuestTransport,
        timeout: Duration,
    ) -> Self {
        Self {
            http,
            info,
            transport,
            timeout,
        }
    }

    /// Sandbox id.
    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// Snapshot tag the sandbox was created from.
    pub fn snapshot_tag(&self) -> &str {
        &self.info.snapshot_tag
    }

    /// Guest TCP address as reported by the controller.
    pub fn guest_addr(&self) -> &str {
        &self.info.guest_addr
    }

    /// Unix timestamp of creation, when known.
    pub fn created_at_unix(&self) -> Option<i64> {
        self.info.created_at_unix
    }

    /// Full controller record.
    pub fn info(&self) -> &SandboxInfo {
        &self.info
    }

    /// The guest transport this facade uses.
    pub fn transport(&self) -> GuestTransport {
        self.transport
    }

    /// Return a copy of this facade with a different guest transport.
    pub fn with_transport(mut self, transport: GuestTransport) -> Self {
        self.transport = transport;
        self
    }

    fn ops(&self) -> GuestOps {
        match self.transport {
            GuestTransport::Ndjson => GuestOps::Ndjson(crate::forkd_guest::ForkdGuestClient {
                address: self.info.guest_addr.clone(),
                timeout: self.timeout,
            }),
            #[cfg(feature = "zeroboot")]
            GuestTransport::Zbrt => GuestOps::Zbrt(zbrt::ZbrtGuest {
                address: self.info.guest_addr.clone(),
                timeout: self.timeout,
            }),
        }
    }

    /// Probe the guest. NDJSON: `ping` action (`pong` flag). ZBRT: `Health` →
    /// `HealthAck` healthy flag.
    pub async fn ping(&self) -> Result<bool, RfbError> {
        self.ops().ping().await
    }

    /// Execute a command. `cwd` defaults to the guest root (`"/"`); `stdin`
    /// is delivered on the ZBRT transport (NDJSON `exec` carries no stdin
    /// field on the wire and ignores it); `timeout_s` is ceil-ed to whole
    /// seconds for NDJSON and milli-seconds for ZBRT.
    pub async fn exec(
        &self,
        args: &[impl AsRef<str>],
        cwd: &str,
        timeout_s: f64,
        stdin: &[u8],
    ) -> Result<ExecResult, RfbError> {
        if args.is_empty() {
            return Err(RfbError::Validation("args must not be empty".to_owned()));
        }
        validation::file_path(cwd)?;
        let secs = validation::timeout_secs(timeout_s)?;
        let argv: Vec<String> = args.iter().map(|a| a.as_ref().to_owned()).collect();
        self.ops().exec(argv, cwd, secs, stdin.to_vec()).await
    }

    /// Evaluate code. The eval `output` maps to `ExecResult::stdout` on both
    /// transports. `cwd=None` uses the guest default; `timeout_s=None` means
    /// no explicit deadline (NDJSON omits the key, ZBRT sends 0).
    pub async fn eval(
        &self,
        code: &str,
        cwd: Option<&str>,
        timeout_s: Option<f64>,
    ) -> Result<ExecResult, RfbError> {
        validation::eval_code(code)?;
        if let Some(cwd) = cwd {
            validation::file_path(cwd)?;
        }
        let secs = match timeout_s {
            Some(t) => Some(validation::timeout_secs(t)?),
            None => None,
        };
        self.ops().eval(code, cwd, secs).await
    }

    /// `ls` a guest directory; defaults to the workspace root.
    pub async fn ls(&self, path: &str) -> Result<Vec<DirEntry>, RfbError> {
        validation::fs_path(path)?;
        self.ops().ls(path).await
    }

    /// Find guest paths whose file name matches `pattern`.
    pub async fn find(&self, path: &str, pattern: &str) -> Result<Vec<String>, RfbError> {
        validation::fs_path(path)?;
        validation::pattern(pattern)?;
        self.ops().find(path, pattern).await
    }

    /// Grep guest file contents; returns typed matches.
    pub async fn grep(&self, path: &str, pattern: &str) -> Result<Vec<GrepMatch>, RfbError> {
        validation::fs_path(path)?;
        validation::pattern(pattern)?;
        self.ops().grep(path, pattern).await
    }

    /// Read a guest file, optionally from `offset` with a `max_bytes` cap
    /// (1..=51200).
    pub async fn read(
        &self,
        path: &str,
        offset: Option<u64>,
        max_bytes: Option<usize>,
    ) -> Result<FileRead, RfbError> {
        validation::file_path(path)?;
        if let Some(max_bytes) = max_bytes {
            validation::limit(
                max_bytes,
                MAX_GUEST_RESULT_BYTES,
                "max_bytes must be within 1..=51200",
            )?;
        }
        self.ops().read(path, offset, max_bytes).await
    }

    /// Write (or append to) a guest file; returns bytes written. The payload
    /// is capped at 51200 bytes.
    pub async fn write(
        &self,
        path: &str,
        data: &[u8],
        append: bool,
        mode: Option<u32>,
    ) -> Result<u64, RfbError> {
        validation::file_path(path)?;
        if data.len() > MAX_GUEST_RESULT_BYTES {
            return Err(RfbError::Validation(
                "write payload exceeds 51200 bytes".to_owned(),
            ));
        }
        self.ops().write(path, data.to_vec(), append, mode).await
    }

    /// Start an interactive stream. NDJSON supports `pty`/`env`; over ZBRT a
    /// requested `pty` or non-empty `env` is rejected locally (fail closed).
    pub async fn stream(
        &self,
        args: &[impl AsRef<str>],
        cwd: Option<&str>,
        pty: Option<bool>,
        env: Option<Value>,
    ) -> Result<GuestStream, RfbError> {
        if args.is_empty() {
            return Err(RfbError::Validation("args must not be empty".to_owned()));
        }
        if let Some(cwd) = cwd {
            validation::file_path(cwd)?;
        }
        let argv: Vec<String> = args.iter().map(|a| a.as_ref().to_owned()).collect();
        self.ops().stream(argv, cwd, pty, env).await
    }

    /// Delete the sandbox (`DELETE /v1/sandboxes/{id}`); 2xx and 404 are both
    /// success.
    pub async fn delete(&self) -> Result<(), RfbError> {
        Ok(self.http.delete_sandbox(&self.info.id).await?)
    }
}

/// Internal dispatch over the two guest transports. Never public.
pub(super) enum GuestOps {
    Ndjson(crate::forkd_guest::ForkdGuestClient),
    #[cfg(feature = "zeroboot")]
    Zbrt(zbrt::ZbrtGuest),
}

impl GuestOps {
    async fn ping(&self) -> Result<bool, RfbError> {
        match self {
            GuestOps::Ndjson(client) => Ok(ndjson::ping_healthy(&client.ping().await?)),
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => guest.health().await,
        }
    }

    async fn exec(
        &self,
        argv: Vec<String>,
        cwd: &str,
        secs: u64,
        stdin: Vec<u8>,
    ) -> Result<ExecResult, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let value = client.exec_in(cwd, argv, secs).await?;
                ndjson::exec_result(&value)
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                let timeout_ms = u32::try_from(secs.saturating_mul(1000)).unwrap_or(u32::MAX);
                guest
                    .exec(argv, Some(cwd.to_owned()), stdin, timeout_ms)
                    .await
            }
        }
    }

    async fn eval(
        &self,
        code: &str,
        cwd: Option<&str>,
        secs: Option<u64>,
    ) -> Result<ExecResult, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let request = crate::guest::EvalRequest {
                    cwd: cwd.map(str::to_owned),
                    code: code.to_owned(),
                    timeout: secs.map(Duration::from_secs),
                };
                let value = client.eval_request(request).await?;
                ndjson::eval_result(&value)
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                // ZBRT v1 has no eval op; the facade convention is an Execute
                // turn whose argv names the structured `eval` op followed by
                // the source (see README known limitations). Whole seconds are
                // sent as milliseconds; `None` sends 0 (no explicit deadline).
                let argv = vec!["eval".to_owned(), code.to_owned()];
                let timeout_ms = secs
                    .and_then(|s| u32::try_from(s.saturating_mul(1000)).ok())
                    .unwrap_or(0);
                guest
                    .exec(argv, cwd.map(str::to_owned), Vec::new(), timeout_ms)
                    .await
            }
        }
    }

    async fn ls(&self, path: &str) -> Result<Vec<DirEntry>, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let args = serde_json::to_value(LsRequest {
                    path: path.to_owned(),
                    max_results: MAX_GUEST_RESULTS,
                })
                .map_err(|e| RfbError::Decode(e.to_string()))?;
                let value = client.execute_tool("ls", args).await?;
                Ok(serde_json::from_value::<LsResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .entries)
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                let value = guest
                    .fs(1, path, json!({"max_results": MAX_GUEST_RESULTS}))
                    .await?;
                Ok(serde_json::from_value::<LsResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .entries)
            }
        }
    }

    async fn find(&self, path: &str, pattern: &str) -> Result<Vec<String>, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let args = serde_json::to_value(FindRequest {
                    path: path.to_owned(),
                    pattern: pattern.to_owned(),
                    max_results: MAX_GUEST_RESULTS,
                })
                .map_err(|e| RfbError::Decode(e.to_string()))?;
                let value = client.execute_tool("find", args).await?;
                Ok(serde_json::from_value::<FindResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .matches)
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                let value = guest
                    .fs(
                        2,
                        path,
                        json!({"pattern": pattern, "max_results": MAX_GUEST_RESULTS}),
                    )
                    .await?;
                Ok(serde_json::from_value::<FindResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .matches)
            }
        }
    }

    async fn grep(&self, path: &str, pattern: &str) -> Result<Vec<GrepMatch>, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let args = serde_json::to_value(GrepRequest {
                    path: path.to_owned(),
                    pattern: pattern.to_owned(),
                    max_results: MAX_GUEST_RESULTS,
                    max_bytes: MAX_GUEST_RESULT_BYTES,
                })
                .map_err(|e| RfbError::Decode(e.to_string()))?;
                let value = client.execute_tool("grep", args).await?;
                Ok(serde_json::from_value::<GrepResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .matches)
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                let value = guest
                    .fs(
                        3,
                        path,
                        json!({
                            "pattern": pattern,
                            "max_results": MAX_GUEST_RESULTS,
                            "max_bytes": MAX_GUEST_RESULT_BYTES,
                        }),
                    )
                    .await?;
                Ok(serde_json::from_value::<GrepResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .matches)
            }
        }
    }

    async fn read(
        &self,
        path: &str,
        offset: Option<u64>,
        max_bytes: Option<usize>,
    ) -> Result<FileRead, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let args = serde_json::to_value(ReadRequest {
                    path: path.to_owned(),
                    offset,
                    max_bytes,
                })
                .map_err(|e| RfbError::Decode(e.to_string()))?;
                let value = client.execute_tool("read", args).await?;
                serde_json::from_value::<FileRead>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                let value = guest
                    .fs(4, path, json!({"offset": offset, "max_bytes": max_bytes}))
                    .await?;
                serde_json::from_value::<FileRead>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))
            }
        }
    }

    async fn write(
        &self,
        path: &str,
        data: Vec<u8>,
        append: bool,
        mode: Option<u32>,
    ) -> Result<u64, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let args = serde_json::to_value(WriteRequest {
                    path: path.to_owned(),
                    data,
                    append,
                    mode,
                })
                .map_err(|e| RfbError::Decode(e.to_string()))?;
                let value = client.execute_tool("write", args).await?;
                Ok(serde_json::from_value::<WriteResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .bytes_written)
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                let value = guest
                    .fs(
                        5,
                        path,
                        json!({"data": data, "append": append, "mode": mode}),
                    )
                    .await?;
                Ok(serde_json::from_value::<WriteResult>(value)
                    .map_err(|e| RfbError::Decode(e.to_string()))?
                    .bytes_written)
            }
        }
    }

    async fn stream(
        &self,
        argv: Vec<String>,
        cwd: Option<&str>,
        pty: Option<bool>,
        env: Option<Value>,
    ) -> Result<GuestStream, RfbError> {
        match self {
            GuestOps::Ndjson(client) => {
                let inner = client.stream(argv, cwd, pty, env).await?;
                Ok(GuestStream {
                    inner: StreamInner::Ndjson(inner),
                    exited: false,
                })
            }
            #[cfg(feature = "zeroboot")]
            GuestOps::Zbrt(guest) => {
                if pty == Some(true) {
                    return Err(RfbError::Validation(
                        "pty is not supported over the ZBRT transport".to_owned(),
                    ));
                }
                if let Some(env) = env {
                    if env.as_object().is_some_and(|map| !map.is_empty()) {
                        return Err(RfbError::Validation(
                            "env is not supported over the ZBRT transport".to_owned(),
                        ));
                    }
                }
                let inner = zbrt::ZbrtGuest::start(guest, argv, cwd.map(str::to_owned)).await?;
                Ok(GuestStream {
                    inner: StreamInner::Zbrt(inner),
                    exited: false,
                })
            }
        }
    }
}

/// Interactive stream facade. Clean close (after the terminal `Exit` event or
/// peer disconnect) reads as `Ok(None)`.
pub struct GuestStream {
    inner: StreamInner,
    exited: bool,
}

enum StreamInner {
    Ndjson(crate::forkd_guest::ForkdGuestStream),
    #[cfg(feature = "zeroboot")]
    Zbrt(zbrt::ZbrtStream),
}

impl GuestStream {
    /// Next event; `Ok(None)` on clean close.
    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, RfbError> {
        if self.exited {
            return Ok(None);
        }
        let event = match &mut self.inner {
            StreamInner::Ndjson(inner) => match inner.next_event().await? {
                Some(value) => ndjson::stream_event(value)?,
                None => None,
            },
            #[cfg(feature = "zeroboot")]
            StreamInner::Zbrt(inner) => inner.next_event().await?,
        };
        if matches!(
            event,
            Some(StreamEvent {
                kind: StreamEventKind::Exit,
                ..
            })
        ) {
            self.exited = true;
        }
        Ok(event)
    }

    /// Send text to the guest's stdin. Raises [`RfbError::Remote`] after the
    /// stream has terminated; unsupported outright over ZBRT (see README).
    pub async fn send_input(&mut self, text: impl Into<String>) -> Result<(), RfbError> {
        if self.exited {
            return Err(RfbError::Remote(
                "guest stream is no longer running".to_owned(),
            ));
        }
        match &mut self.inner {
            StreamInner::Ndjson(inner) => Ok(inner.send_input(text).await?),
            #[cfg(feature = "zeroboot")]
            StreamInner::Zbrt(inner) => inner.send_input().await,
        }
    }

    /// Ask the guest to terminate the stream. Idempotent.
    pub async fn stop(&mut self) -> Result<(), RfbError> {
        match &mut self.inner {
            StreamInner::Ndjson(inner) => Ok(inner.stop().await?),
            #[cfg(feature = "zeroboot")]
            StreamInner::Zbrt(inner) => inner.stop().await,
        }
    }
}
