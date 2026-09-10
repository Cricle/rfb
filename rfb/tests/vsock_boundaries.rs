#![cfg(all(feature = "zeroboot", target_os = "linux"))]

use std::time::Duration;

use rfb::protocol::{Frame, Kind, HEADER_LEN};
use rfb::vsock::{connect_firecracker_uds, execute_frame, round_trip};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

fn socket_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rfb-vsock-{label}-{}", std::process::id()))
}

#[tokio::test]
async fn firecracker_address_sends_exact_connect_request() {
    let path = socket_path("address");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut command = [0u8; 11];
        stream.read_exact(&mut command).await.unwrap();
        assert_eq!(&command, b"CONNECT 42\n");
        stream.write_all(b"OK 7\n").await.unwrap();
    });

    let mut client = connect_firecracker_uds(&path, 42, Duration::from_secs(1))
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    server.await.unwrap();
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn firecracker_connection_and_handshake_errors_are_classified() {
    let missing = socket_path("missing");
    let err = connect_firecracker_uds(&missing, 1, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ));

    let path = socket_path("bad-handshake");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut command = Vec::new();
        let mut byte = [0; 1];
        while stream.read_exact(&mut byte).await.is_ok() && byte[0] != b'\n' {
            command.push(byte[0]);
        }
        stream.write_all(b"NOPE\n").await.unwrap();
    });
    let err = connect_firecracker_uds(&path, 9, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
    server.await.unwrap();
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn round_trip_handles_empty_payload_boundary() {
    let (mut client, _server) = UnixStream::pair().unwrap();
    assert_eq!(
        round_trip(&mut client, &[], Duration::from_secs(1))
            .await
            .unwrap(),
        Vec::<u8>::new()
    );
}

#[tokio::test]
async fn execute_frame_decodes_header_and_payload_at_boundary() {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let response = Frame {
        kind: Kind::Execute,
        flags: 0,
        request_id: [3; 16],
        payload: vec![0xde, 0xad],
    };
    let mut encoded = Vec::new();
    response.encode(&mut encoded).unwrap();
    let server_task = tokio::spawn(async move {
        let mut header = vec![0; HEADER_LEN];
        server.read_exact(&mut header).await.unwrap();
        server.write_all(&encoded[..HEADER_LEN + 1]).await.unwrap();
        server.write_all(&encoded[HEADER_LEN + 1..]).await.unwrap();
    });
    let got = execute_frame(&mut client, [9; 16], vec![1], Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(got.payload, response.payload);
    server_task.await.unwrap();
}
