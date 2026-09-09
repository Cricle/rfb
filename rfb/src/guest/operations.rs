//! Reusable typed guest-operations facade.
#![allow(missing_docs)]
use super::{
    CancelRequest, CancelResult, EvalRequest, EvalResult, FindRequest, FindResult, GrepRequest,
    GrepResult, GuestStream, Health, LsRequest, LsResult, ReadRequest, ReadResult, StreamSpec,
    WriteRequest, WriteResult,
};
use crate::{BoxFuture, ExecResult, ExecSpec, Sandbox, SandboxError};
use std::time::Duration;

/// Defaults applied consistently by callers before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationsConfig {
    pub default_timeout: Duration,
}
impl Default for OperationsConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(1800),
        }
    }
}

/// Platform-neutral, typed entry point for all guest operations.
pub struct GuestOperations<'a, S: ?Sized> {
    sandbox: &'a S,
    config: OperationsConfig,
}
impl<'a, S: Sandbox + ?Sized> GuestOperations<'a, S> {
    pub fn new(sandbox: &'a S) -> Self {
        Self {
            sandbox,
            config: OperationsConfig::default(),
        }
    }
    pub fn with_config(sandbox: &'a S, config: OperationsConfig) -> Self {
        Self { sandbox, config }
    }
    /// Run a command, filling in the configured default timeout when the caller
    /// did not specify one and rejecting invalid specs before dispatch.
    pub fn exec(&self, mut request: ExecSpec) -> BoxFuture<'_, Result<ExecResult, SandboxError>> {
        self.apply_default_timeout(&mut request.timeout);
        self.validated(request.validate(), move |s| s.exec(request))
    }
    pub fn health(&self) -> BoxFuture<'_, Result<Health, SandboxError>> {
        self.sandbox.health()
    }
    pub fn ls(&self, request: LsRequest) -> BoxFuture<'_, Result<LsResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.ls(request))
    }
    pub fn find(&self, request: FindRequest) -> BoxFuture<'_, Result<FindResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.find(request))
    }
    pub fn grep(&self, request: GrepRequest) -> BoxFuture<'_, Result<GrepResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.grep(request))
    }
    pub fn read(&self, request: ReadRequest) -> BoxFuture<'_, Result<ReadResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.read(request))
    }
    pub fn write(&self, request: WriteRequest) -> BoxFuture<'_, Result<WriteResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.write(request))
    }
    pub fn eval(
        &self,
        mut request: EvalRequest,
    ) -> BoxFuture<'_, Result<EvalResult, SandboxError>> {
        self.apply_default_timeout(&mut request.timeout);
        self.validated(request.validate(), move |s| s.eval(request))
    }
    pub fn cancel(
        &self,
        request: CancelRequest,
    ) -> BoxFuture<'_, Result<CancelResult, SandboxError>> {
        self.validated(request.validate(), move |s| s.cancel(request))
    }
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
            *timeout = Some(self.config.default_timeout);
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
