#![cfg(feature = "zeroboot")]

//! ZeroBoot V1 guest connection contract over an in-memory transport: ZBRT
//! frames driven through the RuntimeService + WorkspaceGuestExecutor without
//! a real VM or vsock. These are the testable seams of the guest service.

use rfb_runtime::resources::RuntimeLimits;
use rfb_runtime::runtime_service::RuntimeService;
use rfb_runtime::workspace_executor::WorkspaceGuestExecutor;
use rfb_runtime::zeroboot_connection::serve;
use rfb_runtime::zeroboot_protocol::{
    read_frame_async, write_frame_async, Cancel, Error as ZbrtError, Frame, Fs, Hello, HelloAck,
    Kind,
};
#[cfg(unix)]
use rfb_runtime::zeroboot_protocol::{Execute, Exit, Health};
use std::fs;
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::duplex;
use tokio::sync::Mutex;

fn workspace() -> std::path::PathBuf {
    static NEXT_WORKSPACE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT_WORKSPACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rfb-zb-conn-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        sequence
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn new_frame(kind: Kind, payload: Vec<u8>) -> Frame {
    Frame {
        kind,
        flags: 0,
        request_id: [7; 16],
        payload,
    }
}

#[cfg(unix)]
fn exec_frame(argv: &[&str], timeout_ms: u32) -> Frame {
    new_frame(
        Kind::Execute,
        Execute {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            stdin: Vec::new(),
            timeout_ms,
        }
        .encode()
        .unwrap(),
    )
}

#[cfg(unix)]
fn exit_code(frame: &Frame) -> i32 {
    assert_eq!(frame.kind, Kind::Exit);
    let exit = Exit::decode(&frame.payload).expect("valid Exit payload");
    exit.code
}

async fn spawn_guest(root: std::path::PathBuf) -> tokio::io::DuplexStream {
    let executor = WorkspaceGuestExecutor::new(&root, RuntimeLimits::default()).unwrap();
    let service = Arc::new(Mutex::new(RuntimeService::with_executor_impl(
        RuntimeLimits::default(),
        executor,
    )));
    let (host, guest) = duplex(16 * 1024);
    let (guest_read, guest_write) = tokio::io::split(guest);
    tokio::spawn(async move {
        let _ = serve(guest_read, guest_write, service).await;
    });
    host
}

/// Collect every `Output` frame for the given request id, then the single
/// terminal frame. Returns (stdout bytes, stderr bytes, terminal frame).
#[cfg(unix)]
async fn collect_until_terminal(
    host: &mut tokio::io::DuplexStream,
    request_id: [u8; 16],
) -> (Vec<u8>, Vec<u8>, Frame) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let frame = read_frame_async(host).await.unwrap();
        assert_eq!(frame.request_id, request_id);
        match frame.kind {
            Kind::Output => {
                let output = rfb_runtime::zeroboot_protocol::Output::decode(&frame.payload)
                    .expect("valid Output payload");
                if output.stream == 0 {
                    stdout.extend_from_slice(&output.data);
                } else {
                    stderr.extend_from_slice(&output.data);
                }
            }
            _terminal @ (Kind::Exit | Kind::Error) => return (stdout, stderr, frame),
            other => panic!("unexpected frame kind {other:?} while awaiting terminal"),
        }
    }
}

#[tokio::test]
async fn hello_negotiates_zbrt_capabilities() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    write_frame_async(
        &mut host,
        &new_frame(
            Kind::Hello,
            Hello {
                client: "rfb-cli".into(),
                capabilities: vec!["execute".into()],
            }
            .encode()
            .unwrap(),
        ),
    )
    .await
    .unwrap();

    let frame = read_frame_async(&mut host).await.unwrap();
    assert_eq!(frame.kind, Kind::HelloAck);
    let ack = HelloAck::decode(&frame.payload).unwrap();
    assert_eq!(ack.server, "rfb-zeroboot-guest");
    for capability in [
        "execute",
        "stream",
        "deadline",
        "health",
        "cancel",
        "filesystem",
    ] {
        assert!(
            ack.capabilities.iter().any(|c| c == capability),
            "missing advertised capability {capability}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn execute_streams_output_frames_then_single_exit() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    // The host provider/CLI send Execute without a wire Hello.
    write_frame_async(
        &mut host,
        &exec_frame(
            &["sh", "-c", "printf hello; printf err >&2; printf world"],
            5000,
        ),
    )
    .await
    .unwrap();

    let (stdout, stderr, terminal) = collect_until_terminal(&mut host, [7; 16]).await;
    assert_eq!(exit_code(&terminal), 0);
    assert_eq!(stdout, b"helloworld");
    assert_eq!(stderr, b"err");

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn execute_echo_smoke_and_health() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    write_frame_async(&mut host, &exec_frame(&["echo", "ok"], 0))
        .await
        .unwrap();
    let (stdout, _, terminal) = collect_until_terminal(&mut host, [7; 16]).await;
    assert_eq!(exit_code(&terminal), 0);
    assert_eq!(stdout, b"ok\n");

    write_frame_async(
        &mut host,
        &new_frame(
            Kind::Health,
            Health {
                healthy: true,
                message: None,
            }
            .encode()
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let frame = read_frame_async(&mut host).await.unwrap();
    assert_eq!(frame.kind, Kind::HealthAck);
    let health = Health::decode(&frame.payload).unwrap();
    assert!(health.healthy);

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn malformed_execute_fails_closed_with_error_frame() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    write_frame_async(
        &mut host,
        &new_frame(Kind::Execute, b"not-a-valid-execute".to_vec()),
    )
    .await
    .unwrap();
    let frame = read_frame_async(&mut host).await.unwrap();
    assert_eq!(frame.kind, Kind::Error);
    let error = ZbrtError::decode(&frame.payload).unwrap();
    assert_eq!(error.code, 1);

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cancel_without_active_request_is_idempotent() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    write_frame_async(
        &mut host,
        &new_frame(
            Kind::Cancel,
            Cancel {
                reason: None,
                target: None,
            }
            .encode()
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let frame = read_frame_async(&mut host).await.unwrap();
    // Sandbox cancel semantics are idempotent: cancelling with nothing active
    // acknowledges successfully instead of failing closed.
    assert_eq!(frame.kind, Kind::CancelAck);

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn duplicate_execute_request_id_fails_closed() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    write_frame_async(&mut host, &exec_frame(&["echo", "one"], 5000))
        .await
        .unwrap();
    let (_, _, terminal) = collect_until_terminal(&mut host, [7; 16]).await;
    assert_eq!(exit_code(&terminal), 0);

    // Reusing the same 128-bit request id is a wire violation: reject it.
    write_frame_async(&mut host, &exec_frame(&["echo", "two"], 5000))
        .await
        .unwrap();
    let frame = read_frame_async(&mut host).await.unwrap();
    assert_eq!(frame.kind, Kind::Error);
    let error = ZbrtError::decode(&frame.payload).unwrap();
    assert!(error.message.contains("duplicate request id"));

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_terminates_process_group_with_single_terminal() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    // A long-running command that would take ~5s if allowed to finish.
    write_frame_async(&mut host, &exec_frame(&["sleep", "5"], 30000))
        .await
        .unwrap();

    // Cancel immediately; the guest must acknowledge and then emit exactly one
    // terminal frame (the process group is killed by the workspace executor).
    let started = std::time::Instant::now();
    write_frame_async(
        &mut host,
        &new_frame(
            Kind::Cancel,
            Cancel {
                reason: None,
                target: None,
            }
            .encode()
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let ack = read_frame_async(&mut host).await.unwrap();
    assert_eq!(ack.kind, Kind::CancelAck);
    assert_eq!(ack.request_id, [7; 16]);

    let terminal = read_frame_async(&mut host).await.unwrap();
    assert_eq!(terminal.request_id, [7; 16]);
    assert_eq!(
        terminal.kind,
        Kind::Exit,
        "cancelled turn must end with Exit"
    );
    assert_eq!(exit_code(&terminal), -1);

    // The turn was cancelled, not allowed to run to completion.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancelled turn was not terminated promptly"
    );

    // Exactly-once terminal: no further frames follow the Exit.
    let _frame = tokio::time::timeout(Duration::from_millis(300), read_frame_async(&mut host))
        .await
        .expect_err("expected no frame after the terminal Exit");

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_turn_output_does_not_leak_into_next_request() {
    let root = workspace();
    let mut host = spawn_guest(root.clone()).await;

    // Turn A is cancelled mid-flight while its stdout pipe is still draining;
    // any straggler Output chunks must not be forwarded under the next
    // request's id (exactly-once terminal + request isolation).
    write_frame_async(
        &mut host,
        &new_frame(
            Kind::Execute,
            Execute {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo a1; sleep 0.3; echo a2".into(),
                ],
                cwd: None,
                stdin: Vec::new(),
                timeout_ms: 30000,
            }
            .encode()
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    write_frame_async(
        &mut host,
        &new_frame(
            Kind::Cancel,
            Cancel {
                reason: None,
                target: None,
            }
            .encode()
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let ack = read_frame_async(&mut host).await.unwrap();
    assert_eq!(ack.kind, Kind::CancelAck);
    let terminal = read_frame_async(&mut host).await.unwrap();
    assert_eq!(terminal.kind, Kind::Exit);
    assert_eq!(exit_code(&terminal), -1);

    // Turn B on the same connection: only its own output may appear.
    let b_id = [8; 16];
    write_frame_async(
        &mut host,
        &Frame {
            kind: Kind::Execute,
            flags: 0,
            request_id: b_id,
            payload: Execute {
                argv: vec!["echo".into(), "b1".into()],
                cwd: None,
                stdin: Vec::new(),
                timeout_ms: 5000,
            }
            .encode()
            .unwrap(),
        },
    )
    .await
    .unwrap();
    let (stdout, stderr, terminal) = collect_until_terminal(&mut host, b_id).await;
    assert_eq!(exit_code(&terminal), 0);
    assert_eq!(
        stdout, b"b1\n",
        "cancelled turn output leaked into next request"
    );
    assert!(stderr.is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn fs_rpc_runs_structured_workspace_operation() {
    let root = workspace();
    fs::create_dir_all(root.join("sub")).unwrap();
    let mut host = spawn_guest(root.clone()).await;

    write_frame_async(
        &mut host,
        &new_frame(
            Kind::Fs,
            Fs {
                op: 1, // ls
                path: ".".into(),
                data: serde_json::json!({"max_results": 10})
                    .to_string()
                    .into_bytes(),
            }
            .encode()
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let frame = read_frame_async(&mut host).await.unwrap();
    assert_eq!(frame.kind, Kind::FsResult);
    let value: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
    assert!(
        value["entries"]
            .as_array()
            .map(|entries| entries
                .iter()
                .any(|e| e["name"].as_str() == Some("sub") && e["is_dir"].as_bool() == Some(true)))
            .unwrap_or(false),
        "workspace ls must list the seeded sub directory as a DirEntry"
    );

    fs::remove_dir_all(root).unwrap();
}
