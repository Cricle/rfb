#![cfg(all(feature = "zeroboot", target_os = "linux"))]

//! Mock-guest tests for the reusable ZeroBoot V1 vsock session.
//!
//! None of these boot a Firecracker VM: a fake ZBRT guest runs on an in-process
//! Unix stream (or a UnixListener that answers the Firecracker `CONNECT` relay
//! handshake) and exercises the session's Hello / Execute / Output / Exit /
//! Health / Cancel / Fs frame exchange plus the sandbox routing surface.

use rfb::guest::{
    CancelRequest, FindRequest, GrepRequest, GuestStream, LsRequest, ReadRequest, WriteRequest,
};
use rfb::protocol::{
    Error as ProtocolError, Execute, Exit, Frame, Health, Hello, HelloAck, Kind, Output,
};
use rfb::zeroboot::{Config, SessionError, ZeroBootSandbox, ZeroBootSession, ZBRT_V1_CAPABILITIES};
use rfb::{Capability, ExecSpec, Sandbox, SandboxError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

mod common;

fn socket_path(label: &str) -> std::path::PathBuf {
    // Anti-collision path from `common::fsutil` (pid + nanos + sequence);
    // the label keeps the suite's readable prefix.
    common::fsutil::socket_path(&format!("rfb-zeroboot-session-{label}"))
}

fn reply(id: [u8; 16], kind: Kind, payload: Vec<u8>) -> Frame {
    Frame {
        kind,
        flags: 0,
        request_id: id,
        payload,
    }
}

fn hello_ack(capabilities: Vec<String>) -> Vec<u8> {
    HelloAck {
        server: "mock-guest".into(),
        capabilities,
    }
    .encode()
    .unwrap()
}

fn exec_request(args: &[&str], timeout_ms: u32) -> Execute {
    Execute {
        argv: args.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        stdin: Vec::new(),
        timeout_ms,
    }
}

/// Thin delegates over the canonical wire implementation
/// (`rfb::protocol` re-exports `rfb-runtime`'s `zeroboot_protocol`); the
/// hand-rolled header reassembly this file used to carry duplicated it.
async fn read_frame(stream: &mut UnixStream) -> Frame {
    rfb::protocol::read_frame_async(stream).await.unwrap()
}

async fn write_frame(stream: &mut UnixStream, frame: &Frame) {
    rfb::protocol::write_frame_async(stream, frame)
        .await
        .unwrap();
}

/// Answer the Firecracker `CONNECT <port>\n` / `OK <host-port>\n` relay
/// handshake that `connect_firecracker_uds` performs.
async fn expect_connect(stream: &mut UnixStream, port: u32) {
    let mut command = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await.unwrap();
        command.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        assert!(command.len() < 128, "CONNECT line too long");
    }
    assert_eq!(command, format!("CONNECT {port}\n").into_bytes());
    stream.write_all(b"OK 123\n").await.unwrap();
}

/// Fallible frame read for mock guests that serve until the connection
/// closes (EOF is a normal end of service, not a failure). Maps every
/// transport/decode failure (including EOF) to `None`, exactly as before.
async fn try_read_frame(stream: &mut UnixStream) -> Option<Frame> {
    rfb::protocol::read_frame_async(stream).await.ok()
}

/// Wait until the session's reader task has reported the connection dead,
/// so a test never races the reader's exit.
async fn wait_until_dead(session: &ZeroBootSession) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while session.is_alive() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "reader never reported the dead connection"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn session_negotiates_hello_and_reuses_one_connection() {
    let path = socket_path("reuse");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let server_connections = connections.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        server_connections.fetch_add(1, Ordering::SeqCst);
        expect_connect(&mut stream, 5000).await;

        let hello = read_frame(&mut stream).await;
        assert_eq!(hello.kind, Kind::Hello);
        let hello_request = Hello::decode(&hello.payload).unwrap();
        assert_eq!(hello_request.client, "rfb-host");
        assert_eq!(
            hello_request.capabilities,
            ZBRT_V1_CAPABILITIES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        write_frame(
            &mut stream,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into(), "health".into()]),
            ),
        )
        .await;

        let exec1 = read_frame(&mut stream).await;
        assert_eq!(exec1.kind, Kind::Execute);
        write_frame(
            &mut stream,
            &reply(
                exec1.request_id,
                Kind::Output,
                Output {
                    stream: 0,
                    data: b"hello\n".to_vec(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
        write_frame(
            &mut stream,
            &reply(
                exec1.request_id,
                Kind::Exit,
                Exit {
                    code: 0,
                    signal: None,
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;

        let exec2 = read_frame(&mut stream).await;
        assert_eq!(exec2.kind, Kind::Execute);
        write_frame(
            &mut stream,
            &reply(
                exec2.request_id,
                Kind::Output,
                Output {
                    stream: 1,
                    data: b"oops\n".to_vec(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
        write_frame(
            &mut stream,
            &reply(
                exec2.request_id,
                Kind::Exit,
                Exit {
                    code: 2,
                    signal: None,
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;

        let health = read_frame(&mut stream).await;
        assert_eq!(health.kind, Kind::Health);
        write_frame(
            &mut stream,
            &reply(
                health.request_id,
                Kind::HealthAck,
                Health {
                    healthy: true,
                    message: Some("ready".into()),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = std::sync::Arc::new(
        ZeroBootSession::open(&path, 5000, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    assert!(session.supports("execute"));
    assert!(session.supports("health"));
    assert!(!session.supports("filesystem"));
    assert!(!session.supports("cancel"));

    let result = session
        .exec(exec_request(&["echo", "hello"], 2000))
        .await
        .unwrap();
    assert_eq!(result.status, Some(0));
    assert_eq!(result.stdout, b"hello\n");
    assert!(result.stderr.is_empty());

    let result = session.exec(exec_request(&["false"], 2000)).await.unwrap();
    assert_eq!(result.status, Some(2));
    assert!(result.stdout.is_empty());
    assert_eq!(result.stderr, b"oops\n");

    let health = session.health().await.unwrap();
    assert!(health.healthy);
    assert_eq!(health.message.as_deref(), Some("ready"));

    server.await.unwrap();
    // Both execs and the health probe ran over the same guest connection:
    // reusing a session must not reconnect (or cold-boot) per request.
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn exec_consumes_multiple_output_frames_until_exit() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
        let exec = read_frame(&mut server).await;
        assert_eq!(exec.kind, Kind::Execute);
        for (stream_id, data) in [
            (0u8, b"a".as_slice()),
            (1u8, b"b".as_slice()),
            (0u8, b"c".as_slice()),
        ] {
            write_frame(
                &mut server,
                &reply(
                    exec.request_id,
                    Kind::Output,
                    Output {
                        stream: stream_id,
                        data: data.to_vec(),
                    }
                    .encode()
                    .unwrap(),
                ),
            )
            .await;
        }
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Exit,
                Exit {
                    code: 3,
                    signal: None,
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = std::sync::Arc::new(
        ZeroBootSession::from_stream(client, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    let result = session
        .exec(exec_request(
            &["sh", "-c", "echo a; echo b >&2; echo c"],
            2000,
        ))
        .await
        .unwrap();
    assert_eq!(result.status, Some(3));
    assert_eq!(result.stdout, b"ac");
    assert_eq!(result.stderr, b"b");
    server_task.await.unwrap();
}

#[tokio::test]
async fn exec_accepts_legacy_result_frame() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
        let exec = read_frame(&mut server).await;
        assert_eq!(exec.kind, Kind::Execute);
        let mut payload = Vec::new();
        payload.extend_from_slice(&7i32.to_be_bytes());
        payload.extend_from_slice(&4u32.to_be_bytes());
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(b"data");
        write_frame(&mut server, &reply(exec.request_id, Kind::Result, payload)).await;
    });

    let session = std::sync::Arc::new(
        ZeroBootSession::from_stream(client, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    let result = session.exec(exec_request(&["legacy"], 2000)).await.unwrap();
    assert_eq!(result.status, Some(7));
    assert_eq!(result.stdout, b"data");
    server_task.await.unwrap();
}

#[tokio::test]
async fn exec_maps_guest_error_frame() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
        let exec = read_frame(&mut server).await;
        assert_eq!(exec.kind, Kind::Execute);
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Error,
                ProtocolError {
                    code: 1,
                    message: "spawn failed".into(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = std::sync::Arc::new(
        ZeroBootSession::from_stream(client, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    let error = session
        .exec(exec_request(&["missing"], 2000))
        .await
        .unwrap_err();
    match error {
        SessionError::Remote { code, message } => {
            assert_eq!(code, 1);
            assert_eq!(message, "spawn failed");
        }
        other => panic!("expected Remote error, got {other:?}"),
    }
    server_task.await.unwrap();
}

#[tokio::test]
async fn stream_fails_closed_when_the_demux_drops_output_frames() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["stream".into()]),
            ),
        )
        .await;
        let exec = read_frame(&mut server).await;
        assert_eq!(exec.kind, Kind::Execute);
        // Flood more Output frames than the demux channel can hold
        // (FRAME_CHANNEL_CAPACITY = 256) while the consumer below is
        // deliberately not reading: the reader drops the overflow, and the
        // terminal must surface that loss instead of ending the stream
        // cleanly with a truncated transcript and a misleading exit 0.
        for _ in 0..1000 {
            write_frame(
                &mut server,
                &reply(
                    exec.request_id,
                    Kind::Output,
                    Output {
                        stream: 0,
                        data: vec![b'a'; 16],
                    }
                    .encode()
                    .unwrap(),
                ),
            )
            .await;
        }
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Exit,
                Exit {
                    code: 0,
                    signal: None,
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = std::sync::Arc::new(
        ZeroBootSession::from_stream(client, Duration::from_secs(5))
            .await
            .unwrap(),
    );
    let mut stream = session
        .stream(exec_request(&["flood"], 30_000))
        .await
        .unwrap();
    // Give the demultiplexer time to fill its bounded channel and drop the
    // overflow before this consumer starts draining.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let dropped_error = loop {
        match stream.next_event().await {
            Ok(Some(_)) => continue,
            Ok(None) => {
                panic!("stream ended cleanly despite dropped output frames");
            }
            Err(error) => break error,
        }
    };
    match dropped_error {
        SandboxError::Execution(message) => {
            assert!(
                message.contains("dropped") && message.contains("demux overflow"),
                "error must report the lost guest output: {message}"
            );
        }
        other => panic!("expected Execution error, got {other:?}"),
    }
    server_task.await.unwrap();
}

#[tokio::test]
async fn exec_detects_protocol_violation_in_response_frames() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
        let exec = read_frame(&mut server).await;
        assert_eq!(exec.kind, Kind::Execute);
        // Reply on the right request id with a frame kind that cannot answer
        // an Execute (Hello is only valid in the negotiation phase): the
        // session must reject it as a protocol violation.
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Hello,
                Hello {
                    client: "mock".into(),
                    capabilities: Vec::new(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = std::sync::Arc::new(
        ZeroBootSession::from_stream(client, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    let error = session
        .exec(exec_request(&["echo"], 2000))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SessionError::Protocol(ref message) if message.contains("unexpected frame")
    ));
    server_task.await.unwrap();
}

#[tokio::test]
async fn health_round_trips_and_carries_guest_message() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into(), "health".into()]),
            ),
        )
        .await;
        let health = read_frame(&mut server).await;
        assert_eq!(health.kind, Kind::Health);
        write_frame(
            &mut server,
            &reply(
                health.request_id,
                Kind::HealthAck,
                Health {
                    healthy: false,
                    message: Some("degraded".into()),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    let health = session.health().await.unwrap();
    assert!(!health.healthy);
    assert_eq!(health.message.as_deref(), Some("degraded"));
    server_task.await.unwrap();
}

#[tokio::test]
async fn cancel_and_fs_route_frames_and_map_guest_errors() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec![
                    "execute".into(),
                    "health".into(),
                    "filesystem".into(),
                    "cancel".into(),
                ]),
            ),
        )
        .await;

        let cancel = read_frame(&mut server).await;
        assert_eq!(cancel.kind, Kind::Cancel);
        let cancel_request = rfb::protocol::Cancel::decode(&cancel.payload).unwrap();
        assert_eq!(cancel_request.reason.as_deref(), Some("r1"));
        assert_eq!(cancel_request.target, None);
        // The real V1 guest answers Cancel/Fs with Error: the host must surface
        // that as a fail-closed Remote error rather than an acknowledged cancel.
        write_frame(
            &mut server,
            &reply(
                cancel.request_id,
                Kind::Error,
                ProtocolError {
                    code: 95,
                    message: "cancel is not implemented; request was not stopped".into(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;

        let fs = read_frame(&mut server).await;
        assert_eq!(fs.kind, Kind::Fs);
        let fs_request = rfb::protocol::Fs::decode(&fs.payload).unwrap();
        assert_eq!(fs_request.op, rfb::zeroboot::fs_op::READ);
        assert_eq!(fs_request.path, "/etc/hostname");
        write_frame(
            &mut server,
            &reply(
                fs.request_id,
                Kind::Error,
                ProtocolError {
                    code: 95,
                    message: "filesystem RPC is not implemented".into(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(session.supports("filesystem"));
    assert!(session.supports("cancel"));
    match session.cancel(Some("r1".into())).await.unwrap_err() {
        SessionError::Remote { code, message } => {
            assert_eq!(code, 95);
            assert!(message.contains("cancel is not implemented"));
        }
        other => panic!("expected Remote error, got {other:?}"),
    }
    match session
        .fs(rfb::zeroboot::fs_op::READ, "/etc/hostname", Vec::new())
        .await
        .unwrap_err()
    {
        SessionError::Remote { code, message } => {
            assert_eq!(code, 95);
            assert!(message.contains("filesystem RPC"));
        }
        other => panic!("expected Remote error, got {other:?}"),
    }
    server_task.await.unwrap();
}

#[tokio::test]
async fn sandbox_routes_health_and_exec_but_fails_closed_for_fs_and_cancel() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into(), "health".into()]),
            ),
        )
        .await;
        let health = read_frame(&mut server).await;
        assert_eq!(health.kind, Kind::Health);
        write_frame(
            &mut server,
            &reply(
                health.request_id,
                Kind::HealthAck,
                Health {
                    healthy: true,
                    message: Some("ready".into()),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
        let exec = read_frame(&mut server).await;
        assert_eq!(exec.kind, Kind::Execute);
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Output,
                Output {
                    stream: 0,
                    data: b"ok".to_vec(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Exit,
                Exit {
                    code: 0,
                    signal: None,
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    let sandbox = ZeroBootSandbox::from_session_for_test(Config::default(), session);
    assert_eq!(
        sandbox.capabilities(),
        &[Capability::Execute, Capability::Health]
    );

    // The mock guest never advertised filesystem/cancel: every such request
    // fails closed without producing a frame on the wire.
    let error = sandbox
        .read(ReadRequest::new("/etc/hostname"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability(Capability::ReadFile)
    ));
    let error = sandbox
        .write(WriteRequest::new("/tmp/f", vec![1]))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability(Capability::WriteFile)
    ));
    let error = sandbox.ls(LsRequest::new(".")).await.unwrap_err();
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability(Capability::Ls)
    ));
    let error = sandbox
        .find(FindRequest::new(".", "*.rs"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability(Capability::Find)
    ));
    let error = sandbox
        .grep(GrepRequest::new(".", "zbrt"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability(Capability::Grep)
    ));
    let error = sandbox
        .cancel(CancelRequest::with_id("r1"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability(Capability::Cancel)
    ));
    assert!(matches!(
        sandbox.stream(rfb::guest::StreamSpec::new("echo")).await,
        Err(SandboxError::UnsupportedCapability(Capability::Stream))
    ));

    // Health and Execute are advertised and route over the reusable session.
    let health = sandbox.health().await.unwrap();
    assert!(health.healthy);
    let result = sandbox.exec(ExecSpec::new("echo")).await.unwrap();
    assert_eq!(result.status, Some(0));
    assert_eq!(result.stdout, b"ok");
    server_task.await.unwrap();
}

#[tokio::test]
async fn stream_and_cancel_form_a_closed_loop() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec![
                    "execute".into(),
                    "stream".into(),
                    "deadline".into(),
                    "health".into(),
                    "cancel".into(),
                    "filesystem".into(),
                ]),
            ),
        )
        .await;

        let exec = read_frame(&mut server).await;
        assert_eq!(exec.kind, Kind::Execute);
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Output,
                Output {
                    stream: 0,
                    data: b"tick\n".to_vec(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;

        // stop() sends a targeted Cancel: the frame reuses this stream's
        // request id and the payload encodes it as the cancel target so the
        // guest can verify the request being stopped. The mock acks it on that
        // same id (routed into the stream's channel) and then emits the single
        // terminal Exit for the request.
        let cancel = read_frame(&mut server).await;
        assert_eq!(cancel.kind, Kind::Cancel);
        assert_eq!(cancel.request_id, exec.request_id);
        let cancel_request = rfb::protocol::Cancel::decode(&cancel.payload).unwrap();
        assert_eq!(cancel_request.target, Some(exec.request_id));
        write_frame(
            &mut server,
            &reply(cancel.request_id, Kind::CancelAck, Vec::new()),
        )
        .await;
        write_frame(
            &mut server,
            &reply(
                exec.request_id,
                Kind::Exit,
                Exit {
                    code: -1,
                    signal: None,
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });

    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    let sandbox = ZeroBootSandbox::from_session_for_test(Config::default(), session);
    // The sandbox advertises exactly the negotiated set, mirroring the
    // provider surface: the guest acked `filesystem`, so the full fs group
    // (Ls/Find/Grep/ReadFile/WriteFile) joins Execute/Health/Stream/Cancel.
    assert_eq!(
        sandbox.capabilities(),
        &[
            Capability::Execute,
            Capability::Health,
            Capability::Stream,
            Capability::Cancel,
            Capability::Ls,
            Capability::Find,
            Capability::Grep,
            Capability::ReadFile,
            Capability::WriteFile,
        ]
    );

    let mut stream = sandbox
        .stream(rfb::guest::StreamSpec::new("sleep"))
        .await
        .unwrap();
    let event = stream.next_event().await.unwrap().unwrap();
    assert_eq!(
        event,
        rfb::guest::StreamEvent::Stdout {
            data: b"tick\n".to_vec()
        }
    );

    // stop() sends Cancel and drains until the single terminal Exit (which
    // stop() consumes, mirroring the forkd adapter), so no further events may
    // follow — the terminal is exactly once.
    stream.stop().await.unwrap();
    assert!(stream.next_event().await.unwrap().is_none());
    server_task.await.unwrap();
}

#[tokio::test]
async fn sandbox_fails_closed_for_stream_without_guest_support() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into(), "health".into()]),
            ),
        )
        .await;
    });

    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    let sandbox = ZeroBootSandbox::from_session_for_test(Config::default(), session);
    assert!(matches!(
        sandbox.stream(rfb::guest::StreamSpec::new("echo")).await,
        Err(SandboxError::UnsupportedCapability(Capability::Stream))
    ));
    server_task.await.unwrap();
}

#[tokio::test]
async fn sandbox_stream_rejects_pty_and_env_fail_closed() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into(), "stream".into()]),
            ),
        )
        .await;
    });

    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    let sandbox = ZeroBootSandbox::from_session_for_test(Config::default(), session);
    let mut pty = rfb::guest::StreamSpec::new("echo");
    pty.pty = Some(true);
    assert!(matches!(
        sandbox.stream(pty).await,
        Err(SandboxError::Execution(_))
    ));
    let mut env = rfb::guest::StreamSpec::new("echo");
    env.env = vec![("A".into(), "1".into())];
    assert!(matches!(
        sandbox.stream(env).await,
        Err(SandboxError::Execution(_))
    ));
    server_task.await.unwrap();
}

#[tokio::test]
async fn sandbox_fails_closed_for_exec_without_guest_support() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(hello.request_id, Kind::HelloAck, hello_ack(vec![])),
        )
        .await;
    });

    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    let sandbox = ZeroBootSandbox::from_session_for_test(Config::default(), session);
    let error = sandbox.exec(ExecSpec::new("echo")).await.unwrap_err();
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability(Capability::Execute)
    ));
    server_task.await.unwrap();
}

#[tokio::test]
async fn concurrent_execs_queue_instead_of_failing_the_turn() {
    // Regression: the ZeroBoot V1 guest runs a single turn per connection, so
    // concurrent session.exec calls used to fail with "a turn is already
    // active". The session must queue them instead; the mock guest answers
    // strictly one turn at a time and routes every reply by request_id.
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;

        for _ in 0..3 {
            let exec = read_frame(&mut server).await;
            assert_eq!(exec.kind, Kind::Execute);
            let request = Execute::decode(&exec.payload).unwrap();
            let word = request.argv[1].clone();
            write_frame(
                &mut server,
                &reply(
                    exec.request_id,
                    Kind::Output,
                    Output {
                        stream: 0,
                        data: format!("{word}\n").into_bytes(),
                    }
                    .encode()
                    .unwrap(),
                ),
            )
            .await;
            write_frame(
                &mut server,
                &reply(
                    exec.request_id,
                    Kind::Exit,
                    Exit {
                        code: 0,
                        signal: None,
                    }
                    .encode()
                    .unwrap(),
                ),
            )
            .await;
        }
    });

    let session = std::sync::Arc::new(
        ZeroBootSession::from_stream(client, Duration::from_secs(2))
            .await
            .unwrap(),
    );
    let (a, b, c) = tokio::join!(
        session.exec(exec_request(&["echo", "one"], 2000)),
        session.exec(exec_request(&["echo", "two"], 2000)),
        session.exec(exec_request(&["echo", "three"], 2000)),
    );
    assert_eq!(a.unwrap().stdout, b"one\n");
    assert_eq!(b.unwrap().stdout, b"two\n");
    assert_eq!(c.unwrap().stdout, b"three\n");
    server_task.await.unwrap();
}

#[tokio::test]
async fn sandbox_pool_runs_concurrent_execs_on_separate_connections() {
    // The pool is what turns a VM's capacity into usable concurrency, so two
    // execs must be in flight at the same time on two connections. A pool of
    // one can never reach the barrier, which is exactly the regression this
    // guards: the single-session turn lock serialized every command.
    let path = socket_path("pool");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let in_flight = Arc::new(tokio::sync::Barrier::new(2));
    let server = tokio::spawn(async move {
        let mut connections = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let in_flight = in_flight.clone();
            connections.push(tokio::spawn(async move {
                expect_connect(&mut stream, 5000).await;
                let hello = read_frame(&mut stream).await;
                assert_eq!(hello.kind, Kind::Hello);
                write_frame(
                    &mut stream,
                    &reply(
                        hello.request_id,
                        Kind::HelloAck,
                        hello_ack(vec!["execute".into()]),
                    ),
                )
                .await;

                let exec = read_frame(&mut stream).await;
                assert_eq!(exec.kind, Kind::Execute);
                // Both connections must hold a live Execute before either may
                // answer: no reply is written until the barrier trips.
                in_flight.wait().await;
                write_frame(
                    &mut stream,
                    &reply(
                        exec.request_id,
                        Kind::Exit,
                        Exit {
                            code: 0,
                            signal: None,
                        }
                        .encode()
                        .unwrap(),
                    ),
                )
                .await;
            }));
        }
        for connection in connections {
            connection.await.unwrap();
        }
    });

    let mut sessions = Vec::new();
    for _ in 0..2 {
        sessions.push(
            ZeroBootSession::open(&path, 5000, Duration::from_secs(2))
                .await
                .unwrap(),
        );
    }
    let sandbox = ZeroBootSandbox::from_sessions_for_test(Config::default(), sessions);
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            sandbox.exec(ExecSpec::new("echo")),
            sandbox.exec(ExecSpec::new("echo")),
        )
    })
    .await
    .expect("pooled execs must not serialize behind a single connection");
    assert_eq!(first.unwrap().status, Some(0));
    assert_eq!(second.unwrap().status, Some(0));
    server.await.unwrap();
}

#[tokio::test]
async fn fork_fails_closed_without_hot_mode_snapshot_dir() {
    // fork checkpoints into the hot-mode snapshot dir; a sandbox without one
    // (cold boot, or a mock session like this one) must refuse up front with
    // a message naming the missing prerequisite — never a half-forked state.
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
    });
    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    let sandbox = ZeroBootSandbox::from_session_for_test(Config::default(), session);
    let error = match sandbox.fork().await {
        Err(error) => error,
        Ok(_) => panic!("fork must fail closed without a snapshot dir"),
    };
    assert!(error.to_string().contains("RFB_ZBRT_SNAPSHOT_DIR"));
    server_task.await.unwrap();
}

#[tokio::test]
async fn reader_exit_marks_session_dead() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
        // HOLD the connection open briefly: dropping immediately would race
        // the EOF past the liveness assertion below. Returning (dropping the
        // stream) is what actually kills the session — the reader observes
        // the EOF and must report it.
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let session = ZeroBootSession::from_stream(client, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(session.is_alive());
    server_task.await.unwrap();
    wait_until_dead(&session).await;
}

#[tokio::test]
async fn pool_skips_dead_slot_and_reports_all_dead() {
    // Slot 0 dies before the first claim; every exec must be routed to the
    // surviving connection instead of the corpse. Once the survivor dies
    // too, acquire must fail closed with the all-dead error instead of
    // hanging or handing out dead sessions.
    let (client_a, server_a) = UnixStream::pair().unwrap();
    let (client_b, server_b) = UnixStream::pair().unwrap();
    let a_task = tokio::spawn(async move {
        let mut server = server_a;
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
        // Drop: connection A dies.
    });
    let b_task = tokio::spawn(async move {
        let mut server = server_b;
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["execute".into()]),
            ),
        )
        .await;
        // Serve every exec until the connection closes.
        while let Some(frame) = try_read_frame(&mut server).await {
            if frame.kind != Kind::Execute {
                continue;
            }
            write_frame(
                &mut server,
                &reply(
                    frame.request_id,
                    Kind::Exit,
                    Exit {
                        code: 0,
                        signal: None,
                    }
                    .encode()
                    .unwrap(),
                ),
            )
            .await;
        }
    });
    let session_a = ZeroBootSession::from_stream(client_a, Duration::from_secs(2))
        .await
        .unwrap();
    let session_b = ZeroBootSession::from_stream(client_b, Duration::from_secs(2))
        .await
        .unwrap();
    a_task.await.unwrap();
    wait_until_dead(&session_a).await;
    let sandbox =
        ZeroBootSandbox::from_sessions_for_test(Config::default(), vec![session_a, session_b]);
    for _ in 0..4 {
        let result = sandbox.exec(ExecSpec::new("echo")).await.unwrap();
        assert_eq!(result.status, Some(0));
    }
    // Kill the survivor: the dead slot must be condemned on claim, and once
    // every slot is dead acquire must fail closed with the all-dead error.
    b_task.abort();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match sandbox.exec(ExecSpec::new("echo")).await {
            Ok(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "pool never reported all slots dead"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => {
                let message = error.to_string();
                if message.contains("all pooled guest connections are dead") {
                    break;
                }
                // Transient: the claim raced the reader's alive-flag flip
                // (connection died under an in-flight exec). That error is
                // legitimate and retryable — keep going until the pool
                // condemns the slot and reports all-dead.
                assert!(
                    message.contains("connection closed")
                        || message.contains("transport failure")
                        || message.contains("timed out"),
                    "unexpected error: {error}"
                );
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "pool never reported all slots dead"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

#[tokio::test]
async fn health_routes_to_first_alive_session() {
    // The primary connection is dead; the health probe must route to the
    // first still-alive slot instead of permanently reporting the dead
    // primary's failure.
    let (client_a, server_a) = UnixStream::pair().unwrap();
    let (client_b, mut server_b) = UnixStream::pair().unwrap();
    let a_task = tokio::spawn(async move {
        let mut server = server_a;
        let hello = read_frame(&mut server).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["health".into()]),
            ),
        )
        .await;
        // Drop: connection A dies.
    });
    let b_task = tokio::spawn(async move {
        let hello = read_frame(&mut server_b).await;
        assert_eq!(hello.kind, Kind::Hello);
        write_frame(
            &mut server_b,
            &reply(
                hello.request_id,
                Kind::HelloAck,
                hello_ack(vec!["health".into()]),
            ),
        )
        .await;
        let health = read_frame(&mut server_b).await;
        assert_eq!(health.kind, Kind::Health);
        write_frame(
            &mut server_b,
            &reply(
                health.request_id,
                Kind::HealthAck,
                Health {
                    healthy: true,
                    message: None,
                }
                .encode()
                .unwrap(),
            ),
        )
        .await;
    });
    let session_a = ZeroBootSession::from_stream(client_a, Duration::from_secs(2))
        .await
        .unwrap();
    let session_b = ZeroBootSession::from_stream(client_b, Duration::from_secs(2))
        .await
        .unwrap();
    a_task.await.unwrap();
    wait_until_dead(&session_a).await;
    let sandbox =
        ZeroBootSandbox::from_sessions_for_test(Config::default(), vec![session_a, session_b]);
    let health = sandbox.health().await.unwrap();
    assert!(health.healthy);
    b_task.await.unwrap();
}
