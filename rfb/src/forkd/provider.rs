// Forkd HTTP controller and TCP guest provider for RFB.
use super::{
    guest as core_guest, BackendKind, BoxFuture, Capability, ExecResult, ExecSpec, ProviderError,
    Sandbox, SandboxError, SandboxProvider, SandboxSpec, TransportKind,
};
use crate::{controller, forkd_guest as guest};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
/// Guest capability profile for forkd sandboxes.
pub enum ForkdGuestProfile {
    /// Execute and health capabilities only.
    Minimal,
    /// Standard execution, health, stream, and filesystem capabilities.
    #[default]
    Default,
    /// Default capabilities plus code evaluation.
    CustomShell,
}
impl ForkdGuestProfile {
    /// Return the capabilities enabled by this profile.
    pub fn capabilities(self) -> Vec<Capability> {
        match self {
            Self::Minimal => vec![Capability::Execute, Capability::Health],
            Self::Default => vec![
                Capability::Execute,
                Capability::Health,
                Capability::Stream,
                Capability::Ls,
                Capability::Find,
                Capability::Grep,
                Capability::ReadFile,
                Capability::WriteFile,
            ],
            Self::CustomShell => vec![
                Capability::Execute,
                Capability::Health,
                Capability::Stream,
                Capability::Ls,
                Capability::Find,
                Capability::Grep,
                Capability::ReadFile,
                Capability::WriteFile,
                Capability::Eval,
            ],
        }
    }
}

#[derive(Debug, Clone)]
/// Configuration for the forkd provider.
pub struct ForkdConfig {
    /// Forkd controller base URL.
    pub base_url: String,
    /// Optional bearer token for the controller.
    pub token: Option<String>,
    /// Request timeout for controller calls.
    pub timeout: Duration,
    /// Default snapshot tag used when creating sandboxes.
    pub snapshot_tag: Option<String>,
    /// Timeout for guest TCP requests.
    pub guest_timeout: Duration,
    /// Guest capability profile.
    pub guest_profile: ForkdGuestProfile,
    /// Resolved guest capabilities advertised for created sandboxes.
    pub guest_capabilities: Vec<Capability>,
}
impl Default for ForkdConfig {
    fn default() -> Self {
        let profile = ForkdGuestProfile::Default;
        Self {
            base_url: "http://127.0.0.1:8889".into(),
            token: None,
            timeout: Duration::from_secs(10),
            snapshot_tag: None,
            guest_timeout: Duration::from_secs(10),
            guest_profile: profile,
            guest_capabilities: profile.capabilities(),
        }
    }
}
impl ForkdConfig {
    /// Load configuration from FORKD_* environment variables.
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("FORKD_URL") {
            c.base_url = v;
        }
        if let Ok(v) = std::env::var("FORKD_TOKEN") {
            if !v.trim().is_empty() {
                c.token = Some(v);
            }
        }
        if let Ok(v) = std::env::var("FORKD_SNAPSHOT_TAG") {
            if !v.trim().is_empty() {
                c.snapshot_tag = Some(v);
            }
        }
        if let Ok(v) = std::env::var("FORKD_GUEST_PROFILE") {
            c = c.with_guest_profile(match v.to_ascii_lowercase().as_str() {
                "minimal" => ForkdGuestProfile::Minimal,
                "custom-shell" | "custom_shell" => ForkdGuestProfile::CustomShell,
                _ => ForkdGuestProfile::Default,
            });
        }
        c
    }
    /// Set the guest profile and resolve its capabilities.
    #[must_use]
    pub fn with_guest_profile(mut self, profile: ForkdGuestProfile) -> Self {
        self.guest_profile = profile;
        self.guest_capabilities = profile.capabilities();
        self
    }
}

#[derive(Clone)]
/// High-level forkd sandbox client.
pub struct ForkdClient {
    config: Arc<ForkdConfig>,
    controller: controller::ForkdClient,
}
impl ForkdClient {
    /// Construct a client from the standard FORKD_* environment variables.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn from_env() -> Result<Self, ForkdClientError> {
        Self::new(ForkdConfig::from_env())
    }

        /// Construct a forkd client from the given configuration.
        ///
        /// # Errors
        ///
        /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn new(config: ForkdConfig) -> Result<Self, ForkdClientError> {
        let controller = controller::ForkdClient::new(
            config.base_url.clone(),
            config.token.clone(),
            config.timeout,
        )?;
        Ok(Self {
            config: Arc::new(config),
            controller,
        })
    }
    /// Validate a sandbox identifier's character syntax.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate_sandbox_id(id: &str) -> Result<(), ForkdClientError> {
        controller::ForkdClient::validate_sandbox_id(id)
    }
    /// Validate that a string parses as a guest TCP address.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate_guest_address(a: &str) -> Result<(), ForkdClientError> {
        controller::ForkdClient::validate_guest_address(a)
    }
    /// Create a single forkd sandbox and return a handle to it.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn create(
        &self,
        req: &CreateSandboxRequest<'_>,
    ) -> Result<ForkdSandbox, ForkdClientError> {
        let mut xs = self.controller.create_sandbox(req).await?;
        let x = xs
            .pop()
            .ok_or_else(|| ForkdClientError::Decode("forkd returned no sandboxes".into()))?;
        Self::validate_sandbox_id(&x.id)?;
        Self::validate_guest_address(&x.guest_addr)?;
        Ok(ForkdSandbox {
            info: x,
            client: self.clone(),
        })
    }
    /// Create sandboxes and return their raw metadata from the controller.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn create_sandbox(
        &self,
        req: &CreateSandboxRequest<'_>,
    ) -> Result<Vec<SandboxInfo>, ForkdClientError> {
        self.controller.create_sandbox(req).await
    }
    /// Wait until the named snapshot reaches a ready/bootable state.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn wait_for_snapshot_ready(
        &self,
        tag: &str,
        timeout: Duration,
    ) -> Result<(), ForkdClientError> {
        self.controller.wait_for_snapshot_ready(tag, timeout).await
    }
    /// List live sandboxes from the controller.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn list_sandboxes(&self) -> Result<Vec<SandboxInfo>, ForkdClientError> {
        self.controller.list_sandboxes().await
    }
    /// Ping a sandbox by id.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn ping(&self, sandbox_id: &str) -> Result<serde_json::Value, ForkdClientError> {
        self.controller.ping(sandbox_id).await
    }
    /// Delete a sandbox by id (idempotent on 404).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn delete_sandbox(&self, sandbox_id: &str) -> Result<(), ForkdClientError> {
        self.controller.delete_sandbox(sandbox_id).await
    }
    /// Fetch detailed snapshot metadata, falling back for older controllers.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn snapshot_info(&self, tag: &str) -> Result<Option<SnapshotInfo>, ForkdClientError> {
        self.controller.snapshot_info(tag).await
    }

    /// Whether the named snapshot exists and is ready/bootable.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub async fn snapshot_ready(&self, tag: &str) -> Result<bool, ForkdClientError> {
        self.controller.snapshot_ready(tag).await
    }
}

#[derive(Debug, Clone, Serialize)]
/// Owned request describing forkd sandboxes to create.
pub struct CreateSandboxRequestOwned {
    /// Snapshot tag the sandbox is created from.
    pub snapshot_tag: String,
    /// Number of sandboxes to create.
    pub n: usize,
    /// Give each created sandbox its own network namespace.
    pub per_child_netns: bool,
    /// Optional memory limit in MiB.
    pub memory_limit_mib: Option<u64>,
    /// Prewarm the sandbox before returning.
    pub prewarm: bool,
    /// Branch the sandbox from a live guest.
    pub live_fork: bool,
    /// Use huge pages for sandbox memory.
    pub hugepages: bool,
}
impl<'a> CreateSandboxRequest<'a> {
    /// Build a request for a single sandbox with default options.
    pub fn single(tag: &'a str) -> Self {
        Self {
            snapshot_tag: tag,
            n: 1,
            per_child_netns: false,
            memory_limit_mib: None,
            prewarm: false,
            live_fork: false,
            hugepages: false,
        }
    }
}

#[derive(Clone)]
/// Sandbox managed by forkd.
pub struct ForkdSandbox {
    info: SandboxInfo,
    client: ForkdClient,
}
/// Guest client associated with a forkd sandbox.
pub type ForkdGuest = guest::ForkdGuestClient;
impl ForkdSandbox {
    /// Return the sandbox identifier.
    pub fn id(&self) -> &str {
        &self.info.id
    }
    /// Build a guest client connected to this sandbox's TCP address.
    pub fn guest(&self) -> ForkdGuest {
        let mut g = ForkdGuest::new(self.info.guest_addr.clone());
        g.timeout = self.client.config.guest_timeout;
        g
    }
}
impl Sandbox for ForkdSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::VirtualMachine
    }
    fn transport(&self) -> TransportKind {
        TransportKind::Tcp
    }
    fn capabilities(&self) -> &[Capability] {
        &self.client.config.guest_capabilities
    }
    fn exec<'a>(&'a self, spec: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            spec.validate().map_err(SandboxError::InvalidSpec)?;
            if spec.stdin.is_some() {
                return Err(SandboxError::Execution(
                    "forkd guest does not support stdin".into(),
                ));
            }
            let mut args = vec![spec.command];
            args.extend(spec.args);
            // The forkd guest confines every path (including exec cwd) to the
            // workspace root; the agent rejects "/" as "invalid guest path".
            let v = self
                .guest()
                .exec_in(
                    spec.cwd.as_deref().unwrap_or("/workspace"),
                    args,
                    spec.timeout.map(duration_to_timeout_secs).unwrap_or(10),
                )
                .await
                .map_err(forkd_error)?;
            let bytes = |x: Option<&serde_json::Value>| match x {
                Some(serde_json::Value::String(s)) => Ok(s.as_bytes().to_vec()),
                Some(serde_json::Value::Array(a)) => a
                    .iter()
                    .map(|v| {
                        v.as_u64()
                            .and_then(|n| u8::try_from(n).ok())
                            .ok_or_else(|| SandboxError::Execution("invalid output".into()))
                    })
                    .collect(),
                None | Some(serde_json::Value::Null) => Ok(Vec::new()),
                _ => Err(SandboxError::Execution("invalid output".into())),
            };
            Ok(ExecResult {
                status: v
                    .get("exit_code")
                    .and_then(|x| x.as_i64())
                    .map(|x| x as i32),
                stdout: bytes(v.get("out").or_else(|| v.get("stdout")))?,
                stderr: bytes(v.get("err").or_else(|| v.get("stderr")))?,
                timed_out: v
                    .get("timed_out")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
            })
        })
    }
    fn health<'a>(&'a self) -> BoxFuture<'a, Result<core_guest::Health, SandboxError>> {
        Box::pin(async move {
            let v = self
                .guest()
                .ping()
                .await
                .map_err(|e| SandboxError::Transport(e.to_string()))?;
            Ok(core_guest::Health {
                healthy: v.get("pong").and_then(|x| x.as_bool()).unwrap_or(false),
                latency: None,
                message: None,
            })
        })
    }
    fn read<'a>(
        &'a self,
        r: core_guest::ReadRequest,
    ) -> BoxFuture<'a, Result<core_guest::ReadResult, SandboxError>> {
        self.tool("read", r)
    }
    fn write<'a>(
        &'a self,
        r: core_guest::WriteRequest,
    ) -> BoxFuture<'a, Result<core_guest::WriteResult, SandboxError>> {
        self.tool("write", r)
    }
    fn ls<'a>(
        &'a self,
        r: core_guest::LsRequest,
    ) -> BoxFuture<'a, Result<core_guest::LsResult, SandboxError>> {
        self.tool("ls", r)
    }
    fn find<'a>(
        &'a self,
        r: core_guest::FindRequest,
    ) -> BoxFuture<'a, Result<core_guest::FindResult, SandboxError>> {
        self.tool("find", r)
    }
    fn grep<'a>(
        &'a self,
        r: core_guest::GrepRequest,
    ) -> BoxFuture<'a, Result<core_guest::GrepResult, SandboxError>> {
        self.tool("grep", r)
    }
    fn stream<'a>(
        &'a self,
        spec: core_guest::StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn core_guest::GuestStream + 'a>, SandboxError>> {
        Box::pin(async move {
            spec.validate().map_err(SandboxError::InvalidSpec)?;
            let args = std::iter::once(spec.command).chain(spec.args).collect();
            let env = if spec.env.is_empty() {
                None
            } else {
                Some(
                    serde_json::to_value(spec.env)
                        .map_err(|e| SandboxError::Execution(e.to_string()))?,
                )
            };
            let stream = self
                .guest()
                .stream(args, spec.cwd.as_deref(), spec.pty, env)
                .await
                .map_err(|e| SandboxError::Transport(e.to_string()))?;
            Ok(Box::new(ForkdGuestStreamAdapter(stream)) as Box<dyn core_guest::GuestStream + 'a>)
        })
    }
    fn eval<'a>(
        &'a self,
        r: core_guest::EvalRequest,
    ) -> BoxFuture<'a, Result<core_guest::EvalResult, SandboxError>> {
        Box::pin(async move {
            r.validate().map_err(SandboxError::InvalidSpec)?;
            let v = self
                .guest()
                .eval_request(r)
                .await
                .map_err(|e| SandboxError::Transport(e.to_string()))?;
            let output = match v.get("out").or_else(|| v.get("output")) {
                Some(serde_json::Value::String(s)) => s.as_bytes().to_vec(),
                Some(serde_json::Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect(),
                _ => Vec::new(),
            };
            Ok(core_guest::EvalResult {
                output,
                // The forkd agent answers eval with `status` (PROTOCOL.md
                // §2.4); `exit_code` is the legacy/exec alias some agents
                // still send — accept both so status is never silently None.
                status: v
                    .get("status")
                    .or_else(|| v.get("exit_code"))
                    .and_then(|x| x.as_i64())
                    .map(|x| x as i32),
                timed_out: v
                    .get("timed_out")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
            })
        })
    }
}
impl ForkdSandbox {
    fn tool<'a, T, R>(
        &'a self,
        name: &'static str,
        req: T,
    ) -> BoxFuture<'a, Result<R, SandboxError>>
    where
        T: Serialize + Send + 'a,
        R: for<'de> Deserialize<'de> + 'a,
    {
        Box::pin(async move {
            let v = self
                .guest()
                .execute_tool(
                    name,
                    serde_json::to_value(req)
                        .map_err(|e| SandboxError::Execution(e.to_string()))?,
                )
                .await
                .map_err(|e| SandboxError::Transport(e.to_string()))?;
            serde_json::from_value(v).map_err(|e| SandboxError::Execution(e.to_string()))
        })
    }
}

/// Adapts a forkd NDJSON stream ([`ForkdGuestStream`]) to the typed RFB
/// [`core_guest::GuestStream`] surface used by `Sandbox::stream`.
struct ForkdGuestStreamAdapter(pub(self) ForkdGuestStream);

impl core_guest::GuestStream for ForkdGuestStreamAdapter {
    fn next_event<'a>(
        &'a mut self,
    ) -> BoxFuture<'a, Result<Option<core_guest::StreamEvent>, SandboxError>> {
        Box::pin(async move {
            let value = self.0.next_event().await.map_err(forkd_error)?;
            value.map(forkd_stream_event).transpose()
        })
    }

    fn send_input<'a>(&'a mut self, input: String) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move { self.0.send_input(input).await.map_err(forkd_error) })
    }

    fn stop<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move { self.0.stop().await.map_err(forkd_error) })
    }
}

fn duration_to_timeout_secs(duration: Duration) -> u64 {
    duration.as_secs().saturating_add(u64::from(duration.subsec_nanos() != 0)).max(1)
}

fn forkd_error(error: ForkdGuestError) -> SandboxError {
    match error {
        ForkdGuestError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            SandboxError::Timeout
        }
        ForkdGuestError::Io(error) => SandboxError::Transport(error.to_string()),
        ForkdGuestError::TooLarge => {
            SandboxError::Transport("guest response exceeded limit".into())
        }
        ForkdGuestError::Json(error) => SandboxError::Execution(error.to_string()),
        ForkdGuestError::Remote(message) => SandboxError::Execution(message),
        ForkdGuestError::InvalidPath => SandboxError::Execution("invalid guest path".into()),
        ForkdGuestError::LimitExceeded => {
            SandboxError::Execution("guest result limit exceeded".into())
        }
        ForkdGuestError::UnsupportedGuestRpc(tool) => {
            SandboxError::Execution(format!("unsupported forkd guest RPC: {tool}"))
        }
    }
}

fn forkd_stream_event(value: serde_json::Value) -> Result<core_guest::StreamEvent, SandboxError> {
    if value.get("started").and_then(serde_json::Value::as_bool) == Some(true)
        || value.get("stream").and_then(serde_json::Value::as_str) == Some("started")
        || value.get("event").and_then(serde_json::Value::as_str) == Some("started")
    {
        return Ok(core_guest::StreamEvent::Started);
    }
    if let Some(code) = value.get("exit_code").and_then(serde_json::Value::as_i64) {
        return Ok(core_guest::StreamEvent::Exit {
            code: Some(code as i32),
        });
    }
    if value.get("done").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(core_guest::StreamEvent::Exit { code: None });
    }
    for (key, stderr) in [
        ("stdout", false),
        ("out", false),
        ("stderr", true),
        ("err", true),
    ] {
        if value.get(key).is_some() {
            let data = forkd_value_bytes(value.get(key))?;
            return Ok(if stderr {
                core_guest::StreamEvent::Stderr { data }
            } else {
                core_guest::StreamEvent::Stdout { data }
            });
        }
    }
    Err(SandboxError::Execution("invalid guest stream event".into()))
}

fn forkd_value_bytes(value: Option<&serde_json::Value>) -> Result<Vec<u8>, SandboxError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::String(text)) => Ok(text.as_bytes().to_vec()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(|v| u8::try_from(v).ok())
                    .ok_or_else(|| {
                        SandboxError::Execution(
                            "execution output must be UTF-8 or byte array".into(),
                        )
                    })
            })
            .collect(),
        Some(_) => Err(SandboxError::Execution(
            "execution output must be UTF-8 or byte array".into(),
        )),
    }
}
impl SandboxProvider for ForkdClient {
    fn backend(&self) -> BackendKind {
        BackendKind::VirtualMachine
    }
    fn transport(&self) -> TransportKind {
        TransportKind::Tcp
    }
    fn capabilities(&self) -> &[Capability] {
        &self.config.guest_capabilities
    }
    fn create<'a>(
        &'a self,
        spec: SandboxSpec,
    ) -> BoxFuture<'a, Result<Box<dyn Sandbox>, ProviderError>> {
        Box::pin(async move {
            spec.validate().map_err(ProviderError::InvalidSpec)?;
            if let Some(c) = spec
                .capabilities
                .iter()
                .find(|c| !self.capabilities().contains(c))
            {
                return Err(ProviderError::UnsupportedCapability(*c));
            }
            let tag = self
                .config
                .snapshot_tag
                .clone()
                .ok_or_else(|| ProviderError::Unavailable("snapshot tag is required".into()))?;
            let req = CreateSandboxRequest::single(&tag);
            let s = self
                .create(&req)
                .await
                .map_err(|e| ProviderError::Unavailable(e.to_string()))?;
            Ok(Box::new(s) as Box<dyn Sandbox>)
        })
    }
}
