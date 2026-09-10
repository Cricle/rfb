//! `ExecutionTarget` / `GuestExecution` tests: fail-closed unsupported targets,
//! exec/eval/structured mapping, timeout classification, and argument
//! validation against an in-process fake sandbox (no real guest required).

use futures::executor::block_on;
use rfb::{
    guest, BackendKind, BoxFuture, Capability, ExecResult, ExecSpec, ImageManifest, Sandbox,
    SandboxError, TransportKind,
};
use rfb_rig::{ExecutionError, ExecutionTarget, GuestExecution};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// An in-process fake sandbox whose exec result is scripted per test.
#[derive(Clone)]
struct FakeSandbox {
    result: Result<ExecResult, SandboxError>,
}

impl Default for FakeSandbox {
    fn default() -> Self {
        Self {
            result: Ok(ExecResult {
                status: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: false,
            }),
        }
    }
}

impl FakeSandbox {
    fn script(result: Result<ExecResult, SandboxError>) -> Self {
        Self { result }
    }
}

impl Sandbox for FakeSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 10] = [
            Capability::Execute,
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Grep,
            Capability::Find,
            Capability::Ls,
            Capability::Eval,
            Capability::Cancel,
            Capability::Stream,
            Capability::Health,
        ];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        let result = self.result.clone();
        Box::pin(async move { result })
    }
    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn ls<'a>(
        &'a self,
        _: guest::LsRequest,
    ) -> BoxFuture<'a, Result<guest::LsResult, SandboxError>> {
        Box::pin(async {
            Ok(guest::LsResult {
                entries: vec![guest::DirEntry {
                    name: "a.txt".into(),
                    is_dir: false,
                    size: None,
                }],
                truncated: false,
            })
        })
    }
    fn eval<'a>(
        &'a self,
        _: guest::EvalRequest,
    ) -> BoxFuture<'a, Result<guest::EvalResult, SandboxError>> {
        Box::pin(async {
            Ok(guest::EvalResult {
                output: b"42".to_vec(),
                status: Some(0),
                timed_out: false,
            })
        })
    }
}

#[test]
fn unsupported_target_has_no_capabilities() {
    let target = ExecutionTarget::unsupported("test");
    assert!(target.capabilities().is_empty());
}

#[test]
fn unsupported_target_fails_closed_for_all_operations() {
    let target = ExecutionTarget::unsupported("not provisioned");
    assert!(target.capabilities().is_empty());

    let error = block_on(target.exec(vec!["id".into()], None)).unwrap_err();
    assert!(matches!(error, ExecutionError::Unsupported(reason) if reason == "not provisioned"));

    let error = block_on(target.eval("1+1".into(), None)).unwrap_err();
    assert!(matches!(error, ExecutionError::Unsupported(_)));

    let error = block_on(target.structured("ls", json!({}))).unwrap_err();
    assert!(matches!(error, ExecutionError::Unsupported(_)));
}

#[test]
fn guest_target_exec_maps_status_and_output() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::script(Ok(ExecResult {
        status: Some(0),
        stdout: b"hello".to_vec(),
        stderr: b"warn".to_vec(),
        timed_out: false,
    })));
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let result = block_on(target.exec(
        vec!["echo".into(), "hi".into()],
        Some(Duration::from_secs(5)),
    ))
    .unwrap();
    assert_eq!(result["status"], 0);
    assert_eq!(result["stdout"], "hello");
    assert_eq!(result["stderr"], "warn");
    assert_eq!(result["timed_out"], false);
}

#[test]
fn guest_target_exec_timeout_maps_to_timeout_error() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::script(Ok(ExecResult {
        status: None,
        stdout: vec![],
        stderr: vec![],
        timed_out: true,
    })));
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let error =
        block_on(target.exec(vec!["sleep".into()], Some(Duration::from_secs(1)))).unwrap_err();
    assert!(matches!(error, ExecutionError::Timeout));
}

#[test]
fn guest_target_exec_rejects_empty_arguments() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::default());
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let error = block_on(target.exec(vec![], None)).unwrap_err();
    assert!(matches!(error, ExecutionError::Invalid(message) if message == "empty command"));
}

#[test]
fn guest_target_exec_rejects_invalid_timeout() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::default());
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let error = block_on(target.exec(vec!["ls".into()], Some(Duration::ZERO))).unwrap_err();
    assert!(matches!(error, ExecutionError::Invalid(_)));
}

#[test]
fn guest_target_eval_maps_result() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::default());
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let result = block_on(target.eval("1+1".into(), None)).unwrap();
    assert_eq!(result["status"], 0);
    assert_eq!(result["output"], json!([52, 50]));
}

#[test]
fn guest_target_eval_rejects_empty_code() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::default());
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let error = block_on(target.eval("   ".into(), None)).unwrap_err();
    assert!(matches!(error, ExecutionError::Invalid(_)));
}

#[test]
fn guest_target_structured_round_trips_typed_ops() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::default());
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let result = block_on(target.structured("ls", json!({"path": "."}))).unwrap();
    assert_eq!(result["entries"][0]["name"], "a.txt");
    assert_eq!(result["truncated"], false);
}

#[test]
fn guest_target_structured_rejects_unknown_tools() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::default());
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let error = block_on(target.structured("rm", json!({}))).unwrap_err();
    assert!(matches!(error, ExecutionError::Unsupported(_)));
}

#[test]
fn guest_target_sandbox_errors_map_through() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::script(Err(SandboxError::Timeout)));
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let error = block_on(target.exec(vec!["ls".into()], None)).unwrap_err();
    assert!(matches!(error, ExecutionError::Timeout));

    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeSandbox::script(Err(
        SandboxError::UnsupportedCapability(Capability::Execute),
    )));
    let target = ExecutionTarget::guest(sandbox, "/workspace");
    let error = block_on(target.exec(vec!["ls".into()], None)).unwrap_err();
    assert!(matches!(error, ExecutionError::Unsupported(_)));
}
