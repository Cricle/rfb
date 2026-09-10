//! Public-API contract tests for the extended guest sandbox surface in
//! `rfb-core`.
//!
//! These integration tests pin the typed, platform-neutral guest DTOs (health,
//! stream, structured filesystem RPCs, read/write/eval/cancel), their
//! validation and serde round-trips, and the backward-compatible defaults on
//! the [`Sandbox`] trait: an implementor that does not opt into a guest
//! operation must receive [`SandboxError::UnsupportedCapability`].

use rfb::guest::{
    CancelRequest, CancelResult, DirEntry, EvalRequest, EvalResult, FindRequest, FindResult,
    GrepMatch, GrepRequest, GrepResult, GuestOperations, GuestStream, Health, LsRequest, LsResult,
    OperationsConfig, ReadRequest, ReadResult, StreamEvent, StreamSpec, WriteRequest, WriteResult,
};
use rfb::{
    BoxFuture, Capability, ContractError, ExecResult, ExecSpec, Sandbox, SandboxError,
    SandboxProvider, SandboxSpec,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test]
async fn operations_facade_applies_timeout_and_validates_paths() {
    struct Fake;
    impl Sandbox for Fake {
        fn backend(&self) -> rfb::BackendKind {
            rfb::BackendKind::InMemory
        }
        fn transport(&self) -> rfb::TransportKind {
            rfb::TransportKind::InProcess
        }
        fn capabilities(&self) -> &[Capability] {
            &[]
        }
        fn exec<'a>(
            &'a self,
            spec: rfb::ExecSpec,
        ) -> BoxFuture<'a, Result<rfb::ExecResult, SandboxError>> {
            assert_eq!(spec.timeout, Some(Duration::from_secs(3)));
            Box::pin(async {
                Ok(rfb::ExecResult {
                    status: Some(0),
                    stdout: vec![],
                    stderr: vec![],
                    timed_out: false,
                })
            })
        }
    }
    let ops = rfb::guest::GuestOperations::with_config(
        &Fake,
        rfb::guest::OperationsConfig {
            default_timeout: Duration::from_secs(3),
        },
    );
    assert_eq!(
        ops.exec(rfb::ExecSpec::new("true")).await.unwrap().status,
        Some(0)
    );
    assert!(matches!(
        ops.read(ReadRequest::new("../escape")).await,
        Err(SandboxError::InvalidSpec(ContractError::InvalidPath(_)))
    ));
}

#[test]
fn capability_all_guest_contains_expected_operations() {
    let guest = Capability::all_guest();
    for expected in [
        Capability::Health,
        Capability::Stream,
        Capability::Ls,
        Capability::Find,
        Capability::Grep,
        Capability::ReadFile,
        Capability::WriteFile,
        Capability::Eval,
        Capability::Cancel,
    ] {
        assert!(
            guest.contains(&expected),
            "missing guest capability {expected:?}"
        );
    }
    assert!(!guest.contains(&Capability::Execute));
}

#[test]
fn health_round_trips_and_defaults() {
    let healthy = Health::healthy();
    assert!(healthy.healthy);
    assert_eq!(
        serde_json::from_value::<Health>(serde_json::to_value(&healthy).unwrap()).unwrap(),
        healthy
    );
    let unhealthy = Health::unhealthy("boom");
    assert!(!unhealthy.healthy);
    assert_eq!(unhealthy.message.as_deref(), Some("boom"));
}

#[test]
fn health_serializes_latency_in_millis() {
    let health = Health {
        healthy: true,
        latency: Some(Duration::from_millis(7)),
        message: None,
    };
    let json = serde_json::to_value(&health).unwrap();
    assert_eq!(json["latency"], 7);
    assert_eq!(json["healthy"], true);
    let back: Health = serde_json::from_value(json).unwrap();
    assert_eq!(back.latency, Some(Duration::from_millis(7)));
}

#[test]
fn stream_event_tagged_round_trip() {
    let cases = [
        (StreamEvent::Started, "started"),
        (
            StreamEvent::Stdout {
                data: b"hi".to_vec(),
            },
            "stdout",
        ),
        (
            StreamEvent::Stderr {
                data: b"oops".to_vec(),
            },
            "stderr",
        ),
        (StreamEvent::Exit { code: Some(3) }, "exit"),
        (StreamEvent::Exit { code: None }, "exit"),
    ];
    for (event, tag) in cases {
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], tag);
        assert_eq!(
            serde_json::from_value::<StreamEvent>(json).unwrap(),
            event,
            "round trip failed for {tag}"
        );
    }
}

#[test]
fn stream_spec_validates_and_round_trips() {
    let mut spec = StreamSpec::new("sh");
    spec.args = vec!["-c".into(), "echo hi".into()];
    spec.cwd = Some("workspace".into());
    spec.pty = Some(true);
    spec.env = vec![("A".into(), "B".into())];
    spec.timeout = Some(Duration::from_millis(25));
    assert!(spec.validate().is_ok());
    let json = serde_json::to_value(&spec).unwrap();
    assert_eq!(json["command"], "sh");
    assert_eq!(json["timeout"], 25);
    assert_eq!(serde_json::from_value::<StreamSpec>(json).unwrap(), spec);
}

#[test]
fn stream_spec_rejects_invalid_state() {
    let mut spec = StreamSpec::new(" ");
    assert!(matches!(spec.validate(), Err(ContractError::EmptyCommand)));
    spec = StreamSpec::new("sh");
    spec.timeout = Some(Duration::ZERO);
    assert!(matches!(
        spec.validate(),
        Err(ContractError::InvalidTimeout)
    ));
    spec = StreamSpec::new("sh");
    spec.env = vec![("".into(), "x".into())];
    assert!(matches!(spec.validate(), Err(ContractError::InvalidEnv)));
}

#[test]
fn ls_request_defaults_and_validation() {
    let req = LsRequest::default();
    assert_eq!(req.path, ".");
    assert_eq!(req.max_results, rfb::guest::MAX_GUEST_RESULTS);
    assert!(req.validate().is_ok());
    let json = serde_json::from_value::<LsRequest>(serde_json::json!({})).unwrap();
    assert_eq!(json.path, ".");
    let mut bad = LsRequest::new("sub");
    bad.max_results = 0;
    assert!(matches!(bad.validate(), Err(ContractError::LimitExceeded)));
    assert!(LsRequest::new("/workspace").validate().is_ok());
    for path in ["", "..", "../escape", "/abs", "a/b/../c", "C:\\x", "a\\b"] {
        assert!(
            LsRequest::new(path).validate().is_err(),
            "ls should reject path {path:?}"
        );
    }
}

#[test]
fn ls_result_round_trips() {
    let result = LsResult {
        entries: vec![
            DirEntry {
                name: "src".into(),
                is_dir: true,
                size: None,
            },
            DirEntry {
                name: "Cargo.toml".into(),
                is_dir: false,
                size: Some(64),
            },
        ],
        truncated: false,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["entries"][0]["is_dir"], true);
    assert_eq!(serde_json::from_value::<LsResult>(json).unwrap(), result);
}

#[test]
fn find_request_validates_pattern_and_results() {
    let req = FindRequest::new("src", "*.rs");
    assert!(req.validate().is_ok());
    let mut empty = FindRequest::new("src", "");
    assert!(matches!(
        empty.validate(),
        Err(ContractError::InvalidPattern)
    ));
    empty.pattern = "x".repeat(rfb::guest::MAX_GUEST_PATTERN_BYTES + 1);
    assert!(matches!(
        empty.validate(),
        Err(ContractError::LimitExceeded)
    ));
    let mut zero = FindRequest::new("src", "*.rs");
    zero.max_results = 0;
    assert!(matches!(zero.validate(), Err(ContractError::LimitExceeded)));
}

#[test]
fn find_result_round_trips() {
    let result = FindResult {
        matches: vec!["src/lib.rs".into(), "src/main.rs".into()],
        truncated: true,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["matches"].as_array().unwrap().len(), 2);
    assert_eq!(json["truncated"], true);
    assert_eq!(serde_json::from_value::<FindResult>(json).unwrap(), result);
}

#[test]
fn grep_request_validates_and_round_trips() {
    let req = GrepRequest::new("src", "TODO");
    assert!(req.validate().is_ok());
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["pattern"], "TODO");
    assert_eq!(json["max_bytes"], rfb::guest::MAX_GUEST_RESULT_BYTES);
    assert_eq!(serde_json::from_value::<GrepRequest>(json).unwrap(), req);
    let mut bad = GrepRequest::new("src", "x");
    bad.max_bytes = 0;
    assert!(matches!(bad.validate(), Err(ContractError::LimitExceeded)));
}

#[test]
fn grep_result_round_trips() {
    let result = GrepResult {
        matches: vec![GrepMatch {
            path: "src/lib.rs".into(),
            line: Some(5),
            column: None,
            text: "// TODO: fix".into(),
        }],
        truncated: false,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["matches"][0]["line"], 5);
    assert_eq!(serde_json::from_value::<GrepResult>(json).unwrap(), result);
}

#[test]
fn read_request_validates_paths_and_caps() {
    let req = ReadRequest::new("/workspace/file.txt");
    assert!(req.validate().is_ok());
    let mut bad = ReadRequest::new("src");
    bad.max_bytes = Some(rfb::guest::MAX_GUEST_RESULT_BYTES + 1);
    assert!(matches!(bad.validate(), Err(ContractError::LimitExceeded)));
    for path in ["C:/x", "a\\b", "..", ""] {
        assert!(
            ReadRequest::new(path).validate().is_err(),
            "read should reject {path:?}"
        );
    }
}

#[test]
fn read_result_round_trips() {
    let result = ReadResult {
        data: b"hello".to_vec(),
        truncated: false,
        total_bytes: Some(5),
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["total_bytes"], 5);
    assert_eq!(serde_json::from_value::<ReadResult>(json).unwrap(), result);
}

#[test]
fn write_request_validates_size_and_mode() {
    let req = WriteRequest::new("/workspace/out.txt", b"data".to_vec());
    req.validate().unwrap();
    let big = WriteRequest::new("/a", vec![0u8; rfb::guest::MAX_GUEST_RESULT_BYTES + 1]);
    assert!(matches!(big.validate(), Err(ContractError::LimitExceeded)));
}

#[test]
fn write_result_round_trips() {
    let result = WriteResult { bytes_written: 42 };
    let json = serde_json::to_value(result).unwrap();
    assert_eq!(json["bytes_written"], 42);
    assert_eq!(serde_json::from_value::<WriteResult>(json).unwrap(), result);
}

#[test]
fn eval_request_validates() {
    let req = EvalRequest::new("print('hi')");
    assert!(req.validate().is_ok());
    assert!(matches!(
        EvalRequest::new(" ").validate(),
        Err(ContractError::EmptyCode)
    ));
    let mut big = EvalRequest::new("x");
    big.code = "x".repeat(rfb::guest::MAX_GUEST_CODE_BYTES + 1);
    assert!(matches!(big.validate(), Err(ContractError::LimitExceeded)));
}

#[test]
fn eval_result_round_trips() {
    let result = EvalResult {
        output: b"hi".to_vec(),
        status: Some(0),
        timed_out: false,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["status"], 0);
    assert_eq!(serde_json::from_value::<EvalResult>(json).unwrap(), result);
}

#[test]
fn cancel_request_validates_id() {
    assert!(CancelRequest::new().validate().is_ok());
    assert!(CancelRequest::with_id("op-123").validate().is_ok());
    assert!(matches!(
        CancelRequest::with_id("bad/id").validate(),
        Err(ContractError::InvalidId)
    ));
    assert!(matches!(
        CancelRequest::with_id("").validate(),
        Err(ContractError::InvalidId)
    ));
}

// --- Backward compatibility: a minimal implementor that opts into nothing. ---

struct MinimalSandbox;
struct MinimalProvider;

impl Sandbox for MinimalSandbox {
    fn backend(&self) -> rfb::BackendKind {
        rfb::BackendKind::InMemory
    }
    fn transport(&self) -> rfb::TransportKind {
        rfb::TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        &[]
    }
    fn exec<'a>(
        &'a self,
        spec: rfb::ExecSpec,
    ) -> BoxFuture<'a, Result<rfb::ExecResult, SandboxError>> {
        let _ = spec;
        Box::pin(async { unreachable!() })
    }
}

impl SandboxProvider for MinimalProvider {
    fn backend(&self) -> rfb::BackendKind {
        rfb::BackendKind::InMemory
    }
    fn transport(&self) -> rfb::TransportKind {
        rfb::TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        &[]
    }
    fn create<'a>(
        &'a self,
        _spec: SandboxSpec,
    ) -> BoxFuture<'a, Result<Box<dyn Sandbox>, rfb::ProviderError>> {
        Box::pin(async { Ok(Box::new(MinimalSandbox) as Box<dyn Sandbox>) })
    }
}

#[tokio::test]
async fn unsupported_guest_operations_return_unsupported_capability() {
    let sandbox: &dyn Sandbox = &MinimalSandbox;
    assert!(matches!(
        sandbox.health().await,
        Err(SandboxError::UnsupportedCapability(Capability::Health))
    ));
    assert!(matches!(
        sandbox.ping().await,
        Err(SandboxError::UnsupportedCapability(Capability::Health))
    ));
    assert!(matches!(
        sandbox.read(ReadRequest::new("/a")).await,
        Err(SandboxError::UnsupportedCapability(Capability::ReadFile))
    ));
    assert!(matches!(
        sandbox.write(WriteRequest::new("/a", vec![])).await,
        Err(SandboxError::UnsupportedCapability(Capability::WriteFile))
    ));
    assert!(matches!(
        sandbox.stream(StreamSpec::new("sh")).await,
        Err(SandboxError::UnsupportedCapability(Capability::Stream))
    ));
    assert!(matches!(
        sandbox.ls(LsRequest::new(".")).await,
        Err(SandboxError::UnsupportedCapability(Capability::Ls))
    ));
    assert!(matches!(
        sandbox.find(FindRequest::new(".", "x")).await,
        Err(SandboxError::UnsupportedCapability(Capability::Find))
    ));
    assert!(matches!(
        sandbox.grep(GrepRequest::new(".", "x")).await,
        Err(SandboxError::UnsupportedCapability(Capability::Grep))
    ));
    assert!(matches!(
        sandbox.read_file(ReadRequest::new("/a")).await,
        Err(SandboxError::UnsupportedCapability(Capability::ReadFile))
    ));
    assert!(matches!(
        sandbox.write_file(WriteRequest::new("/a", vec![])).await,
        Err(SandboxError::UnsupportedCapability(Capability::WriteFile))
    ));
    assert!(matches!(
        sandbox.eval(EvalRequest::new("x")).await,
        Err(SandboxError::UnsupportedCapability(Capability::Eval))
    ));
    assert!(matches!(
        sandbox.cancel(CancelRequest::new()).await,
        Err(SandboxError::UnsupportedCapability(Capability::Cancel))
    ));
    // The provider path is also object-safe and constructible.
    let provider: &dyn SandboxProvider = &MinimalProvider;
    let created: Box<dyn Sandbox> = provider
        .create(SandboxSpec::default())
        .await
        .expect("minimal provider create");
    assert!(matches!(
        created.ping().await,
        Err(SandboxError::UnsupportedCapability(Capability::Health))
    ));
}

#[test]
fn guest_stream_trait_is_object_safe() {
    fn assert_object(_: &mut dyn GuestStream) {}
    let _ = assert_object;
}

// --- Operations facade contract tests. ---

/// Minimal object-safe stream that closes cleanly without emitting events.
struct NoopStream;
impl GuestStream for NoopStream {
    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<StreamEvent>, SandboxError>> {
        Box::pin(async { Ok(None) })
    }
    fn send_input<'a>(&'a mut self, _input: String) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Records the timeout each timeout-bearing operation was dispatched with.
struct TimeoutCapture {
    timeouts: Arc<Mutex<Vec<Duration>>>,
}
impl Sandbox for TimeoutCapture {
    fn backend(&self) -> rfb::BackendKind {
        rfb::BackendKind::InMemory
    }
    fn transport(&self) -> rfb::TransportKind {
        rfb::TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        &[]
    }
    fn exec<'a>(&'a self, spec: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        let timeouts = self.timeouts.clone();
        Box::pin(async move {
            timeouts
                .lock()
                .unwrap()
                .push(spec.timeout.expect("defaulted"));
            Ok(ExecResult {
                status: Some(0),
                stdout: vec![],
                stderr: vec![],
                timed_out: false,
            })
        })
    }
    fn eval<'a>(&'a self, req: EvalRequest) -> BoxFuture<'a, Result<EvalResult, SandboxError>> {
        let timeouts = self.timeouts.clone();
        Box::pin(async move {
            timeouts
                .lock()
                .unwrap()
                .push(req.timeout.expect("defaulted"));
            Ok(EvalResult {
                output: vec![],
                status: Some(0),
                timed_out: false,
            })
        })
    }
    fn stream<'a>(
        &'a self,
        spec: StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn GuestStream + 'a>, SandboxError>> {
        let timeouts = self.timeouts.clone();
        Box::pin(async move {
            timeouts
                .lock()
                .unwrap()
                .push(spec.timeout.expect("defaulted"));
            Ok(Box::new(NoopStream) as Box<dyn GuestStream + 'a>)
        })
    }
}

#[tokio::test]
async fn operations_facade_applies_default_timeout_to_exec_eval_stream() {
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let sandbox = TimeoutCapture {
        timeouts: timeouts.clone(),
    };
    let ops = GuestOperations::with_config(
        &sandbox,
        OperationsConfig {
            default_timeout: Duration::from_secs(42),
        },
    );
    ops.exec(ExecSpec::new("true")).await.unwrap();
    ops.eval(EvalRequest::new("1+1")).await.unwrap();
    ops.stream(StreamSpec::new("sh")).await.unwrap();
    assert_eq!(
        *timeouts.lock().unwrap(),
        vec![
            Duration::from_secs(42),
            Duration::from_secs(42),
            Duration::from_secs(42)
        ]
    );
}

#[tokio::test]
async fn operations_facade_preserves_explicit_timeouts() {
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let sandbox = TimeoutCapture {
        timeouts: timeouts.clone(),
    };
    let ops = GuestOperations::with_config(
        &sandbox,
        OperationsConfig {
            default_timeout: Duration::from_secs(60),
        },
    );
    let mut exec = ExecSpec::new("true");
    exec.timeout = Some(Duration::from_millis(123));
    let mut eval = EvalRequest::new("1+1");
    eval.timeout = Some(Duration::from_millis(456));
    let mut stream = StreamSpec::new("sh");
    stream.timeout = Some(Duration::from_millis(789));
    ops.exec(exec).await.unwrap();
    ops.eval(eval).await.unwrap();
    ops.stream(stream).await.unwrap();
    assert_eq!(
        *timeouts.lock().unwrap(),
        vec![
            Duration::from_millis(123),
            Duration::from_millis(456),
            Duration::from_millis(789),
        ]
    );
}

/// Counts dispatches so tests can prove validation short-circuits before the
/// sandbox is reached. Each operation panics if it is ever polled.
struct StrictSandbox {
    calls: Arc<AtomicUsize>,
}
impl StrictSandbox {
    fn record(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
    fn unreachable<T>() -> BoxFuture<'static, Result<T, SandboxError>> {
        Box::pin(async { unreachable!("strict sandbox must not be dispatched") })
    }
}
impl Sandbox for StrictSandbox {
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

#[tokio::test]
async fn operations_facade_rejects_invalid_requests_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let sandbox = StrictSandbox {
        calls: calls.clone(),
    };
    let ops = GuestOperations::new(&sandbox);

    // Map every typed op's result onto the uniform `Result<(), SandboxError>`
    // so the whole surface can be asserted in one pass.
    let all_invalid = [
        ops.exec(ExecSpec::new(" ")).await.map(|_| ()),
        ops.ls(LsRequest::new("../escape")).await.map(|_| ()),
        ops.find(FindRequest::new(".", "")).await.map(|_| ()),
        ops.grep(GrepRequest::new(".", "")).await.map(|_| ()),
        ops.read(ReadRequest::new("../escape")).await.map(|_| ()),
        ops.write(WriteRequest::new("../escape", vec![]))
            .await
            .map(|_| ()),
        ops.eval(EvalRequest::new(" ")).await.map(|_| ()),
        ops.cancel(CancelRequest::with_id("bad/id"))
            .await
            .map(|_| ()),
        ops.stream(StreamSpec::new(" ")).await.map(|_| ()),
    ];

    for result in all_invalid {
        assert!(
            matches!(result, Err(SandboxError::InvalidSpec(_))),
            "expected InvalidSpec, got {result:?}"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "invalid specs must not be dispatched to the sandbox"
    );
}

/// Returns a configured error for every sandbox operation.
struct FailingSandbox {
    error: SandboxError,
}
impl Sandbox for FailingSandbox {
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
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn ls<'a>(&'a self, _: LsRequest) -> BoxFuture<'a, Result<LsResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
    fn read<'a>(&'a self, _: ReadRequest) -> BoxFuture<'a, Result<ReadResult, SandboxError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
}

#[tokio::test]
async fn operations_facade_propagates_sandbox_errors_unchanged() {
    for expected in [
        SandboxError::UnsupportedCapability(Capability::Ls),
        SandboxError::Execution("boom".into()),
        SandboxError::Transport("lost".into()),
        SandboxError::NotReady,
        SandboxError::Timeout,
    ] {
        let sandbox = FailingSandbox {
            error: expected.clone(),
        };
        let ops = GuestOperations::new(&sandbox);
        assert_eq!(
            ops.exec(ExecSpec::new("true")).await.unwrap_err(),
            expected,
            "exec must propagate {expected:?}"
        );
        assert_eq!(
            ops.ls(LsRequest::new(".")).await.unwrap_err(),
            expected,
            "ls must propagate {expected:?}"
        );
        assert_eq!(
            ops.read(ReadRequest::new("/workspace/f"))
                .await
                .unwrap_err(),
            expected,
            "read must propagate {expected:?}"
        );
    }
}

/// Records the cancel request id each dispatch received.
struct CancelCapture {
    seen: Arc<Mutex<Vec<Option<String>>>>,
}
impl Sandbox for CancelCapture {
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
        Box::pin(async { unreachable!() })
    }
    fn cancel<'a>(
        &'a self,
        req: CancelRequest,
    ) -> BoxFuture<'a, Result<CancelResult, SandboxError>> {
        let seen = self.seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(req.id);
            Ok(CancelResult { cancelled: true })
        })
    }
}

#[tokio::test]
async fn operations_facade_cancel_passes_through_and_validates() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sandbox = CancelCapture { seen: seen.clone() };
    let ops = GuestOperations::new(&sandbox);

    assert!(ops.cancel(CancelRequest::new()).await.unwrap().cancelled);
    assert!(
        ops.cancel(CancelRequest::with_id("op-1"))
            .await
            .unwrap()
            .cancelled
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![None, Some("op-1".into())],
        "valid cancel requests must reach the sandbox"
    );

    assert!(matches!(
        ops.cancel(CancelRequest::with_id("bad/id")).await,
        Err(SandboxError::InvalidSpec(ContractError::InvalidId))
    ));
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "invalid cancel id must not reach the sandbox"
    );
}

/// Canned success for every typed sandbox operation.
struct EchoSandbox;
impl Sandbox for EchoSandbox {
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
        Box::pin(async {
            Ok(ExecResult {
                status: Some(0),
                stdout: vec![],
                stderr: vec![],
                timed_out: false,
            })
        })
    }
    fn health<'a>(&'a self) -> BoxFuture<'a, Result<Health, SandboxError>> {
        Box::pin(async { Ok(Health::healthy()) })
    }
    fn ls<'a>(&'a self, _: LsRequest) -> BoxFuture<'a, Result<LsResult, SandboxError>> {
        Box::pin(async { Ok(LsResult::default()) })
    }
    fn find<'a>(&'a self, _: FindRequest) -> BoxFuture<'a, Result<FindResult, SandboxError>> {
        Box::pin(async { Ok(FindResult::default()) })
    }
    fn grep<'a>(&'a self, _: GrepRequest) -> BoxFuture<'a, Result<GrepResult, SandboxError>> {
        Box::pin(async { Ok(GrepResult::default()) })
    }
    fn read<'a>(&'a self, _: ReadRequest) -> BoxFuture<'a, Result<ReadResult, SandboxError>> {
        Box::pin(async {
            Ok(ReadResult {
                data: b"hi".to_vec(),
                truncated: false,
                total_bytes: Some(2),
            })
        })
    }
    fn write<'a>(&'a self, _: WriteRequest) -> BoxFuture<'a, Result<WriteResult, SandboxError>> {
        Box::pin(async { Ok(WriteResult { bytes_written: 2 }) })
    }
    fn eval<'a>(&'a self, _: EvalRequest) -> BoxFuture<'a, Result<EvalResult, SandboxError>> {
        Box::pin(async {
            Ok(EvalResult {
                output: b"out".to_vec(),
                status: Some(0),
                timed_out: false,
            })
        })
    }
    fn cancel<'a>(&'a self, _: CancelRequest) -> BoxFuture<'a, Result<CancelResult, SandboxError>> {
        Box::pin(async { Ok(CancelResult { cancelled: false }) })
    }
    fn stream<'a>(
        &'a self,
        _: StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn GuestStream + 'a>, SandboxError>> {
        Box::pin(async { Ok(Box::new(NoopStream) as Box<dyn GuestStream + 'a>) })
    }
}

#[tokio::test]
async fn operations_facade_dispatches_all_typed_operations() {
    let ops = GuestOperations::new(&EchoSandbox);
    assert!(ops.exec(ExecSpec::new("true")).await.is_ok());
    assert!(ops.health().await.is_ok());
    assert!(ops.ls(LsRequest::new(".")).await.is_ok());
    assert!(ops.find(FindRequest::new(".", "x")).await.is_ok());
    assert!(ops.grep(GrepRequest::new(".", "x")).await.is_ok());
    assert!(ops.read(ReadRequest::new("/workspace/f")).await.is_ok());
    assert!(ops
        .write(WriteRequest::new("/workspace/f", vec![]))
        .await
        .is_ok());
    assert!(ops.eval(EvalRequest::new("1")).await.is_ok());
    assert!(ops.cancel(CancelRequest::new()).await.is_ok());
    assert!(ops.stream(StreamSpec::new("sh")).await.is_ok());
}
