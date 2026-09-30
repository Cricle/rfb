//! Shared no-op `Sandbox` stubs for the `GuestOperations` contract tests.
//!
//! Stubs that return fixed per-operation results stay local to their tests
//! (each pins its own mapping); only the "unreachable + dispatch counting"
//! family lives here.

use rfb::guest::{
    CancelRequest, CancelResult, EvalRequest, EvalResult, FindRequest, FindResult, GrepRequest,
    GrepResult, GuestStream, LsRequest, LsResult, ReadRequest, ReadResult, StreamSpec,
    WriteRequest, WriteResult,
};
use rfb::{BoxFuture, Capability, ExecResult, ExecSpec, Sandbox, SandboxError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Counts dispatches so tests can prove validation short-circuits before the
/// sandbox is reached. Every dispatched operation panics if it is ever
/// actually polled (`StrictSandbox` semantics from `guest.rs`).
pub struct PanicSandbox {
    calls: Arc<AtomicUsize>,
}

impl PanicSandbox {
    pub fn new(calls: Arc<AtomicUsize>) -> Self {
        Self { calls }
    }

    fn record(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    fn unreachable<T>() -> BoxFuture<'static, Result<T, SandboxError>> {
        Box::pin(async { unreachable!("strict sandbox must not be dispatched") })
    }
}

impl Sandbox for PanicSandbox {
    fn backend(&self) -> rfb::BackendKind {
        rfb::BackendKind::InMemory
    }
    fn transport(&self) -> rfb::TransportKind {
        rfb::TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        &[]
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn ls<'a>(&'a self, _: LsRequest) -> BoxFuture<'a, Result<LsResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn find<'a>(&'a self, _: FindRequest) -> BoxFuture<'a, Result<FindResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn grep<'a>(&'a self, _: GrepRequest) -> BoxFuture<'a, Result<GrepResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn read<'a>(&'a self, _: ReadRequest) -> BoxFuture<'a, Result<ReadResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn write<'a>(&'a self, _: WriteRequest) -> BoxFuture<'a, Result<WriteResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn eval<'a>(&'a self, _: EvalRequest) -> BoxFuture<'a, Result<EvalResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn cancel<'a>(&'a self, _: CancelRequest) -> BoxFuture<'a, Result<CancelResult, SandboxError>> {
        self.record();
        Self::unreachable()
    }
    fn stream<'a>(
        &'a self,
        _: StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn GuestStream + 'a>, SandboxError>> {
        self.record();
        Self::unreachable()
    }
}
