//! Reusable typed guest-operations facade.

use super::{
    CancelRequest, CancelResult, EvalRequest, EvalResult, FindRequest, FindResult, GrepRequest,
    GrepResult, GuestStream, Health, LsRequest, LsResult, ReadRequest, ReadResult, StreamSpec,
    WriteRequest, WriteResult,
};
use crate::{BoxFuture, ExecResult, ExecSpec, Sandbox, SandboxError};
use std::time::Duration;

/// Defaults applied consistently by callers before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationsConfig {
    /// Timeout filled into exec/eval/stream requests that carry none. When the
    /// default is `None`, the request stays without an explicit timeout and
    /// each backend applies its own configured deadline (zeroboot:
    /// `Config.timeout`; forkd: `ForkdConfig.guest_timeout`) — the backend's
    /// own knob is the honest default, so this wrapper does not silently
    /// override it.
    pub default_timeout: Option<Duration>,
}

/// Platform-neutral, typed entry point for all guest operations.
///
/// Every operation validates its request fail-closed before dispatch; exec,
/// eval, and stream fill the configured [`OperationsConfig::default_timeout`]
/// when the request carries none.
pub struct GuestOperations<'a, S: ?Sized> {
    sandbox: &'a S,
    config: OperationsConfig,
}
impl<'a, S: Sandbox + ?Sized> GuestOperations<'a, S> {
    /// Wrap a sandbox with the default configuration.
    pub fn new(sandbox: &'a S) -> Self {
        Self {
            sandbox,
            config: OperationsConfig::default(),
        }
    }
    /// Wrap a sandbox with an explicit configuration.
    pub fn with_config(sandbox: &'a S, config: OperationsConfig) -> Self {
        Self { sandbox, config }
    }
    /// Run a command, filling in the configured default timeout when the caller
    /// did not specify one and rejecting invalid specs before dispatch.
    pub fn exec(&self, mut request: ExecSpec) -> BoxFuture<'_, Result<ExecResult, SandboxError>> {
        self.apply_default_timeout(&mut request.timeout);
        self.validated(request.validate(), move |s| s.exec(request))
    }
    /// Report guest health.
    pub fn health(&self) -> BoxFuture<'_, Result<Health, SandboxError>> {
        self.sandbox.health()
    }
    /// List directory entries.
    pub fn ls(&self, request: LsRequest) -> BoxFuture<'_, Result<LsResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.ls(request))
    }
    /// Find files by name pattern.
    pub fn find(&self, request: FindRequest) -> BoxFuture<'_, Result<FindResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.find(request))
    }
    /// Grep file contents.
    pub fn grep(&self, request: GrepRequest) -> BoxFuture<'_, Result<GrepResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.grep(request))
    }
    /// Read a file.
    pub fn read(&self, request: ReadRequest) -> BoxFuture<'_, Result<ReadResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.read(request))
    }
    /// Write a file.
    pub fn write(&self, request: WriteRequest) -> BoxFuture<'_, Result<WriteResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.write(request))
    }
    /// Evaluate code.
    pub fn eval(
        &self,
        mut request: EvalRequest,
    ) -> BoxFuture<'_, Result<EvalResult, SandboxError>> {
        self.apply_default_timeout(&mut request.timeout);
        self.validated(request.validate(), move |s| s.eval(request))
    }
    /// Cancel an in-flight request.
    pub fn cancel(
        &self,
        request: CancelRequest,
    ) -> BoxFuture<'_, Result<CancelResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.cancel(request))
    }
    /// Open an interactive stream.
    pub fn stream(
        &self,
        mut request: StreamSpec,
    ) -> BoxFuture<'_, Result<Box<dyn GuestStream + '_>, SandboxError>> {
        self.apply_default_timeout(&mut request.timeout);
        self.validated(request.validate(), move |s| s.stream(request))
    }
    /// Fill in `default_timeout` when the request carries no explicit timeout.
    fn apply_default_timeout(&self, timeout: &mut Option<Duration>) {
        if timeout.is_none() {
            *timeout = self.config.default_timeout;
        }
    }
    fn validated<T, F>(
        &self,
        result: Result<(), crate::ContractError>,
        call: F,
    ) -> BoxFuture<'_, Result<T, SandboxError>>
    where
        F: FnOnce(&'a S) -> BoxFuture<'a, Result<T, SandboxError>> + Send + 'a,
        T: Send + 'a,
    {
        match result {
            Ok(()) => call(self.sandbox),
            Err(e) => Box::pin(async { Err(SandboxError::InvalidSpec(e)) }),
        }
    }
}
