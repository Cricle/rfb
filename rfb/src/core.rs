//! Core RFB contract types: sandbox capabilities, execution specs, image
//! manifests, resource limits, and the `Sandbox`/`SandboxProvider` traits that
//! every backend integration implements.

pub use crate::guest;
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt, future::Future, pin::Pin, time::Duration};

/// Boxed future used by the object-safe `Sandbox` trait methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Stable imports for the platform-neutral RFB contract.
///
/// Backend integrations remain behind their feature-gated modules and are not
/// part of this facade.
pub mod prelude {
    pub use super::guest;
    pub use super::{
        BackendKind, BoxFuture, Capability, ContractError, ExecResult, ExecSpec, ImageManifest,
        ManifestError, PixelFormat, ProviderError, Resources, Sandbox, SandboxError,
        SandboxProvider, SandboxSpec, TransportKind,
    };
}

/// Which runtime backs a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// In-memory fake used by tests and local development.
    InMemory,
    /// A child OS process.
    Process,
    /// A virtual machine (for example Firecracker).
    VirtualMachine,
    /// A custom backend supplied by the application.
    Custom,
}

/// Which transport a sandbox uses to reach its guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Same-process call (in-memory backend).
    InProcess,
    /// TCP networking.
    Tcp,
    /// Unix domain sockets.
    Unix,
    /// Virtio-vsock.
    Vsock,
    /// A custom transport supplied by the application.
    Custom,
}

/// Framebuffer pixel layout advertised by a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    /// 3 bytes per pixel: red, green, blue.
    Rgb8,
    /// 3 bytes per pixel: blue, green, red.
    Bgr8,
    /// 4 bytes per pixel: red, green, blue, alpha (default).
    #[default]
    Rgba8,
    /// 4 bytes per pixel: blue, green, red, alpha.
    Bgra8,
    /// 1 byte per pixel: grayscale.
    Gray8,
}

impl PixelFormat {
    /// Bytes per pixel for this format.
    ///
    /// ```
    /// use rfb::PixelFormat;
    /// assert_eq!(PixelFormat::Rgb8.bytes_per_pixel(), 3);
    /// assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
    /// assert_eq!(PixelFormat::Gray8.bytes_per_pixel(), 1);
    /// ```
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb8 | Self::Bgr8 => 3,
            Self::Rgba8 | Self::Bgra8 => 4,
            Self::Gray8 => 1,
        }
    }
}

/// Image manifest describing the guest image a sandbox runs.
///
/// ```
/// # use rfb::ImageManifest;
/// let image = ImageManifest::new("example:latest");
/// assert_eq!(image.image_ref, "example:latest");
/// assert_eq!(image.transport, "oci");
/// assert!(image.validate().is_ok());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageManifest {
    /// Image reference (for example an OCI reference or filesystem path).
    pub image_ref: String,
    /// Content digest, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Image transport (defaults to `oci`).
    #[serde(default = "default_image_transport")]
    pub transport: String,
    /// Entrypoint arguments of the image.
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// Wire protocol spoken by the guest (defaults to `rfb`).
    #[serde(default = "default_image_protocol")]
    pub protocol: String,
    /// Guest architecture (defaults to `unknown`).
    #[serde(default = "default_image_arch")]
    pub arch: String,
    /// Resource limits associated with the image.
    #[serde(default)]
    pub resources: Resources,
}

fn default_image_transport() -> String {
    "oci".into()
}
fn default_image_protocol() -> String {
    "rfb".into()
}
fn default_image_arch() -> String {
    "unknown".into()
}

fn is_sha256_digest(value: &str) -> bool {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Default for ImageManifest {
    fn default() -> Self {
        Self {
            image_ref: String::new(),
            digest: None,
            transport: default_image_transport(),
            entrypoint: vec![],
            protocol: default_image_protocol(),
            arch: default_image_arch(),
            resources: Resources::default(),
        }
    }
}

impl ImageManifest {
    /// Build a manifest for the given image reference with default fields.
    pub fn new(image_ref: impl Into<String>) -> Self {
        Self {
            image_ref: image_ref.into(),
            ..Self::default()
        }
    }

    /// Validate required fields and nested resources.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.image_ref.trim().is_empty() {
            return Err(ManifestError::EmptyImageRef);
        }
        if self.transport.trim().is_empty() {
            return Err(ManifestError::EmptyTransport);
        }
        if self.protocol.trim().is_empty() {
            return Err(ManifestError::EmptyProtocol);
        }
        if self.arch.trim().is_empty() {
            return Err(ManifestError::EmptyArch);
        }
        if let Some(digest) = &self.digest {
            if !is_sha256_digest(digest) {
                return Err(ManifestError::InvalidDigest);
            }
        }
        self.resources.validate().map_err(|e| match e {
            ContractError::InvalidResource(m) => ManifestError::InvalidResources(m),
            _ => ManifestError::InvalidResources("invalid image resources"),
        })
    }
}

/// Resource limits for a sandbox or image. Absent fields mean "no limit".
///
/// ```
/// # use rfb::Resources;
/// let resources = Resources { cpus: Some(2), memory_bytes: Some(1024), ..Default::default() };
/// assert!(resources.validate().is_ok());
/// assert!(Resources { cpus: Some(0), ..Default::default() }.validate().is_err());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// CPU count limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// Memory limit in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Disk limit in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// Process/thread count limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u32>,
}

impl Resources {
    /// Validate that any present limit is non-zero.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.cpus == Some(0)
            || self.memory_bytes == Some(0)
            || self.disk_bytes == Some(0)
            || self.pids == Some(0)
        {
            Err(ContractError::InvalidResource(
                "resource limits must be non-zero",
            ))
        } else {
            Ok(())
        }
    }
}

/// A capability a sandbox may advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Run a command in the guest.
    Execute,
    /// Read the guest framebuffer.
    ReadFramebuffer,
    /// Write the guest framebuffer.
    WriteFramebuffer,
    /// Resize the guest display.
    Resize,
    /// Snapshot the guest.
    Snapshot,
    /// Deliver signals to the guest.
    Signal,
    /// Report guest health.
    Health,
    /// Stream guest command output.
    Stream,
    /// List directory entries.
    Ls,
    /// Find files.
    Find,
    /// Grep file contents.
    Grep,
    /// Read a file.
    ReadFile,
    /// Write a file.
    WriteFile,
    /// Evaluate code.
    Eval,
    /// Cancel an in-flight request.
    Cancel,
}

impl Capability {
    /// Capabilities a typical guest sandbox advertises.
    pub const fn all_guest() -> &'static [Capability] {
        &[
            Self::Health,
            Self::Stream,
            Self::Ls,
            Self::Find,
            Self::Grep,
            Self::ReadFile,
            Self::WriteFile,
            Self::Eval,
            Self::Cancel,
        ]
    }
}

/// Specification for creating a sandbox.
///
/// ```
/// # use rfb::{Capability, Resources, SandboxSpec};
/// let spec = SandboxSpec { capabilities: vec![Capability::Execute], resources: Resources::default(), ..Default::default() };
/// assert!(spec.validate().is_ok());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Backend kind, when the provider supports more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendKind>,
    /// Transport kind, when the provider supports more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportKind>,
    /// Resource limits.
    #[serde(default)]
    pub resources: Resources,
    /// Capabilities the sandbox must provide.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Guest image manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageManifest>,
}

impl SandboxSpec {
    /// Validate resources and, when present, the image manifest.
    pub fn validate(&self) -> Result<(), ContractError> {
        self.resources.validate()?;
        if let Some(i) = &self.image {
            i.validate().map_err(ContractError::InvalidManifest)?
        };
        Ok(())
    }
}

/// A single command execution request.
///
/// ```
/// # use rfb::ExecSpec;
/// let spec = ExecSpec::new("uname");
/// assert_eq!(spec.command, "uname");
/// assert!(spec.validate().is_ok());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSpec {
    /// Command to run in the guest.
    pub command: String,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory inside the guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional stdin bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<Vec<u8>>,
    /// Optional timeout; zero is rejected by validation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration_millis"
    )]
    pub timeout: Option<Duration>,
}

impl ExecSpec {
    /// Build a spec for a command with no arguments.
    pub fn new(c: impl Into<String>) -> Self {
        Self {
            command: c.into(),
            args: vec![],
            cwd: None,
            stdin: None,
            timeout: None,
        }
    }

    /// Validate the command, timeout, and working directory.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.command.trim().is_empty() {
            return Err(ContractError::EmptyCommand);
        }
        if self.timeout == Some(Duration::ZERO) {
            return Err(ContractError::InvalidTimeout);
        }
        if let Some(c) = &self.cwd {
            validate_guest_cwd(c)
        } else {
            Ok(())
        }
    }
}

/// Validate a guest working directory at the contract level.
///
/// Deliberately lenient about leading `/`: an absolute cwd is meaningful for
/// some backends, and workspace confinement is enforced where the filesystem
/// policy actually lives (each backend's `PathPolicy`), not here. Backends
/// must still reject paths that escape the workspace — this check only stops
/// obviously malformed input (empty, NUL, Windows separators, drive letters,
/// `..` traversal).
fn validate_guest_cwd(c: &str) -> Result<(), ContractError> {
    if c.is_empty()
        || c.as_bytes().contains(&0)
        || c.starts_with('\\')
        || c.contains('\\')
        || (c.len() >= 2 && c.as_bytes()[1] == b':')
        || c.split('/').any(|x| x == "..")
    {
        Err(ContractError::InvalidCwd)
    } else {
        Ok(())
    }
}

pub(crate) mod duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(v: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        v.map(|d| d.as_millis() as u64).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Option::<u64>::deserialize(d).map(|v| v.map(Duration::from_millis))
    }
}

/// Result of a completed guest execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    /// Process exit status, when reported.
    pub status: Option<i32>,
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
    /// Whether the command was killed by a timeout.
    pub timed_out: bool,
}

/// A sandbox exposing typed guest operations over its transport.
pub trait Sandbox: Send + Sync {
    /// Backend kind of this sandbox.
    fn backend(&self) -> BackendKind;
    /// Transport kind of this sandbox.
    fn transport(&self) -> TransportKind;
    /// Capabilities this sandbox advertises.
    fn capabilities(&self) -> &[Capability];
    /// Execute a command in the guest.
    fn exec<'a>(&'a self, spec: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>>;
    /// Read the guest framebuffer.
    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async {
            Err(SandboxError::UnsupportedCapability(
                Capability::ReadFramebuffer,
            ))
        })
    }
    /// Report guest health.
    fn health<'a>(&'a self) -> BoxFuture<'a, Result<guest::Health, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::Health)) })
    }
    /// Ping the guest, returning health if reachable.
    fn ping<'a>(&'a self) -> BoxFuture<'a, Result<guest::Health, SandboxError>> {
        self.health()
    }
    /// Start an interactive command stream.
    fn stream<'a>(
        &'a self,
        _: guest::StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn guest::GuestStream + 'a>, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::Stream)) })
    }
    /// List directory entries in the guest.
    fn ls<'a>(
        &'a self,
        _: guest::LsRequest,
    ) -> BoxFuture<'a, Result<guest::LsResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::Ls)) })
    }
    /// Find files in the guest.
    fn find<'a>(
        &'a self,
        _: guest::FindRequest,
    ) -> BoxFuture<'a, Result<guest::FindResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::Find)) })
    }
    /// Grep file contents in the guest.
    fn grep<'a>(
        &'a self,
        _: guest::GrepRequest,
    ) -> BoxFuture<'a, Result<guest::GrepResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::Grep)) })
    }
    /// Read a file in the guest.
    fn read<'a>(
        &'a self,
        _: guest::ReadRequest,
    ) -> BoxFuture<'a, Result<guest::ReadResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::ReadFile)) })
    }
    /// Alias for [`Sandbox::read`].
    fn read_file<'a>(
        &'a self,
        r: guest::ReadRequest,
    ) -> BoxFuture<'a, Result<guest::ReadResult, SandboxError>> {
        self.read(r)
    }
    /// Write a file in the guest.
    fn write<'a>(
        &'a self,
        _: guest::WriteRequest,
    ) -> BoxFuture<'a, Result<guest::WriteResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::WriteFile)) })
    }
    /// Alias for [`Sandbox::write`].
    fn write_file<'a>(
        &'a self,
        r: guest::WriteRequest,
    ) -> BoxFuture<'a, Result<guest::WriteResult, SandboxError>> {
        self.write(r)
    }
    /// Evaluate code in the guest.
    fn eval<'a>(
        &'a self,
        _: guest::EvalRequest,
    ) -> BoxFuture<'a, Result<guest::EvalResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::Eval)) })
    }
    /// Cancel an in-flight guest request.
    fn cancel<'a>(
        &'a self,
        _: guest::CancelRequest,
    ) -> BoxFuture<'a, Result<guest::CancelResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::UnsupportedCapability(Capability::Cancel)) })
    }
}

/// Creates sandboxes for a backend.
pub trait SandboxProvider: Send + Sync {
    /// Backend kind this provider creates.
    fn backend(&self) -> BackendKind;
    /// Transport kind this provider uses.
    fn transport(&self) -> TransportKind;
    /// Capabilities sandboxes from this provider advertise.
    fn capabilities(&self) -> &[Capability];
    /// Create a sandbox from a specification.
    fn create<'a>(
        &'a self,
        spec: SandboxSpec,
    ) -> BoxFuture<'a, Result<Box<dyn Sandbox>, ProviderError>>;
}

/// Error validating or interpreting an image manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// `image_ref` is empty.
    EmptyImageRef,
    /// `transport` is empty.
    EmptyTransport,
    /// `protocol` is empty.
    EmptyProtocol,
    /// `arch` is empty.
    EmptyArch,
    /// The content digest is not a SHA-256 value.
    InvalidDigest,
    /// Resources failed validation.
    InvalidResources(&'static str),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyImageRef => "image_ref must not be empty",
            Self::EmptyTransport => "image transport must not be empty",
            Self::EmptyProtocol => "image protocol must not be empty",
            Self::EmptyArch => "image arch must not be empty",
            Self::InvalidDigest => "image digest must be a SHA-256 hex value",
            Self::InvalidResources(message) => message,
        };
        f.write_str(message)
    }
}

impl Error for ManifestError {}

/// Error validating a contract value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// Command is empty.
    EmptyCommand,
    /// Timeout is invalid (for example zero).
    InvalidTimeout,
    /// Working directory is invalid or escapes the workspace.
    InvalidCwd,
    /// A resource limit is invalid.
    InvalidResource(&'static str),
    /// The image manifest is invalid.
    InvalidManifest(ManifestError),
    /// A guest path is invalid or escapes.
    InvalidPath(&'static str),
    /// A pattern is invalid.
    InvalidPattern,
    /// A limit was exceeded.
    LimitExceeded,
    /// Code is empty.
    EmptyCode,
    /// Environment is invalid.
    InvalidEnv,
    /// An identifier is invalid.
    InvalidId,
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyCommand => "command must not be empty",
            Self::InvalidTimeout => "timeout must be non-zero",
            Self::InvalidCwd => "working directory is invalid or escapes the workspace",
            Self::InvalidResource(m) => m,
            Self::InvalidManifest(e) => return write!(f, "invalid image manifest: {e}"),
            Self::InvalidPath(m) => m,
            Self::InvalidPattern => "pattern is invalid",
            Self::LimitExceeded => "a limit was exceeded",
            Self::EmptyCode => "code must not be empty",
            Self::InvalidEnv => "environment is invalid",
            Self::InvalidId => "identifier is invalid",
        };
        f.write_str(message)
    }
}

impl Error for ContractError {}

/// Error from a sandbox provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The sandbox specification is invalid.
    InvalidSpec(ContractError),
    /// The provider does not support a requested capability.
    UnsupportedCapability(Capability),
    /// The provider or its runtime is unavailable.
    Unavailable(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec(e) => write!(f, "invalid sandbox spec: {e}"),
            Self::UnsupportedCapability(c) => write!(f, "provider does not support {c:?}"),
            Self::Unavailable(m) => write!(f, "provider unavailable: {m}"),
        }
    }
}

impl Error for ProviderError {}

/// Error from an individual sandbox operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The specification is invalid.
    InvalidSpec(ContractError),
    /// The sandbox does not support the requested capability.
    UnsupportedCapability(Capability),
    /// Command execution failed.
    Execution(String),
    /// A transport-level failure occurred.
    Transport(String),
    /// The sandbox is not ready.
    NotReady,
    /// The operation exceeded its deadline.
    Timeout,
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec(e) => write!(f, "invalid spec: {e}"),
            Self::UnsupportedCapability(c) => write!(f, "sandbox does not support {c:?}"),
            Self::Execution(m) => write!(f, "execution failed: {m}"),
            Self::Transport(m) => write!(f, "transport failure: {m}"),
            Self::NotReady => f.write_str("sandbox is not ready"),
            Self::Timeout => f.write_str("operation exceeded its deadline"),
        }
    }
}

impl Error for SandboxError {}
