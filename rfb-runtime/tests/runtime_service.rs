use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

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
    let path = std::env::temp_dir().join(format!(
        "rfb-runtime-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
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
            content: b"12345".to_vec()
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
    ControlMessage::StartTurn(SessionRequest {
        session_id: session_id.into(),
        request_id: request_id.into(),
        prompt: "prompt".into(),
    })
}

fn ready(service: &mut RuntimeService) {
    assert!(matches!(
        service
            .handle(ControlMessage::Hello {
                protocol_version: 1
            })
            .as_slice(),
        [RuntimeMessage::HelloAck { .. }]
    ));
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

    let responses = service.handle(ControlMessage::StartTurn(SessionRequest {
        session_id: "session".into(),
        request_id: "turn".into(),
        prompt: r#"{"op":"exec","args":["printf","hello"],"cwd":"."}"#.into(),
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
    assert_eq!(terminal.data, "hello");
    assert!(
        matches!(responses.last(), Some(RuntimeMessage::Event(event)) if event.kind == "turn.completed")
    );

    let write = service.handle(ControlMessage::WriteWorkspaceFile(FileWriteRequest {
        request_id: "write".into(),
        path: "nested/result.txt".into(),
        content: b"saved".to_vec(),
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
        })
        .unwrap();
    assert!(root.join("deep/nested/b.txt").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_executor_filesystem_rpc_enforces_size_limits() {
    let root = temp_workspace();
    let limits = RuntimeLimits {
        max_event_bytes: 8,
        ..Default::default()
    };
    let mut executor = WorkspaceGuestExecutor::new(&root, limits).unwrap();

    let error = executor
        .write_workspace_file(&FileWriteRequest {
            request_id: "w".into(),
            path: "big.txt".into(),
            content: b"123456789".to_vec(),
        })
        .unwrap_err();
    assert!(error.contains("max_event_bytes"));

    // max_bytes outside [1, max_event_bytes] is rejected by policy.
    assert!(executor
        .read_workspace_file(&FileReadRequest {
            request_id: "r".into(),
            path: "x.txt".into(),
            max_bytes: 9,
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
            prompt: r#"{"op":"find","args":{"path":".","pattern":".txt","max_results":2}}"#.into(),
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
        prompt: r#"{"op":"find","args":{"path":".","pattern":"target","max_results":10}}"#.into(),
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
