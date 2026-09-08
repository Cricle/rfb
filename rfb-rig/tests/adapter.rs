//! Public-API integration tests for the `rfb-rig` adapter.
//!
//! These tests exercise [`rfb_rig`] strictly through its public surface and
//! [`rfb::Sandbox`], so they also serve as a compile-time check that the
//! adapter is usable from another crate without access to its internals.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::executor::block_on;
use rfb::guest as guest_dtos;
use rfb::{
    BackendKind, BoxFuture, Capability, ExecResult, ExecSpec, ImageManifest, Sandbox, SandboxError,
    TransportKind,
};
use rfb_rig::{
    execute_schema, portable_dynamic_tools, rig_tools, sandbox_execute_tool, RigCapability,
    RigPortableTools, SandboxExecuteAdapter, SANDBOX_EXECUTE_NAME, TOOL_NAMES,
};
use rig_core::tool::{ToolErrorKind, ToolExecutionError, ToolOutput};
use serde_json::{json, Value};

/// A public in-process fake [`Sandbox`] that records the specs it receives and
/// returns a scripted result.
#[derive(Clone)]
pub struct FakeSandbox {
    state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
struct FakeState {
    specs: Vec<ExecSpec>,
}

impl FakeSandbox {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState::default())),
        }
    }

    /// The specs this sandbox has been asked to execute, in order.
    pub fn exec_specs(&self) -> Vec<ExecSpec> {
        self.state.lock().unwrap().specs.clone()
    }
}

impl Default for FakeSandbox {
    fn default() -> Self {
        Self::new()
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
        &[Capability::Execute]
    }

    fn exec<'a>(&'a self, spec: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        let state = self.state.clone();
        Box::pin(async move {
            let mut state = state.lock().unwrap();
            // Confirm the adapter validated the spec before delegation: an
            // invalid cwd would already have surfaced as an execution error.
            state.specs.push(spec.clone());
            drop(state);
            Ok(ExecResult {
                status: Some(0),
                stdout: spec.command.into_bytes(),
                stderr: Vec::new(),
                timed_out: false,
            })
        })
    }

    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async {
            Err(SandboxError::UnsupportedCapability(
                Capability::ReadFramebuffer,
            ))
        })
    }
}

/// A public fake that scripts a single precomputed result.
///
/// Unlike [`FakeSandbox`], this one lets tests drive every error/timeout branch
/// of the adapter deterministically.
#[derive(Clone)]
pub struct ScriptedSandbox {
    result: Result<ExecResult, SandboxError>,
}

impl ScriptedSandbox {
    pub fn new(result: Result<ExecResult, SandboxError>) -> Self {
        Self { result }
    }
}

impl Sandbox for ScriptedSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }

    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }

    fn capabilities(&self) -> &[Capability] {
        &[]
    }

    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        let result = self.result.clone();
        Box::pin(async move { result })
    }

    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
}

fn run(
    adapter: &SandboxExecuteAdapter,
    arguments: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    block_on(adapter.execute(arguments))
}

#[test]
fn schema_is_strict_and_exposes_the_documented_fields() {
    let schema = execute_schema();

    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["command"]));

    let properties = &schema["properties"];
    assert_eq!(
        properties["command"],
        json!({"type": "string", "minLength": 1})
    );
    assert_eq!(
        properties["args"],
        json!({"type": "array", "items": {"type": "string"}})
    );
    assert_eq!(
        properties["stdin"],
        json!({"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 255}})
    );
    assert_eq!(properties["cwd"], json!({"type": "string"}));
    assert_eq!(
        properties["timeout_ms"],
        json!({"type": "integer", "minimum": 1})
    );
    // The adapter does not claim fields the core contract does not expose.
    assert!(properties.get("max_output_bytes").is_none());
    assert!(properties.get("cwd_absolute").is_none());
}

#[test]
fn tool_surface_exposes_the_stable_name_and_schema() {
    let tool = sandbox_execute_tool(Arc::new(FakeSandbox::new()));
    assert_eq!(tool.name(), SANDBOX_EXECUTE_NAME);
    assert_eq!(tool.definition().name, SANDBOX_EXECUTE_NAME);
    assert_eq!(tool.definition().parameters["additionalProperties"], false);
}

#[test]
fn stable_surface_registers_only_advertised_capabilities() {
    assert_eq!(
        TOOL_NAMES,
        ["read", "write", "edit", "bash", "grep", "find", "ls"]
    );
    let sandbox = Arc::new(AllGuestSandbox);
    let tools = portable_dynamic_tools(sandbox);
    let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(names, TOOL_NAMES);

    let execute_only = Arc::new(FakeSandbox::new());
    assert!(rig_tools(execute_only)
        .tools()
        .iter()
        .any(|tool| tool.name() == "bash"));
    let execute_only_tools = rig_tools(Arc::new(FakeSandbox::new()));
    assert!(!execute_only_tools
        .tools()
        .iter()
        .any(|tool| tool.name() == "read"));
}

#[derive(Default)]
struct AllGuestSandbox;
impl Sandbox for AllGuestSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 10] = [
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Execute,
            Capability::Grep,
            Capability::Find,
            Capability::Ls,
            Capability::Health,
            Capability::Stream,
            Capability::Eval,
            Capability::Cancel,
        ];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
}

#[test]
fn cwd_is_accepted_as_an_opaque_guest_path_and_forwarded_uninterpreted() {
    let sandbox = FakeSandbox::new();
    let adapter = SandboxExecuteAdapter::new(sandbox.clone());

    let result = run(
        &adapter,
        json!({"command": "pwd", "cwd": "/guest/work dir/src"}),
    );
    assert!(result.is_ok(), "valid guest paths must reach the sandbox");
    let forwarded = &sandbox.exec_specs()[0];
    assert_eq!(forwarded.cwd.as_deref(), Some("/guest/work dir/src"));
}

#[test]
fn invalid_cwd_is_rejected_before_reaching_the_sandbox() {
    // `..` traversal and drive-letter prefixes are host-path concerns and must
    // be rejected even though the adapter hands cwd through as an opaque value.
    for cwd in [
        "/workspace/../escape",
        "C:/workspace",
        "workspace\\escape",
        "",
    ] {
        let sandbox = FakeSandbox::new();
        let adapter = SandboxExecuteAdapter::new(sandbox.clone());
        let error = run(&adapter, json!({"command": "pwd", "cwd": cwd}))
            .expect_err("invalid cwd must be rejected");
        assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
        assert_eq!(error.code(), None);
        assert!(
            sandbox.exec_specs().is_empty(),
            "sandbox was never called for {cwd:?}"
        );
    }
}

#[test]
fn structured_output_matches_the_documented_contract() {
    let sandbox = ScriptedSandbox::new(Ok(ExecResult {
        status: Some(7),
        stdout: "héllo\n".as_bytes().to_vec(),
        stderr: vec![0xff, 0xfe],
        timed_out: false,
    }));
    let adapter = SandboxExecuteAdapter::new(sandbox);

    let out = run(
        &adapter,
        json!({"command": "echo", "args": ["héllo"], "stdin": [104, 105, 0]}),
    )
    .unwrap();

    let payload = out.as_json().expect("output is JSON");
    assert_eq!(payload["status"], json!(7));
    // Bytes are surfaced lossily as text so models can consume them; the
    // original bytes remain intact in the core result.
    assert_eq!(payload["stdout"], "héllo\n");
    assert_eq!(payload["stderr"], "��");
    assert_eq!(payload["timed_out"], false);
    assert_eq!(payload["liveness"], "core_only");
}

#[test]
fn exec_spec_is_forwarded_with_args_stdin_and_timeout() {
    let sandbox = FakeSandbox::new();
    let adapter = SandboxExecuteAdapter::new(sandbox.clone());

    run(
        &adapter,
        json!({
            "command": "ls",
            "args": ["-la", "src"],
            "stdin": [98, 121, 116, 101, 115],
            "timeout_ms": 1500
        }),
    )
    .unwrap();

    let spec = &sandbox.exec_specs()[0];
    // The adapter speaks a shell-string contract: commands are forwarded as
    // `/bin/sh -c <command>` with structured args appended positionally.
    assert_eq!(spec.command, "/bin/sh");
    assert_eq!(spec.args, ["-c", "ls", "-la", "src"]);
    assert_eq!(spec.stdin.as_deref(), Some(&b"bytes"[..]));
    assert_eq!(spec.timeout, Some(Duration::from_millis(1500)));
}

#[test]
fn unknown_fields_and_missing_command_are_rejected() {
    let sandbox = FakeSandbox::new();
    let adapter = SandboxExecuteAdapter::new(sandbox.clone());

    for arguments in [
        json!({"command": "x", "extra": 1}),
        json!({"command": "x", "cwd": "/ok", "junk": true}),
        json!({}),
        json!({"args": ["naked"]}),
    ] {
        let error = run(&adapter, arguments).expect_err("must be rejected");
        assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
        assert!(sandbox.exec_specs().is_empty(), "sandbox was never called");
    }
}

#[test]
fn overlong_stdin_bytes_and_zero_timeout_are_rejected() {
    let sandbox = FakeSandbox::new();
    let adapter = SandboxExecuteAdapter::new(sandbox.clone());

    assert_eq!(
        run(&adapter, json!({"command": "x", "stdin": [0, 256]}))
            .unwrap_err()
            .kind(),
        ToolErrorKind::InvalidArgs
    );
    assert_eq!(
        run(&adapter, json!({"command": "x", "timeout_ms": 0}))
            .unwrap_err()
            .kind(),
        ToolErrorKind::InvalidArgs
    );
    assert!(sandbox.exec_specs().is_empty());
}

#[test]
fn timeout_is_classified_and_redacted() {
    let sandbox = ScriptedSandbox::new(Ok(ExecResult {
        status: None,
        stdout: vec![0xde, 0xad],
        stderr: "partial".as_bytes().to_vec(),
        timed_out: true,
    }));
    let adapter = SandboxExecuteAdapter::new(sandbox);

    let error = run(&adapter, json!({"command": "slow"})).unwrap_err();

    assert_eq!(error.kind(), ToolErrorKind::Timeout);
    assert_eq!(error.code(), Some("execution_timeout"));
    assert_eq!(error.retryable(), Some(true));
    // The model sees only stable feedback, never partial stdout/stderr.
    assert_eq!(error.model_feedback(), Some("tool execution timed out"));
    assert!(!error.model_output().render().contains("partial"));
}

#[test]
fn transport_diagnostics_are_redacted_from_model_visible_output() {
    let secret = "tcp://sandbox.internal/session/secret-token";
    let sandbox = ScriptedSandbox::new(Err(SandboxError::Transport(secret.to_string())));
    let adapter = SandboxExecuteAdapter::new(sandbox);

    let error = run(&adapter, json!({"command": "x"})).unwrap_err();

    assert_eq!(error.kind(), ToolErrorKind::Network);
    assert_eq!(error.code(), Some("transport"));
    assert_eq!(
        error.model_feedback(),
        Some("the tool could not reach its upstream service")
    );
    // The operator-facing message still carries the diagnostics for logs...
    assert!(error.message().contains(secret));
    // ...but they never reach the model presentation.
    assert!(!error.model_output().render().contains("secret-token"));
    assert!(!error.model_output().render().contains("sandbox.internal"));
}

#[test]
fn execution_diagnostics_are_redacted_from_model_visible_output() {
    let secret = "payload of restricted data";
    let sandbox = ScriptedSandbox::new(Err(SandboxError::Execution(secret.to_string())));
    let adapter = SandboxExecuteAdapter::new(sandbox);

    let error = run(&adapter, json!({"command": "x"})).unwrap_err();

    assert_eq!(error.kind(), ToolErrorKind::Other);
    assert_eq!(error.code(), Some("execution"));
    assert!(error.message().contains(secret));
    assert_eq!(error.model_feedback(), Some("the tool failed"));
    assert!(!error.model_output().render().contains(secret));
}

#[test]
fn unsupported_capability_is_permission_denied() {
    let sandbox = ScriptedSandbox::new(Err(SandboxError::UnsupportedCapability(
        Capability::ReadFramebuffer,
    )));
    let adapter = SandboxExecuteAdapter::new(sandbox);

    let error = run(&adapter, json!({"command": "x"})).unwrap_err();

    assert_eq!(error.kind(), ToolErrorKind::PermissionDenied);
    assert_eq!(error.code(), Some("unsupported_capability"));
    assert_eq!(error.model_feedback(), Some("the tool denied the request"));
}

#[test]
fn not_ready_is_classified_and_safe() {
    let sandbox = ScriptedSandbox::new(Err(SandboxError::NotReady));
    let adapter = SandboxExecuteAdapter::new(sandbox);

    let error = run(&adapter, json!({"command": "x"})).unwrap_err();

    assert_eq!(error.kind(), ToolErrorKind::Other);
    assert_eq!(error.code(), Some("not_ready"));
    assert_eq!(error.model_feedback(), Some("the tool failed"));
}

// ---------------------------------------------------------------------------
// Structured tool surface (read/write/edit/ls/find/grep/stream/eval/cancel)
// ---------------------------------------------------------------------------

/// An in-memory structured sandbox that answers the typed guest RPCs.
#[derive(Clone, Default)]
struct StructuredSandbox {
    files: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl StructuredSandbox {
    fn with_file(path: &str, data: &[u8]) -> Self {
        let mut files = std::collections::HashMap::new();
        files.insert(path.to_string(), data.to_vec());
        Self {
            files: Arc::new(Mutex::new(files)),
        }
    }
}

impl Sandbox for StructuredSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 10] = [
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Execute,
            Capability::Grep,
            Capability::Find,
            Capability::Ls,
            Capability::Health,
            Capability::Stream,
            Capability::Eval,
            Capability::Cancel,
        ];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn read<'a>(
        &'a self,
        request: guest_dtos::ReadRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::ReadResult, SandboxError>> {
        let files = self.files.clone();
        Box::pin(async move {
            let data = files
                .lock()
                .unwrap()
                .get(&request.path)
                .cloned()
                .unwrap_or_default();
            let total = data.len() as u64;
            let start = (request.offset.unwrap_or(0) as usize).min(data.len());
            let data = match request.max_bytes {
                Some(max) => data[start..(start + max).min(data.len())].to_vec(),
                None => data[start..].to_vec(),
            };
            Ok(guest_dtos::ReadResult {
                data,
                truncated: false,
                total_bytes: Some(total),
            })
        })
    }
    fn write<'a>(
        &'a self,
        request: guest_dtos::WriteRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::WriteResult, SandboxError>> {
        let files = self.files.clone();
        Box::pin(async move {
            let mut files = files.lock().unwrap();
            let mut data = if request.append {
                files.get(&request.path).cloned().unwrap_or_default()
            } else {
                Vec::new()
            };
            data.extend_from_slice(&request.data);
            let bytes = data.len() as u64;
            files.insert(request.path, data);
            Ok(guest_dtos::WriteResult {
                bytes_written: bytes,
            })
        })
    }
    fn ls<'a>(
        &'a self,
        request: guest_dtos::LsRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::LsResult, SandboxError>> {
        let files = self.files.clone();
        Box::pin(async move {
            let mut names: Vec<String> = files
                .lock()
                .unwrap()
                .keys()
                .filter(|key| request.path == "." || key.starts_with(&request.path))
                .cloned()
                .collect();
            names.sort();
            let entries = names
                .into_iter()
                .take(request.max_results)
                .map(|name| guest_dtos::DirEntry {
                    name,
                    is_dir: false,
                    size: None,
                })
                .collect();
            Ok(guest_dtos::LsResult {
                entries,
                truncated: false,
            })
        })
    }
    fn find<'a>(
        &'a self,
        request: guest_dtos::FindRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::FindResult, SandboxError>> {
        let files = self.files.clone();
        Box::pin(async move {
            let mut matches: Vec<String> = files
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.contains(&request.pattern))
                .cloned()
                .collect();
            matches.sort();
            matches.truncate(request.max_results);
            Ok(guest_dtos::FindResult {
                matches,
                truncated: false,
            })
        })
    }
    fn grep<'a>(
        &'a self,
        request: guest_dtos::GrepRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::GrepResult, SandboxError>> {
        let files = self.files.clone();
        Box::pin(async move {
            let files = files.lock().unwrap();
            let mut matches = Vec::new();
            for (path, data) in files.iter() {
                let text = String::from_utf8_lossy(data);
                for line in text.lines().filter(|line| line.contains(&request.pattern)) {
                    if matches.len() >= request.max_results {
                        break;
                    }
                    matches.push(guest_dtos::GrepMatch {
                        path: path.clone(),
                        line: None,
                        column: None,
                        text: line.to_string(),
                    });
                }
            }
            Ok(guest_dtos::GrepResult {
                matches,
                truncated: false,
            })
        })
    }
    fn eval<'a>(
        &'a self,
        _request: guest_dtos::EvalRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::EvalResult, SandboxError>> {
        Box::pin(async { Ok(guest_dtos::EvalResult::default()) })
    }
    fn cancel<'a>(
        &'a self,
        _request: guest_dtos::CancelRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::CancelResult, SandboxError>> {
        Box::pin(async { Ok(guest_dtos::CancelResult { cancelled: true }) })
    }
}

/// A structured sandbox that scripts one error for every typed RPC.
#[derive(Clone)]
struct ScriptedStructuredError {
    error: SandboxError,
}

impl Sandbox for ScriptedStructuredError {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 10] = [
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Execute,
            Capability::Grep,
            Capability::Find,
            Capability::Ls,
            Capability::Health,
            Capability::Stream,
            Capability::Eval,
            Capability::Cancel,
        ];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn read<'a>(
        &'a self,
        _: guest_dtos::ReadRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::ReadResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn write<'a>(
        &'a self,
        _: guest_dtos::WriteRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::WriteResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn ls<'a>(
        &'a self,
        _: guest_dtos::LsRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::LsResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn find<'a>(
        &'a self,
        _: guest_dtos::FindRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::FindResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn grep<'a>(
        &'a self,
        _: guest_dtos::GrepRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::GrepResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn eval<'a>(
        &'a self,
        _: guest_dtos::EvalRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::EvalResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn cancel<'a>(
        &'a self,
        _: guest_dtos::CancelRequest,
    ) -> BoxFuture<'a, Result<guest_dtos::CancelResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn stream<'a>(
        &'a self,
        _: guest_dtos::StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn guest_dtos::GuestStream + 'a>, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
}

struct FakeStream {
    events: std::collections::VecDeque<guest_dtos::StreamEvent>,
}

impl guest_dtos::GuestStream for FakeStream {
    fn next_event<'a>(
        &'a mut self,
    ) -> BoxFuture<'a, Result<Option<guest_dtos::StreamEvent>, SandboxError>> {
        Box::pin(async move { Ok(self.events.pop_front()) })
    }
    fn send_input<'a>(&'a mut self, _input: String) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct StreamingSandbox;

impl Sandbox for StreamingSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 1] = [Capability::Stream];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn stream<'a>(
        &'a self,
        spec: guest_dtos::StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn guest_dtos::GuestStream + 'a>, SandboxError>> {
        Box::pin(async move {
            Ok(Box::new(FakeStream {
                events: std::collections::VecDeque::from(vec![
                    guest_dtos::StreamEvent::Started,
                    guest_dtos::StreamEvent::Stdout {
                        data: spec.command.into_bytes(),
                    },
                    guest_dtos::StreamEvent::Exit { code: Some(0) },
                ]),
            }) as Box<dyn guest_dtos::GuestStream + 'a>)
        })
    }
}

fn invoke_tool(
    sandbox: &Arc<dyn Sandbox>,
    capability: RigCapability,
    args: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    let tools = RigPortableTools::from_sandbox(sandbox.clone());
    block_on(tools.invoke(capability, args))
}

/// Invoke the stream tool. `RigPortableTools::from_sandbox` only registers the
/// default seven-tool surface, so the surface must explicitly declare the
/// `Stream` capability (which the sandbox stub advertises in `capabilities()`)
/// for the stream tool to be reachable at all.
fn invoke_stream_tool(
    sandbox: &Arc<dyn Sandbox>,
    args: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    let tools = RigPortableTools::new(sandbox.clone(), [RigCapability::Stream]);
    block_on(tools.invoke(RigCapability::Stream, args))
}

#[test]
fn structured_read_write_tools_round_trip() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(StructuredSandbox::default());
    let written = invoke_tool(
        &sandbox,
        RigCapability::Write,
        json!({"path": "notes.txt", "data": [104, 105, 0]}),
    )
    .unwrap();
    assert_eq!(written.as_json().unwrap()["bytes_written"], json!(3));

    let read = invoke_tool(&sandbox, RigCapability::Read, json!({"path": "notes.txt"})).unwrap();
    assert_eq!(read.as_json().unwrap()["data"], json!([104, 105, 0]));
    assert_eq!(read.as_json().unwrap()["total_bytes"], json!(3));
}

#[test]
fn structured_tools_register_only_advertised_capabilities() {
    let read_only: Arc<dyn Sandbox> = Arc::new(ReadOnlySandbox);
    let tools = RigPortableTools::from_sandbox(read_only.clone());
    let registered = tools.tools();
    let names: Vec<&str> = registered.iter().map(|tool| tool.name()).collect();
    assert!(names.contains(&"read"));
    assert!(
        !names.contains(&"write"),
        "write must be gated by WriteFile"
    );
    assert!(!names.contains(&"edit"), "edit must be gated by read+write");
    assert!(!names.contains(&"bash"), "bash must be gated by Execute");
}

#[derive(Default)]
struct ReadOnlySandbox;
impl Sandbox for ReadOnlySandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 1] = [Capability::ReadFile];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn framebuffer<'a>(&'a self) -> BoxFuture<'a, Result<ImageManifest, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
}

#[test]
fn edit_tool_single_and_replace_all_semantics() {
    let sandbox: Arc<dyn Sandbox> =
        Arc::new(StructuredSandbox::with_file("a.txt", b"one two three"));
    invoke_tool(
        &sandbox,
        RigCapability::Edit,
        json!({"path": "a.txt", "old_text": "one", "new_text": "1"}),
    )
    .unwrap();
    let read = invoke_tool(&sandbox, RigCapability::Read, json!({"path": "a.txt"})).unwrap();
    assert_eq!(
        read.as_json().unwrap()["data"],
        json!([49, 32, 116, 119, 111, 32, 116, 104, 114, 101, 101])
    );

    // A non-unique match without replace_all fails closed.
    let sandbox: Arc<dyn Sandbox> = Arc::new(StructuredSandbox::with_file("b.txt", b"one two one"));
    let error = invoke_tool(
        &sandbox,
        RigCapability::Edit,
        json!({"path": "b.txt", "old_text": "one", "new_text": "1"}),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
    assert!(error.message().contains("exactly once"));

    // replace_all applies to every occurrence.
    invoke_tool(
        &sandbox,
        RigCapability::Edit,
        json!({"path": "b.txt", "old_text": "one", "new_text": "1", "replace_all": true}),
    )
    .unwrap();
    let read = invoke_tool(&sandbox, RigCapability::Read, json!({"path": "b.txt"})).unwrap();
    assert_eq!(
        read.as_json().unwrap()["data"],
        json!([49, 32, 116, 119, 111, 32, 49])
    );
}

#[test]
fn edit_tool_reports_missing_text_and_non_utf8_content() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(StructuredSandbox::with_file("a.txt", b"hello world"));
    let error = invoke_tool(
        &sandbox,
        RigCapability::Edit,
        json!({"path": "a.txt", "old_text": "zzz", "new_text": "x"}),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
    assert!(error.message().contains("not found"));

    let error = invoke_tool(
        &sandbox,
        RigCapability::Edit,
        json!({"path": "a.txt", "old_text": "", "new_text": "x"}),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
    assert!(error.message().contains("must not be empty"));

    let binary: Arc<dyn Sandbox> = Arc::new(StructuredSandbox::with_file("bin.dat", &[0xff, 0xfe]));
    let error = invoke_tool(
        &binary,
        RigCapability::Edit,
        json!({"path": "bin.dat", "old_text": "a", "new_text": "b"}),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
    assert!(error.message().contains("UTF-8"));
}

#[test]
fn structured_invalid_args_are_rejected_before_reaching_the_sandbox() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(StructuredSandbox::default());
    for (capability, args) in [
        (RigCapability::Read, json!({"path": ""})),
        (RigCapability::Read, json!({"path": "../etc/passwd"})),
        (RigCapability::Read, json!({"path": "x", "max_bytes": 0})),
        (RigCapability::Write, json!({"path": "", "data": [1]})),
        (RigCapability::Write, json!({"path": "x", "data": [0, 256]})),
        (RigCapability::Ls, json!({"path": ".", "max_results": 0})),
        (RigCapability::Find, json!({"path": ".", "pattern": ""})),
        (
            RigCapability::Grep,
            json!({"path": ".", "pattern": "x", "max_bytes": 0}),
        ),
    ] {
        let error = invoke_tool(&sandbox, capability, args.clone()).unwrap_err();
        assert_eq!(
            error.kind(),
            ToolErrorKind::InvalidArgs,
            "{capability:?} with {args:?}"
        );
    }

    // `eval` is not part of the default structured tool surface, so the
    // capability gate fires before argument validation: even invalid args are
    // surfaced as PermissionDenied rather than InvalidArgs.
    for args in [json!({"code": "  "}), json!({"code": "1+1", "timeout": 0})] {
        let error = invoke_tool(&sandbox, RigCapability::Eval, args.clone()).unwrap_err();
        assert_eq!(
            error.kind(),
            ToolErrorKind::PermissionDenied,
            "unregistered eval with {args:?}"
        );
        assert_eq!(error.code(), Some("unsupported_capability"));
    }
}

#[test]
fn structured_ls_find_grep_return_typed_json() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(StructuredSandbox::with_file(
        "src/main.rs",
        b"fn main() {}\n",
    ));
    let ls = invoke_tool(&sandbox, RigCapability::Ls, json!({"path": "."})).unwrap();
    let entries = ls.as_json().unwrap()["entries"].as_array().unwrap();
    assert!(entries.iter().any(|entry| entry["name"] == "src/main.rs"));

    let find = invoke_tool(
        &sandbox,
        RigCapability::Find,
        json!({"path": ".", "pattern": "main"}),
    )
    .unwrap();
    let matches = find.as_json().unwrap()["matches"].as_array().unwrap();
    assert!(matches
        .iter()
        .any(|path| path.as_str() == Some("src/main.rs")));

    let grep = invoke_tool(
        &sandbox,
        RigCapability::Grep,
        json!({"path": ".", "pattern": "fn main"}),
    )
    .unwrap();
    let matches = grep.as_json().unwrap()["matches"].as_array().unwrap();
    assert!(matches.iter().any(|m| m["text"] == "fn main() {}"));
}

#[test]
fn structured_timeout_error_is_classified_and_safe() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(ScriptedStructuredError {
        error: SandboxError::Timeout,
    });
    let error = invoke_tool(&sandbox, RigCapability::Read, json!({"path": "a.txt"})).unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::Timeout);
    assert_eq!(error.code(), Some("execution_timeout"));
    assert_eq!(error.retryable(), Some(true));
    assert_eq!(error.model_feedback(), Some("tool execution timed out"));
}

#[test]
fn structured_transport_error_is_redacted_from_model_visible_output() {
    let secret = "tcp://sandbox.internal/session/secret-token";
    let sandbox: Arc<dyn Sandbox> = Arc::new(ScriptedStructuredError {
        error: SandboxError::Transport(secret.to_string()),
    });
    let error = invoke_tool(&sandbox, RigCapability::Ls, json!({"path": "."})).unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::Network);
    assert_eq!(error.code(), Some("transport"));
    assert!(error.message().contains(secret));
    assert!(!error.model_output().render().contains("secret-token"));
    assert!(!error.model_output().render().contains("sandbox.internal"));
}

#[test]
fn structured_invalid_spec_error_is_invalid_args() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(ScriptedStructuredError {
        error: SandboxError::InvalidSpec(rfb::ContractError::InvalidCwd),
    });
    let error = invoke_tool(&sandbox, RigCapability::Ls, json!({"path": "."})).unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
    assert_eq!(error.code(), Some("invalid_spec"));
}

#[test]
fn structured_execution_error_is_classified_and_redacted() {
    let secret = "inner restricted payload";
    let sandbox: Arc<dyn Sandbox> = Arc::new(ScriptedStructuredError {
        error: SandboxError::Execution(secret.to_string()),
    });
    let error = invoke_tool(
        &sandbox,
        RigCapability::Grep,
        json!({"path": ".", "pattern": "x"}),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::Other);
    assert_eq!(error.code(), Some("execution"));
    assert!(error.message().contains(secret));
    assert!(!error.model_output().render().contains(secret));
}

#[test]
fn structured_unsupported_capability_is_permission_denied() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(ScriptedStructuredError {
        error: SandboxError::UnsupportedCapability(Capability::ReadFramebuffer),
    });
    let error = invoke_tool(&sandbox, RigCapability::Read, json!({"path": "a.txt"})).unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::PermissionDenied);
    assert_eq!(error.code(), Some("unsupported_capability"));
    assert_eq!(error.model_feedback(), Some("the tool denied the request"));
}

#[test]
fn structured_stream_tool_drains_guest_events() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(StreamingSandbox);
    let out = invoke_stream_tool(
        &sandbox,
        json!({"command": "sh", "args": ["-c", "echo hi"]}),
    )
    .unwrap();
    let events = out.as_json().unwrap().as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["kind"], "started");
    assert_eq!(events[1]["kind"], "stdout");
    assert_eq!(events[1]["data"], json!([115, 104]));
    assert_eq!(events[2]["kind"], "exit");
    assert_eq!(events[2]["code"], json!(0));
}

#[test]
fn structured_stream_tool_surfaces_sandbox_errors() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(ScriptedStructuredError {
        error: SandboxError::Transport("stream relay unreachable".into()),
    });
    let error = invoke_stream_tool(&sandbox, json!({"command": "sh"})).unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::Network);
    assert_eq!(error.code(), Some("transport"));
}
