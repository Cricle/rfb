//! Host-side vsock transport tests. Endpoint validation is platform-neutral;
//! the client/session round trips run over real Unix sockets / stream pairs so
//! no Firecracker or KVM is required.

#![cfg(feature = "host-vsock")]

use rfb_runtime::host_vsock::{VsockEndpoint, VsockEndpointError};

#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
fn unique_temp_dir() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "rfb-host-vsock-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn endpoint_rejects_invalid_cid() {
    assert!(VsockEndpoint::new("/tmp/sock", 1, 100).is_err());
}

#[test]
fn endpoint_rejects_all_reserved_cids() {
    for cid in [0, 1, 2] {
        let error = VsockEndpoint::new("/tmp/sock", cid, 5000).unwrap_err();
        assert_eq!(error, VsockEndpointError::InvalidCid);
    }
}

#[test]
fn endpoint_rejects_zero_port() {
    let error = VsockEndpoint::new("/tmp/sock", 52, 0).unwrap_err();
    assert_eq!(error, VsockEndpointError::InvalidPort);
}

#[test]
fn endpoint_rejects_relative_uds_path() {
    let error = VsockEndpoint::new("relative/relay.sock", 52, 5000).unwrap_err();
    assert_eq!(error, VsockEndpointError::RelativePath);
}

#[test]
fn endpoint_accepts_a_valid_endpoint() {
    // `/tmp/...` is only absolute on Unix, so build a platform-valid absolute
    // UDS path through the public API to keep endpoint validation neutral.
    let sock = std::env::temp_dir().join("relay.sock");
    let endpoint = VsockEndpoint::new(&sock, 52, 5000).unwrap();
    assert_eq!(endpoint.guest_cid, 52);
    assert_eq!(endpoint.guest_port, 5000);
    assert_eq!(endpoint.host_uds, sock);
    assert!(endpoint.identity.is_none());
}

#[test]
fn identity_capture_fails_without_an_existing_relay() {
    // On Unix this is Unavailable (missing path); on other platforms the
    // transport itself is Unsupported. Both are errors that fail closed.
    // Use temp_dir so the path is absolute on every platform and construction
    // succeeds; only the (intentionally failing) identity capture remains.
    let sock = std::env::temp_dir().join("no/such/rfb/relay.sock");
    let mut endpoint = VsockEndpoint::new(&sock, 52, 5000).unwrap();
    assert!(endpoint.capture_identity().is_err());
    assert!(endpoint.validate_current().is_err());
}

#[cfg(unix)]
#[cfg(test)]
mod unix_tests {
    use super::unique_temp_dir;
    use rfb_runtime::codec::{read_frame, write_frame, Frame, FrameCodec, MessageType};
    use rfb_runtime::host_vsock::{
        HostClient, SharedHostSession, VsockClient, VsockClientError, VsockEndpoint,
        VsockEndpointError,
    };
    use rfb_runtime::session::{
        ControlMessage, RuntimeMessage, SessionEvent, TerminalEvent, TerminalStream,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    #[tokio::test]
    async fn identity_pinning_detects_missing_and_recreated_relays() {
        let dir = unique_temp_dir();
        let sock = dir.join("relay.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let mut endpoint = VsockEndpoint::new(&sock, 52, 5000).unwrap();

        endpoint.capture_identity().unwrap();
        endpoint.validate_current().unwrap();

        // Removing the relay makes the pinned identity unavailable.
        drop(listener);
        std::fs::remove_file(&sock).unwrap();
        assert!(matches!(
            endpoint.validate_current(),
            Err(VsockEndpointError::Unavailable(_))
        ));

        // Recreating the relay at the same path invalidates the pinned identity.
        let _listener2 = UnixListener::bind(&sock).unwrap();
        assert!(endpoint.validate_current().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn identity_capture_rejects_a_non_socket_path() {
        let dir = unique_temp_dir();
        let file = dir.join("not-a-socket");
        std::fs::write(&file, b"plain file").unwrap();
        let mut endpoint = VsockEndpoint::new(&file, 52, 5000).unwrap();
        assert!(matches!(
            endpoint.capture_identity(),
            Err(VsockEndpointError::Unavailable(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn vsock_client_connect_fails_when_relay_is_missing() {
        let endpoint = VsockEndpoint::new("/no/such/rfb/relay.sock", 52, 5000).unwrap();
        let result =
            VsockClient::connect(endpoint, FrameCodec::default(), Duration::from_millis(500)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn vsock_client_connect_surfaces_relay_rejection() {
        let dir = unique_temp_dir();
        let sock = dir.join("relay.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let endpoint = VsockEndpoint::new(&sock, 52, 5000).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            assert_eq!(String::from_utf8_lossy(&request), "CONNECT 5000\n");
            stream.write_all(b"ERR relay busy\n").await.unwrap();
        });

        let error = VsockClient::connect(endpoint, FrameCodec::default(), Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(matches!(error, VsockClientError::ConnectRejected(_)));
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn vsock_client_connect_uses_one_deadline_for_fragmented_connect_response() {
        let dir = unique_temp_dir();
        let sock = dir.join("relay.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let endpoint = VsockEndpoint::new(&sock, 52, 5000).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            assert_eq!(request, b"CONNECT 5000\n");

            // Each gap is below the 1 s timeout, but the complete response is
            // not. A fresh timeout per byte would incorrectly accept this.
            stream.write_all(b"O").await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            stream.write_all(b"K").await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            let _ = stream.write_all(b"\n").await;
        });

        let result =
            VsockClient::connect(endpoint, FrameCodec::default(), Duration::from_secs(1)).await;
        assert!(matches!(result, Err(VsockClientError::Timeout)));
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn vsock_client_recv_times_out_on_a_fragmented_frame() {
        let (host_stream, mut peer_stream) = UnixStream::pair().unwrap();
        let mut client = VsockClient::from_stream_for_test(host_stream, Duration::from_millis(100));
        peer_stream.write_u32_le(32).await.unwrap();
        let result = client.recv().await;
        assert!(matches!(
            result,
            Err(VsockClientError::Io(error))
                if error.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[tokio::test]
    async fn vsock_client_connect_handshakes_over_a_real_uds() {
        let dir = unique_temp_dir();
        let sock = dir.join("relay.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let mut endpoint = VsockEndpoint::new(&sock, 52, 5000).unwrap();
        endpoint.capture_identity().unwrap();
        let codec = FrameCodec::default();
        let server_codec = codec.clone();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            assert_eq!(String::from_utf8_lossy(&request), "CONNECT 5000\n");
            stream.write_all(b"OK 5000\n").await.unwrap();

            let (frame, _): (Frame, ControlMessage) =
                read_frame(&mut stream, &server_codec).await.unwrap();
            assert_eq!(frame.message_type, MessageType::Hello);
            assert_eq!(frame.sequence, 1);
            write_frame(
                &mut stream,
                &server_codec,
                MessageType::HelloAck,
                1,
                &RuntimeMessage::HelloAck {
                    protocol_version: rfb_runtime::PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();

            let (frame, _): (Frame, ControlMessage) =
                read_frame(&mut stream, &server_codec).await.unwrap();
            assert_eq!(frame.sequence, 3);
            write_frame(
                &mut stream,
                &server_codec,
                MessageType::Shutdown,
                3,
                &RuntimeMessage::ShutdownAck,
            )
            .await
            .unwrap();
        });

        let mut client = VsockClient::connect(endpoint, codec, Duration::from_secs(5))
            .await
            .unwrap();
        client
            .send_control(
                &ControlMessage::Hello {
                    protocol_version: rfb_runtime::PROTOCOL_VERSION,
                },
                1,
            )
            .await
            .unwrap();
        let (frame, response) = client.recv().await.unwrap();
        assert_eq!(frame.message_type, MessageType::HelloAck);
        assert_eq!(frame.sequence, 1);
        assert!(matches!(
            response,
            RuntimeMessage::HelloAck {
                protocol_version: 1
            }
        ));
        client
            .send_control(&ControlMessage::Shutdown, 3)
            .await
            .unwrap();
        let (frame, response) = client.recv().await.unwrap();
        assert_eq!(frame.message_type, MessageType::Shutdown);
        assert!(matches!(response, RuntimeMessage::ShutdownAck));
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn host_client_connect_negotiates_hello_and_capabilities_then_shuts_down() {
        let dir = unique_temp_dir();
        let sock = dir.join("relay.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let endpoint = VsockEndpoint::new(&sock, 52, 5000).unwrap();
        let codec = FrameCodec::default();
        let server_codec = codec.clone();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            assert_eq!(String::from_utf8_lossy(&request), "CONNECT 5000\n");
            stream.write_all(b"OK 5000\n").await.unwrap();

            let (frame, _): (Frame, ControlMessage) =
                read_frame(&mut stream, &server_codec).await.unwrap();
            assert_eq!(frame.sequence, 1);
            assert_eq!(frame.message_type, MessageType::Hello);
            write_frame(
                &mut stream,
                &server_codec,
                MessageType::HelloAck,
                1,
                &RuntimeMessage::HelloAck {
                    protocol_version: rfb_runtime::PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();

            let (frame, _): (Frame, ControlMessage) =
                read_frame(&mut stream, &server_codec).await.unwrap();
            assert_eq!(frame.sequence, 2);
            assert_eq!(frame.message_type, MessageType::Capabilities);
            write_frame(
                &mut stream,
                &server_codec,
                MessageType::Capabilities,
                2,
                &RuntimeMessage::Capabilities {
                    session_per_vm: true,
                    writable_workspace: true,
                },
            )
            .await
            .unwrap();

            let (frame, _): (Frame, ControlMessage) =
                read_frame(&mut stream, &server_codec).await.unwrap();
            assert_eq!(frame.message_type, MessageType::Shutdown);
            write_frame(
                &mut stream,
                &server_codec,
                MessageType::Shutdown,
                frame.sequence,
                &RuntimeMessage::ShutdownAck,
            )
            .await
            .unwrap();
        });

        let client = HostClient::new(endpoint, codec, Duration::from_secs(5));
        let mut session = client.connect().await.unwrap();
        let sequence = session.next_sequence();
        assert!(sequence > 2);
        session.shutdown().await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scripted RFB1 guest that answers the shared-session operations over a
    /// stream pair: turn events, file content/ack responses, and shutdown.
    async fn run_guest_peer(mut stream: UnixStream) {
        let codec = FrameCodec::default();
        loop {
            let (frame, control): (Frame, ControlMessage) =
                read_frame(&mut stream, &codec).await.unwrap();
            match control {
                ControlMessage::StartTurn(request) => {
                    let session_id = request.session_id;
                    let seq = frame.sequence;
                    write_frame(
                        &mut stream,
                        &codec,
                        MessageType::Event,
                        seq,
                        &RuntimeMessage::Event(SessionEvent {
                            session_id: session_id.clone(),
                            sequence: seq,
                            kind: "turn.started".into(),
                            payload: Vec::new(),
                        }),
                    )
                    .await
                    .unwrap();
                    write_frame(
                        &mut stream,
                        &codec,
                        MessageType::Event,
                        seq,
                        &RuntimeMessage::Event(SessionEvent {
                            session_id: session_id.clone(),
                            sequence: seq,
                            kind: "terminal.output".into(),
                            payload: serde_json::to_vec(&TerminalEvent {
                                stream: TerminalStream::Stdout,
                                data: "hello-from-guest".into(),
                            })
                            .unwrap(),
                        }),
                    )
                    .await
                    .unwrap();
                    write_frame(
                        &mut stream,
                        &codec,
                        MessageType::Event,
                        seq,
                        &RuntimeMessage::Event(SessionEvent {
                            session_id: session_id.clone(),
                            sequence: seq,
                            kind: "turn.completed".into(),
                            payload: serde_json::to_vec(&serde_json::json!({
                                "exit_code": 0,
                                "success": true
                            }))
                            .unwrap(),
                        }),
                    )
                    .await
                    .unwrap();
                }
                ControlMessage::ReadWorkspaceFile(request) => {
                    write_frame(
                        &mut stream,
                        &codec,
                        MessageType::FileContent,
                        frame.sequence,
                        &RuntimeMessage::FileContent {
                            request_id: request.request_id,
                            path: request.path,
                            content: b"guest-file-data".to_vec(),
                        },
                    )
                    .await
                    .unwrap();
                }
                ControlMessage::WriteWorkspaceFile(request) => {
                    write_frame(
                        &mut stream,
                        &codec,
                        MessageType::WriteAck,
                        frame.sequence,
                        &RuntimeMessage::WriteAck {
                            request_id: request.request_id,
                            path: request.path,
                        },
                    )
                    .await
                    .unwrap();
                }
                ControlMessage::Shutdown => {
                    write_frame(
                        &mut stream,
                        &codec,
                        MessageType::Shutdown,
                        frame.sequence,
                        &RuntimeMessage::ShutdownAck,
                    )
                    .await
                    .unwrap();
                    break;
                }
                other => panic!("unexpected control message from host: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn shared_session_round_trips_exec_tools_and_file_ops() {
        let (host_stream, guest_stream) = UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            run_guest_peer(guest_stream).await;
        });
        let session = SharedHostSession::from_stream(host_stream);

        let result = session
            .exec(".", vec!["printf".into(), "hi".into()], 5)
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "hello-from-guest");

        let listed = session
            .execute_tool("ls", serde_json::json!({"path": ".", "max_results": 10}))
            .await
            .unwrap();
        assert_eq!(listed["stdout"], "hello-from-guest");

        let data = session
            .read_workspace_file("notes.txt", 1024)
            .await
            .unwrap();
        assert_eq!(data, b"guest-file-data".as_slice());

        session
            .write_workspace_file("out.txt", b"payload".to_vec())
            .await
            .unwrap();

        session.shutdown().await.unwrap();
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn shared_session_rejects_invalid_arguments_before_any_io() {
        let (host, _guest) = UnixStream::pair().unwrap();
        let session = SharedHostSession::from_stream(host);

        assert!(matches!(
            session.exec(".", vec![], 5).await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.exec("..", vec!["ls".into()], 5).await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.eval(".", "1+1").await,
            Err(VsockClientError::UnsupportedEval)
        ));
        assert!(matches!(
            session.execute_tool("rm", serde_json::json!({})).await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session
                .execute_tool("grep", serde_json::json!({"unknown": 1}))
                .await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.read_workspace_file("/etc/passwd", 10).await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.read_workspace_file("a.txt", 0).await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session.read_workspace_file("a.txt", 17 * 1024 * 1024).await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session
                .write_workspace_file("../escape.txt", Vec::new())
                .await,
            Err(VsockClientError::InvalidArgument(_))
        ));
        assert!(matches!(
            session
                .write_workspace_file("big.bin", vec![0u8; 16 * 1024 * 1024 + 1])
                .await,
            Err(VsockClientError::InvalidArgument(_))
        ));
    }
}
