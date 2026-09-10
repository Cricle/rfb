#![cfg(all(feature = "zeroboot", any(target_os = "linux", target_os = "android")))]

use std::time::Duration;

use rfb::vsock::{connect_firecracker_uds, round_trip};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

#[tokio::test]
async fn firecracker_handshake_and_round_trip() {
    let path = std::env::temp_dir().join(format!("zeroboot-vsock-mock-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut command = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            command.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        assert_eq!(command, b"CONNECT 5000\n");
        stream.write_all(b"OK 123\n").await.unwrap();
        for expected in [
            b"ping".as_slice(),
            &[0x00, 0xff, 0x80, 0x01, 0x7f],
            &[0xaa, 0x55, 0x00, 0xff, 0xc3, 0x28],
        ] {
            let mut payload = vec![0u8; expected.len()];
            stream.read_exact(&mut payload).await.unwrap();
            assert_eq!(payload, expected);
            let split = expected.len() / 2;
            stream.write_all(&payload[..split]).await.unwrap();
            stream.write_all(&payload[split..]).await.unwrap();
        }
    });

    let mut client = connect_firecracker_uds(&path, 5000, Duration::from_secs(1))
        .await
        .unwrap();
    for payload in [
        b"ping".as_slice(),
        &[0x00, 0xff, 0x80, 0x01, 0x7f],
        &[0xaa, 0x55, 0x00, 0xff, 0xc3, 0x28],
    ] {
        let echoed = round_trip(&mut client, payload, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(echoed, payload);
    }
    server.await.unwrap();
    let _ = std::fs::remove_file(path);
}
