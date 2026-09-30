#![cfg(feature = "guest")]

//! ZeroBoot V1 full-sandbox contract coverage using the fake/serial runtime.
//! These tests intentionally exercise RFB1 only; no V2 wire is involved.

mod common;

use rfb_runtime::resources::RuntimeLimits;
use rfb_runtime::runtime_service::RuntimeService;
use rfb_runtime::session::{
    ControlMessage, FileReadRequest, FileWriteRequest, RuntimeMessage, SessionRequest,
    TerminalEvent, TerminalStream,
};
use rfb_runtime::workspace_executor::WorkspaceGuestExecutor;
use std::fs;

fn workspace() -> std::path::PathBuf {
    common::unique_temp_dir("rfb-zb-v1")
}
fn hello(s: &mut RuntimeService) {
    common::ready_strict(s);
}
fn exec(args: &[&str], cwd: &str) -> String {
    serde_json::json!({"op":"exec", "args":args, "cwd":cwd}).to_string()
}

#[test]
fn v1_negotiates_capabilities_and_rejects_wrong_version() {
    let root = workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut s = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    assert!(
        matches!(s.handle(ControlMessage::Hello { protocol_version: 2 }).as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("unsupported protocol version"))
    );
    hello(&mut s);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn v1_exec_contract_preserves_args_cwd_stdin_and_both_streams() {
    let root = workspace();
    fs::create_dir_all(root.join("sub")).unwrap();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut s = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    hello(&mut s);
    // sh/printf are POSIX tools; Windows has neither on PATH, so the stdout/
    // stderr passthrough contract runs through cmd there (args are kept free
    // of spaces so the Windows command line needs no quoting; the cwd check
    // and the stdin-EOF check stay POSIX-only).
    #[cfg(windows)]
    let prompt = exec(
        &["cmd", "/c", "echo", "hello", "&&", "echo", "err>&2"],
        "sub",
    );
    // cmd echoes the space before `&&`, so stdout is "hello \r\n".
    #[cfg(windows)]
    let (want_stdout_suffix, want_stderr) = ("hello \r\n", "err\r\n");
    #[cfg(not(windows))]
    let prompt = exec(
        &[
            "sh",
            "-c",
            "printf '%s' \"$PWD\"; printf err >&2; cat >/dev/null",
        ],
        "sub",
    );
    #[cfg(not(windows))]
    let (want_stdout_suffix, want_stderr) = ("/sub", "err");
    let r = s.handle(ControlMessage::StartTurn(SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt,
    }));
    let events: Vec<_> = r
        .iter()
        .filter_map(|m| match m {
            RuntimeMessage::Event(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(events.first().unwrap().kind, "turn.started");
    let terminals: Vec<TerminalEvent> = events
        .iter()
        .filter(|e| e.kind == "terminal.output")
        .map(|e| serde_json::from_slice(&e.payload).unwrap())
        .collect();
    assert!(terminals
        .iter()
        .any(|e| e.stream == TerminalStream::Stdout && e.data.ends_with(want_stdout_suffix)));
    assert!(terminals
        .iter()
        .any(|e| e.stream == TerminalStream::Stderr && e.data == want_stderr));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "turn.completed"
                || e.kind == "turn.failed"
                || e.kind == "turn.cancelled")
            .count(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn v1_files_and_path_limits_are_confined() {
    let root = workspace();
    let executor = WorkspaceGuestExecutor::new(
        &root,
        RuntimeLimits {
            max_event_bytes: 4,
            max_workspace_bytes: 16,
            ..Default::default()
        },
    )
    .unwrap();
    let mut s = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    hello(&mut s);
    assert!(matches!(
        s.handle(ControlMessage::WriteWorkspaceFile(FileWriteRequest {
            request_id: "w".into(),
            path: "../escape".into(),
            content: b"x".to_vec(),
            append: false,
        }))
        .as_slice(),
        [RuntimeMessage::Error { .. }]
    ));
    // The wire contract caps one file write at 51200 bytes (PROTOCOL.md);
    // max_event_bytes no longer gates file writes (large payloads are the
    // caller's job to shard).
    assert!(matches!(
        s.handle(ControlMessage::WriteWorkspaceFile(FileWriteRequest {
            request_id: "w2".into(),
            path: "ok".into(),
            content: vec![b'x'; 50 * 1024 + 1],
            append: false,
        }))
        .as_slice(),
        [RuntimeMessage::Error { .. }]
    ));
    assert!(matches!(
        s.handle(ControlMessage::ReadWorkspaceFile(FileReadRequest {
            request_id: "r".into(),
            path: "ok".into(),
            max_bytes: 0
        }))
        .as_slice(),
        [RuntimeMessage::Error { .. }]
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn v1_health_cancel_and_terminal_are_exactly_once() {
    // Health is represented by successful handshake/capability readiness in V1.
    let root = workspace();
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let mut s = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
    hello(&mut s);
    let req = SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec(&["printf", "ok"], "."),
    };
    let first = s.handle(ControlMessage::StartTurn(req.clone()));
    let replay = s.handle(ControlMessage::StartTurn(req));
    assert_eq!(first, replay);
    assert_eq!(
        first
            .iter()
            .filter(|m| matches!(m, RuntimeMessage::Event(e) if e.kind.starts_with("turn.")))
            .count(),
        2
    );
    assert!(
        matches!(s.handle(ControlMessage::Cancel { session_id: "s".into(), request_id: "r".into() }).as_slice(), [RuntimeMessage::Error { message, .. }] if message.contains("not active"))
    );
    fs::remove_dir_all(root).unwrap();
}
