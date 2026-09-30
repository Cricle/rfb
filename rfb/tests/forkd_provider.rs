#![cfg(feature = "forkd")]

mod common;

use common::http::mock_once::mock_http_base;
use common::ndjson::mock_once::mock_ndjson_once;
use rfb::forkd::{ForkdClient, ForkdConfig, ForkdGuestProfile};
use rfb::{
    BackendKind, Capability, ExecSpec, ProviderError, SandboxProvider, SandboxSpec, TransportKind,
};
use std::time::Duration;
use tokio::net::TcpStream;

/// `common::http::mock_once::mock_http_base` pinned to this file's
/// `&'static str` body style (the old `Box::leak` dance is gone — the body
/// is owned now).
async fn http_mock(body: &str, status: &str) -> String {
    mock_http_base(status, body.to_owned()).await
}

/// `common::ndjson::mock_once::mock_ndjson_once` with the byte-wise
/// read-until-newline this file's `guest_mock` used (same wire behavior).
async fn guest_mock(response: &'static str) -> String {
    let (addr, _task) = mock_ndjson_once(response.to_owned()).await;
    addr
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
        &format!(r#"[{{"id":"sb-1","snapshot_tag":"snap","guest_addr":"{guest}"}}]"#),
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
        &format!(r#"[{{"id":"sb-2","snapshot_tag":"snap","guest_addr":"{guest}"}}]"#),
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

#[test]
fn forkd_stream_event_key_precedence_matches_the_ndjson_contract() {
    use rfb::forkd::forkd_stream_event;
    use rfb::guest::StreamEvent;

    // UNIFIED_API.md §11: a signal-killed turn ends with `{"exit_code":null}`;
    // the KEY being present terminates the stream and maps to Exit(None) — it
    // used to be skipped here as an unrecognized line.
    assert_eq!(
        forkd_stream_event(serde_json::json!({"exit_code": null})).unwrap(),
        StreamEvent::Exit { code: None }
    );
    assert_eq!(
        forkd_stream_event(serde_json::json!({"exit_code": 3})).unwrap(),
        StreamEvent::Exit { code: Some(3) }
    );
    // Terminal key presence wins over every other key on the same line
    // (same precedence as `crate::client::ndjson::stream_event`).
    assert_eq!(
        forkd_stream_event(serde_json::json!({"exit_code": 0, "stdout": "late"})).unwrap(),
        StreamEvent::Exit { code: Some(0) }
    );
    // Output keys take precedence over the started markers.
    assert_eq!(
        forkd_stream_event(serde_json::json!({"stdout": "hi", "event": "started"})).unwrap(),
        StreamEvent::Stdout {
            data: b"hi".to_vec()
        }
    );
    assert_eq!(
        forkd_stream_event(serde_json::json!({"event": "started"})).unwrap(),
        StreamEvent::Started
    );
}
