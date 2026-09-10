#![cfg(feature = "forkd")]

use rfb::forkd::{ForkdClient, ForkdClientError, ForkdConfig};
use std::time::Duration;

#[test]
fn validation_rejects_bad_sandbox_ids_and_accepts_valid_ids() {
    assert!(ForkdClient::validate_sandbox_id("sandbox-01_ok").is_ok());
    for id in ["", "bad id", "bad/slash"] {
        assert!(matches!(
            ForkdClient::validate_sandbox_id(id),
            Err(ForkdClientError::Decode(message)) if message == "invalid forkd sandbox id"
        ));
    }
    let too_long = "x".repeat(129);
    assert!(matches!(
        ForkdClient::validate_sandbox_id(&too_long),
        Err(ForkdClientError::Decode(message)) if message == "invalid forkd sandbox id"
    ));
}

#[test]
fn validation_rejects_bad_guest_addresses_and_accepts_ipv4_ipv6() {
    assert!(ForkdClient::validate_guest_address("127.0.0.1:8080").is_ok());
    assert!(ForkdClient::validate_guest_address("[::1]:5000").is_ok());
    for address in ["", "127.0.0.1", "127.0.0.1:65536", "host:abc"] {
        assert!(ForkdClient::validate_guest_address(address).is_err());
    }
}

#[test]
fn new_rejects_invalid_urls_and_accepts_http_urls() {
    let config = |url: &str, token: Option<String>| ForkdConfig {
        base_url: url.to_string(),
        token,
        timeout: Duration::from_millis(1),
        ..ForkdConfig::default()
    };
    assert!(matches!(
        ForkdClient::new(config("not a url", None)),
        Err(ForkdClientError::Decode(_))
    ));
    assert!(
        matches!(ForkdClient::new(config("ftp://localhost", None)), Err(ForkdClientError::Decode(message)) if message.contains("http(s)"))
    );
    assert!(ForkdClient::new(config("http://localhost///", Some("token".into()))).is_ok());
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn mock_response(
    status: &str,
    body: &str,
    delay: Duration,
) -> Result<bool, ForkdClientError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await;
        tokio::time::sleep(delay).await;
        let _ = stream.write_all(response.as_bytes()).await;
    });
    let client = ForkdClient::new(ForkdConfig {
        base_url: format!("http://{address}"),
        token: None,
        timeout: Duration::from_millis(100),
        ..ForkdConfig::default()
    })
    .unwrap();
    let result = client.snapshot_ready("base").await;
    let _ = server.await;
    result
}

#[tokio::test]
async fn snapshot_info_prefers_info_endpoint() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let n = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..n]);
        assert!(request.starts_with("GET /v1/snapshots/base/info HTTP/1.1"));
        let body = r#"{"tag":"base","status":"ready","bootable":true}"#;
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let client = ForkdClient::new(ForkdConfig {
        base_url: format!("http://{address}"),
        timeout: Duration::from_secs(1),
        ..ForkdConfig::default()
    })
    .unwrap();
    let info = client.snapshot_info("base").await.unwrap().unwrap();
    server.await.unwrap();
    assert_eq!(info.tag, "base");
}

#[tokio::test]
async fn snapshot_info_falls_back_to_legacy_endpoint_on_not_found() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for (expected_path, status, body) in [
            ("/v1/snapshots/base/info", "404 Not Found", "{}"),
            (
                "/v1/snapshots/base",
                "200 OK",
                r#"{"tag":"base","status":"ready","bootable":true}"#,
            ),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let n = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.starts_with(&format!("GET {expected_path} HTTP/1.1")));
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let client = ForkdClient::new(ForkdConfig {
        base_url: format!("http://{address}"),
        timeout: Duration::from_secs(1),
        ..ForkdConfig::default()
    })
    .unwrap();
    let info = client.snapshot_info("base").await.unwrap().unwrap();
    server.await.unwrap();
    assert_eq!(info.tag, "base");
}

#[tokio::test]
async fn snapshot_ready_successfully_decodes_mock_response() {
    let body = r#"[{"tag":"base","status":"Ready","bootable":true}]"#;
    let ready = mock_response("200 OK", body, Duration::ZERO).await.unwrap();
    assert!(ready);
}

#[tokio::test]
async fn snapshot_ready_reports_mock_http_error_and_message() {
    let error = mock_response(
        "503 Service Unavailable",
        r#"{"error":"controller unavailable"}"#,
        Duration::ZERO,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, ForkdClientError::Http { status, message } if status == reqwest::StatusCode::SERVICE_UNAVAILABLE && message == "controller unavailable")
    );
}

#[tokio::test]
async fn snapshot_ready_reports_mock_timeout() {
    let error = mock_response("200 OK", "[]", Duration::from_millis(250))
        .await
        .unwrap_err();
    assert!(matches!(error, ForkdClientError::Transport(_)));
}

#[tokio::test]
async fn snapshot_ready_reports_mock_parse_error() {
    let error = mock_response("200 OK", "not-json", Duration::ZERO)
        .await
        .unwrap_err();
    assert!(matches!(error, ForkdClientError::Decode(_)));
}

#[test]
fn error_type_remains_matchable_for_http_errors() {
    let error = ForkdClientError::Http {
        status: reqwest::StatusCode::BAD_GATEWAY,
        message: "bad gateway".into(),
    };
    assert!(matches!(error, ForkdClientError::Http { .. }));
}
