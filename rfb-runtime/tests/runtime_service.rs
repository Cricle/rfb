mod common;

use std::fs;

fn exec_prompt(argv: &[&str], timeout_secs: Option<u64>) -> String {
    let mut prompt = serde_json::json!({"op": "exec", "args": argv, "cwd": "."});
    if let Some(timeout_secs) = timeout_secs {
        prompt["timeout_secs"] = serde_json::json!(timeout_secs);
    }
    prompt.to_string()
}

#[cfg(unix)]
fn slow_command() -> [&'static str; 3] {
    ["sh", "-c", "sleep 5"]
}

#[cfg(windows)]
fn slow_command() -> [&'static str; 3] {
    ["cmd", "/C", "ping 127.0.0.1 -n 6 >NUL"]
}

#[cfg(unix)]
fn large_output_command() -> [&'static str; 3] {
    [
        "sh",
        "-c",
        "dd if=/dev/zero bs=1048576 count=2 2>/dev/null; dd if=/dev/zero bs=1048576 count=2 1>&2 2>/dev/null",
    ]
}

#[cfg(windows)]
fn large_output_command() -> [&'static str; 3] {
    [
        "cmd",
        "/C",
        "for /L %i in (1,1,2000) do @echo 1234567890123456789012345678901234567890",
    ]
}
use rfb_runtime::resources::RuntimeLimits;
use rfb_runtime::runtime_service::{GuestEvent, GuestExecutor, RuntimeService};
use rfb_runtime::session::{
    ControlMessage, FileReadRequest, FileWriteRequest, RuntimeMessage, SessionRequest,
};
use rfb_runtime::workspace_executor::WorkspaceGuestExecutor;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingExecutor {
    starts: Mutex<Vec<String>>,
    cancels: Mutex<Vec<(String, String)>>,
    shutdowns: Mutex<usize>,
    events: Vec<GuestEvent>,
}

impl GuestExecutor for RecordingExecutor {
    fn start_turn(&mut self, request: &SessionRequest) -> Result<Vec<GuestEvent>, String> {
        self.starts.lock().unwrap().push(request.prompt.clone());
        Ok(self.events.clone())
    }
    fn cancel(&mut self, session_id: &str, request_id: &str) -> Result<(), String> {
        self.cancels
            .lock()
            .unwrap()
            .push((session_id.into(), request_id.into()));
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), String> {
        *self.shutdowns.lock().unwrap() += 1;
        Ok(())
    }
}

fn temp_workspace() -> std::path::PathBuf {
    common::unique_temp_dir("rfb-runtime-test")
}

#[test]
fn workspace_executor_exec_output_is_byte_exact_for_binary() {
    // 修复前的回归：exec 的输出在 agent 侧被 from_utf8_lossy（64KB 二进制
    // 膨胀 2×、高位字节全毁成 U+FFFD）。线契约 = 有效 UTF-8 走字符串、
    // 否则走字节数组；活流事件同理（ZBRT 帧层原样搬运）。
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: r#"{"op":"exec","args":["bash","-c","printf '\\200\\201\\202\\377'"],"cwd":"."}"#
            .into(),
    };
    let events = executor.start_turn(&request).unwrap();

    // 终端结果里的 stdout = 字节数组形态（bash 的输出非 UTF-8）。
    let completed = events.iter().find(|e| e.kind == "turn.completed").unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
    let stdout = payload.get("stdout").expect("stdout key");
    let bytes: Vec<u8> = match stdout {
        serde_json::Value::Array(items) => {
            items.iter().map(|v| v.as_u64().unwrap() as u8).collect()
        }
        other => panic!("binary output must ride the array form, got: {other}"),
    };
    assert_eq!(bytes, vec![0x80u8, 0x81, 0x82, 0xff]);

    // 活流事件（ZBRT Output 帧的来源）同样保真。
    let outputs: Vec<&GuestEvent> = events
        .iter()
        .filter(|e| e.kind == "terminal.output")
        .collect();
    assert_eq!(outputs.len(), 1, "one binary stdout event");
    let terminal: rfb_runtime::session::TerminalEvent =
        serde_json::from_slice(&outputs[0].payload).unwrap();
    let data = match terminal.data_bytes {
        Some(bytes) => bytes,
        None => terminal.data.into_bytes(),
    };
    assert_eq!(data, vec![0x80u8, 0x81, 0x82, 0xff]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_accepts_structured_exec_and_rejects_shell_prompt() {
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: r#"{"op":"exec","args":["rustc","--version"],"cwd":"."}"#.into(),
    };
    let events = executor.start_turn(&request).unwrap();
    let completed = events.iter().find(|e| e.kind == "turn.completed").unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
    assert!(payload["exit_code"].is_number() || payload["exit_code"].is_null());
    assert!(payload["success"].is_boolean());
    assert!(payload["stdout"].is_string());
    assert!(payload["stderr"].is_string());
    let bad = SessionRequest {
        prompt: "echo unsafe".into(),
        ..request
    };
    assert!(executor.start_turn(&bad).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_rejects_zero_timeout() {
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&["echo", "nope"], Some(0)),
    };
    let error = executor.start_turn(&request).unwrap_err();
    assert!(error.contains("timeout_secs"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_bounds_both_output_streams() {
    let root = temp_workspace();
    let limits = RuntimeLimits {
        max_event_bytes: 4096,
        ..Default::default()
    };
    let mut executor = WorkspaceGuestExecutor::new(&root, limits).unwrap();
    let command = large_output_command();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&command, Some(10)),
    };
    let events = executor.start_turn(&request).unwrap();
    let completed = events.iter().find(|e| e.kind == "turn.completed").unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
    assert!(payload["stdout"].as_str().unwrap().len() <= 4096);
    assert!(payload["stderr"].as_str().unwrap().len() <= 4096);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_truncates_large_output_to_the_exact_cap() {
    let root = temp_workspace();
    let limits = RuntimeLimits {
        max_event_bytes: 4096,
        ..Default::default()
    };
    let mut executor = WorkspaceGuestExecutor::new(&root, limits).unwrap();
    let command = large_output_command();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&command, Some(10)),
    };
    let events = executor.start_turn(&request).unwrap();
    let completed = events.iter().find(|e| e.kind == "turn.completed").unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
    let stdout = payload["stdout"].as_str().unwrap();
    let stderr = payload["stderr"].as_str().unwrap();
    // The fixture emits far more than 4096 bytes of stdout on every platform,
    // so the bounded capture must land exactly on the cap (not merely below).
    assert_eq!(stdout.len(), 4096, "stdout must be truncated to the cap");
    assert!(stderr.len() <= 4096, "stderr must stay bounded");
    assert!(
        std::str::from_utf8(stdout.as_bytes()).is_ok(),
        "truncated stdout must remain valid UTF-8"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn workspace_executor_truncation_never_splits_a_multibyte_codepoint() {
    // The bounded capture must cut at a char boundary: when the cap lands
    // inside a multi-byte UTF-8 sequence, the result is still valid UTF-8 and
    // at most `max_event_bytes` characters long.
    let root = temp_workspace();
    let limits = RuntimeLimits {
        max_event_bytes: 101,
        ..Default::default()
    };
    let mut executor = WorkspaceGuestExecutor::new(&root, limits).unwrap();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(
            &[
                "sh",
                "-c",
                "i=0; while [ $i -lt 1000 ]; do printf 'é'; i=$((i+1)); done",
            ],
            Some(10),
        ),
    };
    let events = executor.start_turn(&request).unwrap();
    let completed = events.iter().find(|e| e.kind == "turn.completed").unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
    let stdout = payload["stdout"].as_str().unwrap();
    assert!(stdout.len() <= 101, "output must stay within the cap");
    assert!(
        std::str::from_utf8(stdout.as_bytes()).is_ok(),
        "cap must never split a codepoint"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_exec_timeout_secs_is_effective() {
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let command = slow_command();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&command, Some(1)),
    };
    let error = executor.start_turn(&request).unwrap_err();
    assert_eq!(error, "command timed out");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_allows_read_only_search_operations() {
    let root = temp_workspace();
    fs::write(root.join("match.txt"), b"needle\n").unwrap();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    for prompt in [
        r#"{"op":"ls","args":{"path":".","max_results":10}}"#,
        r#"{"op":"find","args":{"path":".","pattern":"match.txt","max_results":10}}"#,
        r#"{"op":"grep","args":{"path":"match.txt","pattern":"needle","max_results":10,"max_bytes":100}}"#,
    ] {
        let request = SessionRequest {
            session_id: "s".into(),
            request_id: "r".into(),
            prompt: prompt.into(),
        };
        let events = executor.start_turn(&request).unwrap();
        let completed = events.iter().find(|e| e.kind == "turn.completed").unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
        assert!(payload["exit_code"].is_number() || payload["exit_code"].is_null());
        assert!(payload["success"].is_boolean());
        assert!(payload["stdout"].is_string());
        assert!(payload["stderr"].is_string());
        let op = serde_json::from_str::<serde_json::Value>(prompt).unwrap()["op"]
            .as_str()
            .unwrap()
            .to_owned();
        let field = match op.as_str() {
            "ls" => "entries",
            "find" => "matches",
            "grep" => "matches",
            _ => unreachable!(),
        };
        assert!(payload[field].is_array(), "{prompt}");
    }
    let unsafe_find = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: r#"{"op":"find","args":{"path":".","pattern":"*","max_results":10},"extra":true}"#
            .into(),
    };
    assert!(executor.start_turn(&unsafe_find).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_confines_paths_and_enforces_read_write_limits() {
    let root = temp_workspace();
    fs::write(root.join("ok.txt"), b"hello").unwrap();
    let mut executor = WorkspaceGuestExecutor::new(
        &root,
        RuntimeLimits {
            max_workspace_bytes: 4,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(executor
        .read_workspace_file(&FileReadRequest {
            request_id: "r".into(),
            path: "../ok.txt".into(),
            max_bytes: 4
        })
        .is_err());
    assert!(executor
        .read_workspace_file(&FileReadRequest {
            request_id: "r".into(),
            path: "ok.txt".into(),
            max_bytes: 4
        })
        .is_err());
    assert!(executor
        .read_workspace_file(&FileReadRequest {
            request_id: "r".into(),
            path: "ok.txt".into(),
            max_bytes: 0,
        })
        .is_err());
    assert!(executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w".into(),
            path: "new.txt".into(),
            content: b"12345".to_vec(),
            append: false,
        })
        .is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn default_environment_executor_fails_closed() {
    std::env::remove_var("RFB_RUNTIME_EXECUTOR");
    let mut service = RuntimeService::from_environment();
    ready(&mut service);
    let response = service.handle(request("s", "r"));
    assert!(matches!(
        response.as_slice(),
        [RuntimeMessage::Error { .. }]
    ));
}

fn request(session_id: &str, request_id: &str) -> ControlMessage {
    common::start_turn(session_id, request_id, "prompt")
}

fn ready(service: &mut RuntimeService) {
    common::ready(service);
}

#[test]
fn injected_executor_maps_events_assigns_sequences_and_replays_terminal_request() {
    let executor = RecordingExecutor {
        events: vec![
            GuestEvent::new("turn.started", b"hello"),
            GuestEvent::new("turn.completed", b"done"),
        ],
        ..Default::default()
    };
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);
    let first = service.handle(request("s", "r"));
    assert!(
        matches!(&first[..], [RuntimeMessage::Event(a), RuntimeMessage::Event(b)] if a.sequence == 1 && b.sequence == 2 && a.session_id == "s" && b.kind == "turn.completed")
    );
    let replay = service.handle(request("s", "r"));
    assert_eq!(replay, first);
    assert!(matches!(
        service.handle(request("s", "r2"))[..],
        [RuntimeMessage::Event(_), RuntimeMessage::Event(_)]
    ));
}

#[test]
fn injected_executor_cancel_is_identity_checked_and_terminal() {
    let executor = RecordingExecutor {
        events: vec![GuestEvent::new("turn.started", Vec::new())],
        ..Default::default()
    };
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);
    service.handle(request("s", "r"));
    assert!(
        matches!(&service.handle(ControlMessage::Cancel { session_id: "s".into(), request_id: "wrong".into() })[..], [RuntimeMessage::Error { ref message, .. }] if message.contains("different active"))
    );
    let cancelled = service.handle(ControlMessage::Cancel {
        session_id: "s".into(),
        request_id: "r".into(),
    });
    assert!(
        matches!(&cancelled[..], [RuntimeMessage::Event(event)] if event.kind == "turn.cancelled")
    );
}

struct FailClosedCancelExecutor;

impl GuestExecutor for FailClosedCancelExecutor {
    fn start_turn(&mut self, _request: &SessionRequest) -> Result<Vec<GuestEvent>, String> {
        Ok(vec![GuestEvent::new("turn.started", Vec::new())])
    }

    fn cancel(&mut self, _session_id: &str, _request_id: &str) -> Result<(), String> {
        Err("running request cannot be cancelled safely".into())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn active_cancel_fails_closed_without_terminal_event_or_state_loss() {
    let mut service =
        RuntimeService::with_executor_impl(RuntimeLimits::default(), FailClosedCancelExecutor);
    ready(&mut service);
    let started = service.handle(request("s", "r"));
    assert!(
        matches!(started.as_slice(), [RuntimeMessage::Event(event)] if event.kind == "turn.started")
    );

    let cancelled = service.handle(ControlMessage::Cancel {
        session_id: "s".into(),
        request_id: "r".into(),
    });
    assert!(
        matches!(cancelled.as_slice(), [RuntimeMessage::Error { message, .. }] if message == "running request cannot be cancelled safely")
    );

    let second = service.handle(request("s", "r2"));
    assert!(
        matches!(second.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("active request: r"))
    );
}

#[test]
fn shutdown_calls_executor_and_rejects_future_controls() {
    let mut service =
        RuntimeService::with_executor_impl(RuntimeLimits::default(), RecordingExecutor::default());
    ready(&mut service);
    assert_eq!(
        service.handle(ControlMessage::Shutdown),
        vec![RuntimeMessage::ShutdownAck]
    );
    assert!(
        matches!(&service.handle(request("s", "r"))[..], [RuntimeMessage::Error { message, .. }] if message == "runtime has shut down")
    );
    assert_eq!(
        service.handle(ControlMessage::Shutdown),
        vec![RuntimeMessage::ShutdownAck]
    );
}

#[test]
fn workspace_executor_protocol_covers_start_terminal_and_filesystem() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);

    // printf is a POSIX tool; Windows has no printf.exe on PATH, so use the
    // shell there. Both produce stdout output the terminal event must carry.
    #[cfg(windows)]
    let (exec_args, expected_data) = (r#"["cmd","/c","echo","hello"]"#, "hello\r\n");
    #[cfg(not(windows))]
    let (exec_args, expected_data) = (r#"["printf","hello"]"#, "hello");

    let responses = service.handle(ControlMessage::StartTurn(SessionRequest {
        session_id: "session".into(),
        request_id: "turn".into(),
        prompt: format!(r#"{{"op":"exec","args":{exec_args},"cwd":"."}}"#),
    }));
    assert!(
        matches!(responses.first(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.started")
    );
    let terminal = responses
        .iter()
        .find_map(|response| match response {
            RuntimeMessage::Event(event) if event.kind == "terminal.output" => Some(event),
            _ => None,
        })
        .expect("terminal output event");
    let terminal: rfb_runtime::session::TerminalEvent =
        serde_json::from_slice(&terminal.payload).unwrap();
    assert_eq!(
        terminal.stream,
        rfb_runtime::session::TerminalStream::Stdout
    );
    assert_eq!(terminal.data, expected_data);
    assert!(
        matches!(responses.last(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.completed")
    );

    let write = service.handle(ControlMessage::WriteWorkspaceFile(FileWriteRequest {
        request_id: "write".into(),
        path: "nested/result.txt".into(),
        content: b"saved".to_vec(),
        append: false,
    }));
    assert!(
        matches!(write.as_slice(), [RuntimeMessage::WriteAck { path, .. }] if path == "nested/result.txt")
    );
    let read = service.handle(ControlMessage::ReadWorkspaceFile(FileReadRequest {
        request_id: "read".into(),
        path: "nested/result.txt".into(),
        max_bytes: 32,
    }));
    assert!(
        matches!(read.as_slice(), [RuntimeMessage::FileContent { content, .. }] if content == b"saved")
    );
    assert_eq!(fs::read(root.join("nested/result.txt")).unwrap(), b"saved");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn serial_workspace_cancel_fails_closed_without_cancelled_event() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);
    let started = SessionRequest {
        session_id: "session".into(),
        request_id: "turn".into(),
        prompt: exec_prompt(&["printf", "ok"], None),
    };
    // A completed turn is not cancellable; this also ensures the service does not
    // manufacture a cancellation event after the serial executor has returned.
    let result = service.handle(ControlMessage::StartTurn(started));
    assert!(
        matches!(result.last(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.completed")
    );
    let cancelled = service.handle(ControlMessage::Cancel {
        session_id: "session".into(),
        request_id: "turn".into(),
    });
    assert!(
        matches!(cancelled.as_slice(), [RuntimeMessage::Error { message, .. }] if message == "request is not active")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn in_flight_cancel_terminates_process_group_and_emits_single_cancelled() {
    use std::sync::atomic::Ordering;

    #[cfg(unix)]
    let long_cmd: [&str; 3] = ["sh", "-c", "sleep 30"];
    #[cfg(windows)]
    let long_cmd: [&str; 3] = ["cmd", "/C", "ping 127.0.0.1 -n 31 >NUL"];

    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);

    let started = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&long_cmd, Some(120)),
    };
    let mut executor = service
        .spawn_turn(&started)
        .expect("turn should be claimed");
    let cancel_flag = executor.cancel_handle().expect("workspace cancel handle");

    // Reader thread (host side): cancel the in-flight turn immediately.
    let cancel_flag = cancel_flag.clone();
    let handles = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel_flag.store(true, Ordering::SeqCst);
    });
    // Worker side: run the turn (blocks until the child is terminated).
    let start = std::time::Instant::now();
    let result = executor.start_turn(&started);
    let elapsed = start.elapsed();
    handles.join().unwrap();

    assert!(
        result
            .as_ref()
            .is_err_and(|message| message == "request cancelled"),
        "expected request cancelled, got {result:?}"
    );
    // Process-group cancellation is a Unix capability. On Windows only the
    // direct child is terminated, so the ping may survive a bounded reap;
    // the cancellation error is still delivered promptly.
    #[cfg(unix)]
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "cancel did not interrupt the child promptly: {elapsed:?}"
    );
    #[cfg(not(unix))]
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "cancel did not return promptly on Windows: {elapsed:?}"
    );

    // Restore the executor and complete the turn: exactly one terminal event,
    // kind turn.cancelled, session cleared.
    let responses = service.complete_turn("s".into(), "r".into(), result, executor);
    let cancelled_events: Vec<&RuntimeMessage> = responses
        .iter()
        .filter(|r| matches!(r, RuntimeMessage::Event(e) if e.kind == "turn.cancelled"))
        .collect();
    assert_eq!(cancelled_events.len(), 1, "exactly one cancelled terminal");
    assert!(
        responses
            .iter()
            .all(|r| !matches!(r, RuntimeMessage::Event(e) if e.kind == "turn.completed")),
        "no completed terminal after cancel"
    );
    // The service must now accept a new turn under the same session. The
    // executor returned from complete_turn has its cancel flag reset.
    let second = service.handle(ControlMessage::StartTurn(SessionRequest {
        session_id: "s".into(),
        request_id: "r2".into(),
        prompt: exec_prompt(&["printf", "ok"], None),
    }));
    assert!(
        matches!(second.first(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.started"),
        "service should accept a new turn after cancel, got {second:?}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cross_connection_cancel_stops_turn_started_on_another_connection() {
    use std::sync::{Arc, Mutex};

    #[cfg(unix)]
    let long_cmd: [&str; 3] = ["sh", "-c", "sleep 30"];
    #[cfg(windows)]
    let long_cmd: [&str; 3] = ["cmd", "/C", "ping 127.0.0.1 -n 31 >NUL"];

    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let service = Arc::new(Mutex::new(RuntimeService::with_executor_impl(
        RuntimeLimits::default(),
        executor,
    )));
    {
        let mut svc = service.lock().unwrap();
        assert!(matches!(
            svc.handle(ControlMessage::Hello {
                protocol_version: 1
            })
            .as_slice(),
            [RuntimeMessage::HelloAck { .. }]
        ));
    }

    let started = SessionRequest {
        session_id: "shared-session".into(),
        request_id: "turn-a".into(),
        prompt: exec_prompt(&long_cmd, Some(120)),
    };
    // Connection A: claim the turn and run it on a worker thread. The lock is
    // released while the worker blocks, so connection B can still Cancel.
    let executor = {
        let mut svc = service.lock().unwrap();
        svc.spawn_turn(&started).expect("claim turn on A")
    };

    let worker = std::thread::spawn(move || {
        let mut executor = executor;
        let result = executor.start_turn(&started);
        (result, executor)
    });

    // Let the child spawn, then connection B cancels the in-flight turn. The
    // shared service routes Cancel to the same flag the worker is polling.
    std::thread::sleep(std::time::Duration::from_millis(50));
    {
        let mut svc = service.lock().unwrap();
        let responses = svc.cancel_active("shared-session", "turn-a");
        assert!(
            responses.is_empty(),
            "cancel_active should return no responses, got {responses:?}"
        );
    }

    let (result, executor) = worker.join().unwrap();
    assert!(
        result.as_ref().is_err_and(|m| m == "request cancelled"),
        "cross-connection cancel did not stop the turn: {result:?}"
    );
    let rest = {
        let mut svc = service.lock().unwrap();
        svc.complete_turn("shared-session".into(), "turn-a".into(), result, executor)
    };
    let cancelled_events: Vec<&RuntimeMessage> = rest
        .iter()
        .filter(|r| matches!(r, RuntimeMessage::Event(e) if e.kind == "turn.cancelled"))
        .collect();
    assert_eq!(
        cancelled_events.len(),
        1,
        "single cross-connection cancelled"
    );
    let _ = fs::remove_dir_all(root);
}

/// Find semantics are GLOB name matching (PROTOCOL.md find contract): `*`
/// matches any sequence, other characters are literal, and a pattern without
/// a wildcard is a full-name match — a substring contains() would
/// over-match ("note" hitting note.txt) and never honor `*`.
#[test]
fn workspace_executor_find_matches_glob_names_not_substrings() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    service.handle(ControlMessage::Hello {
        protocol_version: 1,
    });

    for name in ["note.txt", "other.log", "sub/nested.txt"] {
        service.handle(ControlMessage::StartTurn(SessionRequest {
            session_id: "s".into(),
            request_id: format!("w-{name}"),
            prompt: format!(
                r#"{{"op":"write","args":{{"path":"{name}","data":list,"append":false}}}}"#
            )
            .replace(
                "list",
                &serde_json::to_string(&"x".as_bytes().to_vec()).unwrap(),
            ),
        }));
    }

    let find = |pattern: &str, service: &mut RuntimeService| -> Vec<String> {
        let responses = service.handle(ControlMessage::StartTurn(SessionRequest {
            session_id: "s".into(),
            request_id: format!("find-{pattern}"),
            prompt: format!(
                r#"{{"op":"find","args":{{"path":".","pattern":"{pattern}","max_results":256}}}}"#
            ),
        }));
        responses
            .iter()
            .find_map(|m| match m {
                RuntimeMessage::Event(e) if e.kind == "turn.completed" => {
                    let value: serde_json::Value = serde_json::from_slice(&e.payload).unwrap();
                    Some(
                        value["matches"]
                            .as_array()
                            .expect("find result matches")
                            .iter()
                            .map(|v| v.as_str().unwrap().to_owned())
                            .collect::<Vec<_>>(),
                    )
                }
                _ => None,
            })
            .expect("find turn completed")
    };

    let mut names = find("*.txt", &mut service);
    names.sort();
    assert_eq!(names, vec!["note.txt", "sub/nested.txt"]);
    // Full-name literal: no wildcard = exact name match, not a substring.
    assert!(
        find("note", &mut service).is_empty(),
        "contains() must not match"
    );
    assert_eq!(find("note.txt", &mut service), vec!["note.txt"]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_filesystem_rpc_writes_reads_and_projects() {
    let root = temp_workspace();
    let limits = RuntimeLimits {
        max_workspace_bytes: 1024,
        ..Default::default()
    };
    let mut executor = WorkspaceGuestExecutor::new(&root, limits).unwrap();

    executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w1".into(),
            path: "dir/a.txt".into(),
            content: b"hello".to_vec(),
            append: false,
        })
        .unwrap();
    assert_eq!(
        executor
            .read_workspace_file(&FileReadRequest {
                request_id: "r1".into(),
                path: "dir/a.txt".into(),
                max_bytes: 5,
            })
            .unwrap(),
        b"hello"
    );

    // Overwriting with a smaller payload stays inside the workspace budget.
    executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w2".into(),
            path: "dir/a.txt".into(),
            content: b"hi".to_vec(),
            append: false,
        })
        .unwrap();
    assert_eq!(
        executor
            .read_workspace_file(&FileReadRequest {
                request_id: "r2".into(),
                path: "dir/a.txt".into(),
                max_bytes: 2,
            })
            .unwrap(),
        b"hi"
    );

    // Parent directories are created for nested writes.
    executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w3".into(),
            path: "deep/nested/b.txt".into(),
            content: b"nested".to_vec(),
            append: false,
        })
        .unwrap();
    assert!(root.join("deep/nested/b.txt").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_filesystem_rpc_enforces_size_limits() {
    let root = temp_workspace();
    let limits = RuntimeLimits::default();
    let mut executor = WorkspaceGuestExecutor::new(&root, limits).unwrap();

    // The wire contract caps one file write at 51200 bytes (PROTOCOL.md);
    // exceeding it is refused regardless of max_event_bytes (large payloads
    // are the caller's job to shard).
    let error = executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w".into(),
            path: "big.txt".into(),
            content: vec![b'a'; 50 * 1024 + 1],
            append: false,
        })
        .unwrap_err();
    assert!(error.contains("file rpc limit"));

    // max_bytes outside [1, 51200] is rejected by policy.
    assert!(executor
        .read_workspace_file(&FileReadRequest {
            request_id: "r".into(),
            path: "x.txt".into(),
            max_bytes: 50 * 1024 + 1,
        })
        .is_err());
    assert!(executor
        .read_workspace_file(&FileReadRequest {
            request_id: "r".into(),
            path: "x.txt".into(),
            max_bytes: 0,
        })
        .is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn filesystem_rpc_requires_handshake_before_any_workspace_work() {
    let root = temp_workspace();
    fs::create_dir_all(root.join("probe")).unwrap();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);

    // Defense in depth: even if a transport skipped its own pre-handshake
    // guard, the runtime refuses filesystem work before protocol readiness.
    let error = service
        .filesystem_rpc(1, ".", br#"{"max_results": 10}"#)
        .unwrap_err();
    assert!(
        error.contains("protocol handshake required"),
        "pre-handshake filesystem rpc must fail closed, got: {error}"
    );

    assert!(matches!(
        service
            .handle(ControlMessage::Hello {
                protocol_version: 1
            })
            .as_slice(),
        [RuntimeMessage::HelloAck { .. }]
    ));
    let payload = service
        .filesystem_rpc(1, ".", br#"{"max_results": 10}"#)
        .expect("filesystem rpc after handshake");
    let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert!(
        value["entries"]
            .as_array()
            .map(|entries| entries.iter().any(|entry| entry["name"] == "probe"))
            .unwrap_or(false),
        "post-handshake ls must list the seeded directory: {value}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_rejects_invalid_and_unsupported_prompts() {
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let base = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: String::new(),
    };
    let cases = [
        ("not json", "prompt must be structured JSON"),
        (r#"{"args":["ls"]}"#, "request op is required"),
        (r#"{"op":"rm","args":[]}"#, "unsupported operation"),
        (r#"{"op":"exec"}"#, "args must be an array"),
        (r#"{"op":"exec","args":[]}"#, "non-empty string array"),
        (r#"{"op":"exec","args":[1,2]}"#, "non-empty string array"),
        (
            r#"{"op":"exec","args":["ls"],"evil":true}"#,
            "unsupported request fields",
        ),
    ];
    for (prompt, needle) in cases {
        let request = SessionRequest {
            prompt: prompt.into(),
            ..base.clone()
        };
        let error = executor.start_turn(&request).unwrap_err();
        assert!(error.contains(needle), "{prompt:?} -> {error:?}");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_pre_cancelled_turn_fails_fast_without_spawning() {
    use std::sync::atomic::Ordering;
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let flag = executor.cancel_handle();
    flag.store(true, Ordering::SeqCst);
    let mut executor = executor;
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&["sleep", "10"], Some(30)),
    };
    let error = executor.start_turn(&request).unwrap_err();
    assert_eq!(error, "request cancelled");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_cancel_without_active_request_fails_closed() {
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let error = executor.cancel("s", "r").unwrap_err();
    assert_eq!(error, "request is not active");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_search_operations_respect_max_results() {
    let root = temp_workspace();
    for i in 0..5 {
        fs::write(
            root.join(format!("file{i}.txt")),
            b"needle\nneedle\nneedle\n",
        )
        .unwrap();
    }
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();

    let completed_payload =
        |executor: &mut WorkspaceGuestExecutor, request: &SessionRequest| -> serde_json::Value {
            let events = executor.start_turn(request).unwrap();
            let completed = events
                .iter()
                .find(|e| e.kind == "turn.completed")
                .expect("completed event");
            serde_json::from_slice(&completed.payload).unwrap()
        };

    let ls = completed_payload(
        &mut executor,
        &SessionRequest {
            session_id: "s".into(),
            request_id: "ls".into(),
            prompt: r#"{"op":"ls","args":{"path":".","max_results":2}}"#.into(),
        },
    );
    assert_eq!(ls["entries"].as_array().unwrap().len(), 2);

    let find = completed_payload(
        &mut executor,
        &SessionRequest {
            session_id: "s".into(),
            request_id: "find".into(),
            prompt: r#"{"op":"find","args":{"path":".","pattern":"*.txt","max_results":2}}"#.into(),
        },
    );
    assert_eq!(find["matches"].as_array().unwrap().len(), 2);

    let grep = completed_payload(
        &mut executor,
        &SessionRequest {
            session_id: "s".into(),
            request_id: "grep".into(),
            prompt:
                r#"{"op":"grep","args":{"path":"file0.txt","pattern":"needle","max_results":2}}"#
                    .into(),
        },
    );
    assert_eq!(grep["matches"].as_array().unwrap().len(), 2);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_find_recurses_into_subdirectories() {
    let root = temp_workspace();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/target.rs"), b"").unwrap();
    fs::write(root.join("other.txt"), b"").unwrap();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: r#"{"op":"find","args":{"path":".","pattern":"target.rs","max_results":10}}"#
            .into(),
    };
    let events = executor.start_turn(&request).unwrap();
    let completed = events
        .iter()
        .find(|e| e.kind == "turn.completed")
        .expect("completed event");
    let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
    let matches = payload["matches"].as_array().unwrap();
    assert!(
        matches.iter().any(|p| p
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .contains("a/b/target.rs")),
        "nested match must be reported, got {matches:?}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_new_rejects_invalid_limits() {
    let root = temp_workspace();
    for limits in [
        RuntimeLimits {
            max_frame_bytes: 0,
            ..Default::default()
        },
        RuntimeLimits {
            max_event_bytes: 0,
            ..Default::default()
        },
        RuntimeLimits {
            channel_capacity: 0,
            ..Default::default()
        },
        RuntimeLimits {
            max_runtime_seconds: 0,
            ..Default::default()
        },
    ] {
        assert!(
            WorkspaceGuestExecutor::new(&root, limits).is_err(),
            "invalid limits must be rejected"
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn service_rejects_capabilities_before_handshake() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    let responses = service.handle(ControlMessage::Capabilities {
        session_per_vm: true,
        writable_workspace: true,
    });
    assert!(
        matches!(responses.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("handshake"))
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn service_hello_with_wrong_version_resets_the_handshake() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);

    let rejected = service.handle(ControlMessage::Hello {
        protocol_version: 2,
    });
    assert!(
        matches!(rejected.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("unsupported protocol version"))
    );

    // The handshake is not established: control messages still fail closed.
    let turn = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&["printf", "x"], None),
    };
    assert!(
        matches!(service.handle(ControlMessage::StartTurn(turn)).as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("handshake"))
    );

    // A correct Hello re-establishes the handshake.
    assert!(matches!(
        service
            .handle(ControlMessage::Hello {
                protocol_version: 1
            })
            .as_slice(),
        [RuntimeMessage::HelloAck { .. }]
    ));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn service_read_host_file_is_always_rejected() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);
    let responses = service.handle(ControlMessage::ReadHostFile(FileReadRequest {
        request_id: "host".into(),
        path: "secret.txt".into(),
        max_bytes: 16,
    }));
    assert!(
        matches!(responses.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("not supported"))
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn service_sequences_are_monotonic_across_turns() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);
    let mut last = 0u64;
    for i in 0..3 {
        let responses = service.handle(ControlMessage::StartTurn(SessionRequest {
            session_id: "s".into(),
            request_id: format!("turn-{i}"),
            prompt: exec_prompt(&["printf", "ok"], None),
        }));
        let sequences: Vec<u64> = responses
            .iter()
            .filter_map(|r| match r {
                RuntimeMessage::Event(event) => Some(event.sequence),
                _ => None,
            })
            .collect();
        assert!(!sequences.is_empty(), "turn {i} must emit events");
        for sequence in sequences {
            assert!(sequence > last, "sequence must be strictly monotonic");
            last = sequence;
        }
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn file_rpcs_are_unavailable_while_a_turn_is_in_flight() {
    use std::sync::atomic::Ordering;

    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    ready(&mut service);
    let started = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec_prompt(&["sleep", "30"], Some(120)),
    };
    let mut executor = service.spawn_turn(&started).expect("claim turn");

    // The executor is out on the worker: structured file RPCs fail closed.
    let responses = service.handle(ControlMessage::ReadWorkspaceFile(FileReadRequest {
        request_id: "f".into(),
        path: "a.txt".into(),
        max_bytes: 4,
    }));
    assert!(
        matches!(responses.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("turn in progress"))
    );

    // A second StartTurn is rejected while one is active.
    let second = service.handle(ControlMessage::StartTurn(SessionRequest {
        session_id: "s".into(),
        request_id: "r2".into(),
        prompt: exec_prompt(&["printf", "x"], None),
    }));
    assert!(
        matches!(second.as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("active request"))
    );

    // Cancel the in-flight turn (flag set before the exec loop starts, so no
    // child is spawned) and restore the executor.
    let flag = executor.cancel_handle().expect("cancel handle");
    flag.store(true, Ordering::SeqCst);
    let result = executor.start_turn(&started);
    assert!(result.as_ref().is_err_and(|m| m == "request cancelled"));
    let responses = service.complete_turn("s".into(), "r".into(), result, executor);
    assert!(
        matches!(responses.first(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.cancelled")
    );
    let _ = fs::remove_dir_all(root);
}

/// The applets the forkd agent serves in-process (echo/true/false) must behave
/// identically on ZBRT, which now shares the same definitions: same stdout,
/// same exit codes, and the same refusal to shadow a relative workspace path.
#[test]
fn workspace_executor_serves_builtin_applets_like_the_forkd_agent() {
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut terminal = |prompt: &str| {
        let request = SessionRequest {
            session_id: "s".into(),
            request_id: "r".into(),
            prompt: prompt.into(),
        };
        let events = executor.start_turn(&request).unwrap();
        let completed = events
            .iter()
            .find(|event| event.kind == "turn.completed")
            .expect("turn completes");
        serde_json::from_slice::<serde_json::Value>(&completed.payload).unwrap()
    };

    let echo = terminal(r#"{"op":"exec","args":["/bin/echo","hello","world"],"cwd":"."}"#);
    assert_eq!(echo["stdout"], "hello world\n");
    assert_eq!(echo["exit_code"], 0);
    assert_eq!(echo["success"], true);

    // `-n` matches the /bin/echo applet byte for byte, so an in-process echo
    // cannot be told apart from the spawned one by its output.
    let no_newline = terminal(r#"{"op":"exec","args":["echo","-n","x"],"cwd":"."}"#);
    assert_eq!(no_newline["stdout"], "x");

    assert_eq!(
        terminal(r#"{"op":"exec","args":["true"],"cwd":"."}"#)["exit_code"],
        0
    );
    let failed = terminal(r#"{"op":"exec","args":["false"],"cwd":"."}"#);
    assert_eq!(failed["exit_code"], 1);
    assert_eq!(failed["success"], false);

    // A relative path is a workspace file the caller addressed explicitly, so
    // it must be spawned (and fail when absent) rather than shadowed.
    let relative = terminal(r#"{"op":"exec","args":["tools/echo","x"],"cwd":"."}"#);
    assert_eq!(relative["exit_code"], -1);
    assert_eq!(relative["success"], false);

    // The builtin never uses the working directory, but an out-of-workspace
    // cwd must still fail closed on every transport.
    let escape = executor.start_turn(&SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: r#"{"op":"exec","args":["echo","x"],"cwd":"../outside"}"#.into(),
    });
    assert!(escape.is_err(), "out-of-workspace cwd must be rejected");
    let _ = fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// P1-1 / P1-3 / P1-2 regressions: structured write append/mode, find
// truncation, chunk-boundary UTF-8.
// ---------------------------------------------------------------------------

/// P1-1: the structured `write` op must honor `append` (previous content
/// survives) and fail closed on `mode`, which has no implementation over this
/// transport — a silently dropped permission-tightening request is worse than
/// an error.
#[test]
fn workspace_executor_write_honors_append_and_fails_closed_on_mode() {
    let root = temp_workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    service.handle(ControlMessage::Hello {
        protocol_version: 1,
    });

    let write = |service: &mut RuntimeService, request_id: &str, args: serde_json::Value| {
        service.handle(ControlMessage::StartTurn(SessionRequest {
            session_id: "s".into(),
            request_id: request_id.into(),
            prompt: serde_json::json!({"op": "write", "args": args}).to_string(),
        }))
    };

    write(
        &mut service,
        "w1",
        serde_json::json!({"path": "log.txt", "data": b"one".to_vec(), "append": false}),
    );
    write(
        &mut service,
        "w2",
        serde_json::json!({"path": "log.txt", "data": b"+two".to_vec(), "append": true}),
    );
    assert_eq!(
        fs::read(root.join("log.txt")).unwrap(),
        b"one+two",
        "append must preserve the previous content (P1-1)"
    );

    // mode is not implemented here: fail closed, file untouched.
    let responses = write(
        &mut service,
        "w3",
        serde_json::json!({"path": "log.txt", "data": b"x".to_vec(), "mode": 420}),
    );
    assert!(
        matches!(responses.as_slice(), [RuntimeMessage::Error { message, .. }]
            if message.contains("mode is not supported over this transport")),
        "got {responses:?}"
    );
    assert_eq!(
        fs::read(root.join("log.txt")).unwrap(),
        b"one+two",
        "the mode rejection must not touch the file"
    );
    let _ = fs::remove_dir_all(root);
}

/// P1-1: the typed control-message write path gains `append` with the same
/// semantics (truncate-write remains the `append: false` default).
#[test]
fn workspace_executor_typed_write_append_preserves_previous_content() {
    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w1".into(),
            path: "log.txt".into(),
            content: b"one".to_vec(),
            append: false,
        })
        .unwrap();
    executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w2".into(),
            path: "log.txt".into(),
            content: b"+two".to_vec(),
            append: true,
        })
        .unwrap();
    assert_eq!(
        executor
            .read_workspace_file(&FileReadRequest {
                request_id: "r".into(),
                path: "log.txt".into(),
                max_bytes: 16,
            })
            .unwrap(),
        b"one+two"
    );
    let _ = fs::remove_dir_all(root);
}

/// P1-3: find's `truncated` must carry the bounded walk's real truncation
/// flag — a max_results cap silently dropping entries is silent data loss.
#[test]
fn workspace_executor_find_reports_real_truncation() {
    let root = temp_workspace();
    for name in ["aa.txt", "ab.txt", "ac.txt"] {
        fs::write(root.join(name), b"x").unwrap();
    }
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    service.handle(ControlMessage::Hello {
        protocol_version: 1,
    });

    let find = |service: &mut RuntimeService, max_results: usize| -> serde_json::Value {
        let responses = service.handle(ControlMessage::StartTurn(SessionRequest {
            session_id: "s".into(),
            request_id: format!("find-{max_results}"),
            prompt: format!(
                r#"{{"op":"find","args":{{"path":".","pattern":"*.txt","max_results":{max_results}}}}}"#
            ),
        }));
        responses
            .iter()
            .find_map(|m| match m {
                RuntimeMessage::Event(e) if e.kind == "turn.completed" => {
                    Some(serde_json::from_slice::<serde_json::Value>(&e.payload).unwrap())
                }
                _ => None,
            })
            .expect("find turn completed")
    };

    let capped = find(&mut service, 2);
    assert_eq!(
        capped["truncated"], true,
        "2 of 3 matches must report truncation"
    );
    let full = find(&mut service, 10);
    assert_eq!(full["truncated"], false);
    assert_eq!(full["matches"].as_array().unwrap().len(), 3);
    let _ = fs::remove_dir_all(root);
}

/// P1-2: a multi-byte character straddling a 32 KiB pipe-read boundary must
/// not be replaced by U+FFFD — the live terminal.output stream concatenation
/// is byte-identical to the child's output.
#[cfg(unix)]
#[test]
fn workspace_executor_live_stream_never_splits_multibyte_across_chunks() {
    use std::sync::{Arc, Mutex};

    let root = temp_workspace();
    let mut executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let live: Arc<Mutex<Vec<GuestEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = live.clone();
    executor.attach_event_sink(Some(Arc::new(move |event| {
        sink.lock().unwrap().push(event);
    })));
    // 16383 'é' (2 bytes each) then 2000 '你' (3 bytes each): at least one
    // character straddles the 32 KiB read boundary whatever the pipe split.
    let prompt = serde_json::json!({
        "op": "exec",
        "args": ["/bin/sh", "-c",
            "i=0; while [ $i -lt 16383 ]; do printf 'é'; i=$((i+1)); done; \
             i=0; while [ $i -lt 2000 ]; do printf '你'; i=$((i+1)); done"],
        "cwd": ".",
        "timeout_secs": 30,
    });
    let request = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: prompt.to_string(),
    };
    let events = executor.start_turn(&request).unwrap();
    let completed = events.iter().find(|e| e.kind == "turn.completed").unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&completed.payload).unwrap();
    let expected = format!("{}{}", "é".repeat(16383), "你".repeat(2000));
    // The aggregated capture is raw bytes: byte-exact.
    assert_eq!(payload["stdout"].as_str().unwrap(), expected);

    let mut streamed = String::new();
    for event in live.lock().unwrap().iter() {
        if event.kind != "terminal.output" {
            continue;
        }
        let terminal: serde_json::Value =
            serde_json::from_slice(&event.payload).expect("terminal.output payload");
        streamed.push_str(terminal["data"].as_str().unwrap());
    }
    assert_eq!(
        streamed, expected,
        "live stream must not split a codepoint at a chunk boundary"
    );
    assert!(!streamed.contains('\u{FFFD}'));
    let _ = fs::remove_dir_all(root);
}

// The cancelled-replay tombstone bound mirrors the src-side CANCELLED_LIMIT
// (kept private); keep the two in sync if the constant ever moves.
const CANCELLED_LIMIT: usize = 256;

fn turn_request_owned(session_id: &str, request_id: &str) -> SessionRequest {
    SessionRequest {
        session_id: session_id.into(),
        request_id: request_id.into(),
        prompt: "prompt".into(),
    }
}

#[test]
fn cancelled_request_eviction_leaves_a_tombstone_against_reexecution() {
    let mut service =
        RuntimeService::with_executor_impl(RuntimeLimits::default(), RecordingExecutor::default());
    ready(&mut service);

    // Cancel one more distinct turn than the retention bound so the oldest
    // cancelled entry is evicted. Each cycle: claim (worker path), cancel,
    // then hand the executor back via complete_turn so the next cycle can
    // claim again.
    for i in 0..=CANCELLED_LIMIT {
        let request_id = format!("r{i}");
        let turn = turn_request_owned("s", &request_id);
        let executor = service.spawn_turn(&turn).unwrap();
        assert_eq!(
            service.cancel_active("s", &request_id),
            Vec::<RuntimeMessage>::new()
        );
        service.complete_turn(
            "s".into(),
            request_id,
            Err("request cancelled".into()),
            executor,
        );
    }

    let evicted_turn = turn_request_owned("s", "r0");
    // The redelivered StartTurn must never re-execute: every claim path
    // rejects explicitly.
    assert!(
        matches!(
            &service.replay_cached_turn(&evicted_turn).unwrap()[..],
            [RuntimeMessage::Error { message, .. }]
                if message == "request was cancelled earlier; terminal no longer available"
        ),
        "evicted cancelled turn must replay the tombstone rejection"
    );
    let evicted_error = match service.spawn_turn(&evicted_turn) {
        Ok(_) => panic!("evicted cancelled turn must not be claimable again"),
        Err(message) => message,
    };
    assert!(
        matches!(
            &evicted_error,
            RuntimeMessage::Error { message, .. }
                if message == "request was cancelled earlier; terminal no longer available"
        ),
        "evicted cancelled turn must not be claimable again"
    );
    assert!(
        matches!(
            &service.handle(ControlMessage::StartTurn(turn_request_owned("s", "r0")))[..],
            [RuntimeMessage::Error { message, .. }]
                if message == "request was cancelled earlier; terminal no longer available"
        ),
        "the serial path must reject the evicted cancelled turn too"
    );

    // The most recent cancelled entry is still retained: its cancel terminal
    // replays and a fresh claim is refused — no silent re-execution either.
    let retained_id = format!("r{CANCELLED_LIMIT}");
    let retained_turn = turn_request_owned("s", &retained_id);
    assert!(
        matches!(
            &service.replay_cached_turn(&retained_turn).unwrap()[..],
            [RuntimeMessage::Event(event)] if event.kind == "turn.cancelled"
        ),
        "retained cancelled turn must replay its cancel terminal"
    );
    let retained_error = match service.spawn_turn(&retained_turn) {
        Ok(_) => panic!("retained cancelled turn must not be claimable again"),
        Err(message) => message,
    };
    assert!(
        matches!(
            &retained_error,
            RuntimeMessage::Error { message, .. } if message == "request was cancelled"
        ),
        "retained cancelled turn must not be claimable again"
    );
    assert!(
        matches!(
            &service.handle(ControlMessage::StartTurn(turn_request_owned(
                "s",
                &retained_id
            )))[..],
            [RuntimeMessage::Event(event)] if event.kind == "turn.cancelled"
        ),
        "the serial path must replay the retained cancel terminal"
    );
}

#[test]
fn worker_path_turn_claims_fail_closed_after_shutdown() {
    let mut service =
        RuntimeService::with_executor_impl(RuntimeLimits::default(), RecordingExecutor::default());
    ready(&mut service);
    assert_eq!(
        service.handle(ControlMessage::Shutdown),
        vec![RuntimeMessage::ShutdownAck]
    );

    // guest_connection's StartTurn arm bypasses handle() for these two calls,
    // so each must enforce the shutdown guard itself.
    let turn = turn_request_owned("s", "r");
    let spawn_error = match service.spawn_turn(&turn) {
        Ok(_) => panic!("spawn_turn must reject after shutdown"),
        Err(message) => message,
    };
    assert!(
        matches!(&spawn_error, RuntimeMessage::Error { message, .. } if message == "runtime has shut down"),
        "spawn_turn must reject after shutdown, got: {spawn_error:?}"
    );
    let replay = service.replay_cached_turn(&turn).unwrap();
    assert!(
        matches!(&replay[..], [RuntimeMessage::Error { message, .. }] if message == "runtime has shut down"),
        "replay_cached_turn must reject after shutdown, got: {replay:?}"
    );
}
