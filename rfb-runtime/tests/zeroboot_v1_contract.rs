#![cfg(feature = "guest")]

//! ZeroBoot V1 full-sandbox contract coverage using the fake/serial runtime.
//! These tests intentionally exercise RFB1 only; no V2 wire is involved.

use rfb_runtime::resources::RuntimeLimits;
use rfb_runtime::runtime_service::RuntimeService;
use rfb_runtime::session::{
    ControlMessage, FileReadRequest, FileWriteRequest, RuntimeMessage, SessionRequest,
    TerminalEvent, TerminalStream,
};
use rfb_runtime::workspace_executor::WorkspaceGuestExecutor;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn workspace() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "rfb-zb-v1-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}
fn hello(s: &mut RuntimeService) {
    assert!(matches!(
        s.handle(ControlMessage::Hello {
            protocol_version: 1
        })
        .as_slice(),
        [RuntimeMessage::HelloAck {
            protocol_version: 1
        }]
    ));
    assert!(matches!(
        s.handle(ControlMessage::Capabilities {
            session_per_vm: true,
            writable_workspace: true
        })
        .as_slice(),
        [RuntimeMessage::Capabilities {
            session_per_vm: true,
            writable_workspace: true
        }]
    ));
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
    // stdin is deliberately null: cat must observe EOF; cwd is workspace-relative.
    let r = s.handle(ControlMessage::StartTurn(SessionRequest {
        session_id: "s".into(),
        request_id: "r".into(),
        prompt: exec(
            &[
                "sh",
                "-c",
                "printf '%s' \"$PWD\"; printf err >&2; cat >/dev/null",
            ],
            "sub",
        ),
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
        .any(|e| e.stream == TerminalStream::Stdout && e.data.ends_with("/sub")));
    assert!(terminals
        .iter()
        .any(|e| e.stream == TerminalStream::Stderr && e.data == "err"));
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
            content: b"x".to_vec()
        }))
        .as_slice(),
        [RuntimeMessage::Error { .. }]
    ));
    assert!(matches!(
        s.handle(ControlMessage::WriteWorkspaceFile(FileWriteRequest {
            request_id: "w2".into(),
            path: "ok".into(),
            content: b"12345".to_vec()
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
