#![cfg(feature = "forkd")]

use rfb::forkd::{ForkdClient, ForkdConfig, ForkdGuestProfile};
use rfb::{
    BackendKind, Capability, ExecSpec, ProviderError, SandboxProvider, SandboxSpec, TransportKind,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn http_mock(body: &'static str, status: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = socket.read(&mut request).await;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    format!("http://{addr}")
}

async fn guest_mock(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while socket.read_exact(&mut byte).await.is_ok() {
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    addr.to_string()
}

#[test]
fn profile_capabilities_are_stable() {
    assert_eq!(
        ForkdGuestProfile::Minimal.capabilities(),
        vec![Capability::Execute, Capability::Health]
    );
    assert!(ForkdGuestProfile::Default
        .capabilities()
        .contains(&Capability::WriteFile));
    assert!(ForkdGuestProfile::CustomShell
        .capabilities()
        .contains(&Capability::Eval));
    assert!(!ForkdGuestProfile::Default
        .capabilities()
        .contains(&Capability::Eval));
}

#[tokio::test]
async fn provider_connects_and_creates_sandbox_with_guest_lifecycle() {
    let guest = guest_mock(concat!(r#"{"pong":true}"#, "\n")).await;
    let base = http_mock(
        Box::leak(
            format!(r#"[{{"id":"sb-1","snapshot_tag":"snap","guest_addr":"{guest}"}}]"#)
                .into_boxed_str(),
        ),
        "200 OK",
    )
    .await;
    let provider = ForkdClient::new(ForkdConfig {
        base_url: base,
        snapshot_tag: Some("snap".into()),
        guest_timeout: Duration::from_secs(1),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(provider.backend(), BackendKind::VirtualMachine);
    assert_eq!(provider.transport(), TransportKind::Tcp);
    let sandbox = SandboxProvider::create(&provider, SandboxSpec::default())
        .await
        .unwrap();
    assert_eq!(sandbox.backend(), BackendKind::VirtualMachine);
    assert!(sandbox.health().await.unwrap().healthy);
}

#[tokio::test]
async fn provider_rejects_invalid_capability_and_missing_snapshot() {
    let base = http_mock("[]", "200 OK").await;
    let no_snapshot = ForkdClient::new(ForkdConfig {
        base_url: base.clone(),
        ..Default::default()
    })
    .unwrap();
    let err = SandboxProvider::create(
        &no_snapshot,
        SandboxSpec {
            capabilities: vec![Capability::Execute],
            ..Default::default()
        },
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(err, ProviderError::Unavailable(message) if message.contains("snapshot tag")));
    let minimal = ForkdClient::new(ForkdConfig {
        base_url: base,
        snapshot_tag: Some("snap".into()),
        guest_profile: ForkdGuestProfile::Minimal,
        guest_capabilities: ForkdGuestProfile::Minimal.capabilities(),
        ..Default::default()
    })
    .unwrap();
    let err = SandboxProvider::create(
        &minimal,
        SandboxSpec {
            capabilities: vec![Capability::Eval],
            ..Default::default()
        },
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(
        err,
        ProviderError::UnsupportedCapability(Capability::Eval)
    ));
}

#[tokio::test]
async fn sandbox_exec_validates_and_maps_guest_errors() {
    let guest = guest_mock(concat!(r#"{"exit_code":0,"out":"ok","err":""}"#, "\n")).await;
    let base = http_mock(
        Box::leak(
            format!(r#"[{{"id":"sb-2","snapshot_tag":"snap","guest_addr":"{guest}"}}]"#)
                .into_boxed_str(),
        ),
        "200 OK",
    )
    .await;
    let provider = ForkdClient::new(ForkdConfig {
        base_url: base,
        snapshot_tag: Some("snap".into()),
        ..Default::default()
    })
    .unwrap();
    let sandbox = SandboxProvider::create(&provider, SandboxSpec::default())
        .await
        .unwrap();
    let result = sandbox.exec(ExecSpec::new("printf")).await.unwrap();
    assert_eq!(result.stdout, b"ok");
    let invalid = sandbox.exec(ExecSpec::new("")).await;
    assert!(invalid.is_err());
}

#[tokio::test]
async fn malformed_controller_response_is_unavailable() {
    let base = http_mock("not-json", "200 OK").await;
    let provider = ForkdClient::new(ForkdConfig {
        base_url: base,
        snapshot_tag: Some("snap".into()),
        ..Default::default()
    })
    .unwrap();
    let err = SandboxProvider::create(&provider, SandboxSpec::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ProviderError::Unavailable(_)));
}

#[allow(dead_code)]
async fn _assert_tcp_type(_: TcpStream) {}
