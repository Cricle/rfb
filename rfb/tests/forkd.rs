#![cfg(feature = "forkd")]

use rfb::forkd::{CreateSandboxRequest, ForkdClient, ForkdConfig, ForkdGuest, ForkdGuestProfile};
use rfb::{
    forkd_guest::{GuestFindRequest, GuestGrepRequest, GuestLsRequest},
    guest::{
        EvalRequest, FindRequest, GrepRequest, LsRequest, ReadRequest, StreamEvent, StreamSpec,
        WriteRequest,
    },
    BackendKind, Capability, ExecSpec, Sandbox, SandboxProvider, SandboxSpec, TransportKind,
};
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[test]
fn legacy_forkd_filesystem_aliases_preserve_canonical_wire_contract() {
    let ls = GuestLsRequest::default();
    assert_eq!(ls, LsRequest::default());
    assert_eq!(
        serde_json::to_value(&ls).unwrap(),
        serde_json::to_value(LsRequest::default()).unwrap()
    );

    let find = GuestFindRequest::new("src", "*.rs");
    assert_eq!(find, FindRequest::new("src", "*.rs"));
    let grep = GuestGrepRequest::new("src", "TODO");
    assert_eq!(grep, GrepRequest::new("src", "TODO"));
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

async fn http_server(response: String) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let body = response.as_bytes();
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    });
    (address, task)
}

fn client(base_url: String) -> ForkdClient {
    ForkdClient::new(ForkdConfig {
        base_url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn create_payload_and_single_defaults() {
    let mut request = CreateSandboxRequest::single("rfb");
    request.memory_limit_mib = Some(32);
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["snapshot_tag"], "rfb");
    assert_eq!(value["n"], 1);
    assert_eq!(value["memory_limit_mib"], 32);
    assert_eq!(value["live_fork"], false);
    assert_eq!(
        serde_json::to_value(CreateSandboxRequest::single("default")).unwrap()["n"],
        1
    );
}

#[test]
fn validation_rejects_invalid_values() {
    assert!(ForkdClient::new(ForkdConfig {
        base_url: "not-url".into(),
        ..Default::default()
    })
    .is_err());
    assert!(ForkdClient::validate_sandbox_id("bad/id").is_err());
    assert!(ForkdClient::validate_guest_address("bad").is_err());
}

#[test]
fn provider_metadata_and_capability() {
    let client = client("http://127.0.0.1:8889".into());
    assert_eq!(
        SandboxProvider::backend(&client),
        BackendKind::VirtualMachine
    );
    assert_eq!(SandboxProvider::transport(&client), TransportKind::Tcp);
    assert_eq!(
        SandboxProvider::capabilities(&client),
        &[
            Capability::Execute,
            Capability::Health,
            Capability::Stream,
            Capability::Ls,
            Capability::Find,
            Capability::Grep,
            Capability::ReadFile,
            Capability::WriteFile,
        ]
    );
}

#[test]
fn default_profile_does_not_advertise_eval() {
    let config = ForkdConfig::default();
    assert!(!config.guest_capabilities.contains(&Capability::Eval));
    assert_eq!(
        config.guest_capabilities,
        ForkdGuestProfile::Default.capabilities()
    );
}

#[test]
fn custom_shell_profile_explicitly_enables_eval() {
    let _guard = env_lock();
    let config = ForkdConfig::default().with_guest_profile(ForkdGuestProfile::CustomShell);
    assert!(config.guest_capabilities.contains(&Capability::Eval));
    assert_eq!(
        config.guest_capabilities,
        ForkdGuestProfile::CustomShell.capabilities()
    );

    std::env::set_var("FORKD_GUEST_PROFILE", "custom-shell");
    let from_env = ForkdConfig::from_env();
    std::env::remove_var("FORKD_GUEST_PROFILE");
    assert_eq!(
        from_env.guest_capabilities,
        ForkdGuestProfile::CustomShell.capabilities()
    );
}

#[test]
fn minimal_profile_is_explicit_and_environment_capabilities_are_not_trusted() {
    let _guard = env_lock();
    let minimal = ForkdConfig::default().with_guest_profile(ForkdGuestProfile::Minimal);
    assert_eq!(
        minimal.guest_capabilities,
        ForkdGuestProfile::Minimal.capabilities()
    );

    std::env::set_var("FORKD_GUEST_CAPABILITIES", "read,write,eval");
    std::env::set_var("FORKD_GUEST_PROFILE", "minimal");
    let from_env = ForkdConfig::from_env();
    std::env::remove_var("FORKD_GUEST_CAPABILITIES");
    std::env::remove_var("FORKD_GUEST_PROFILE");
    assert_eq!(
        from_env.guest_capabilities,
        ForkdGuestProfile::Minimal.capabilities()
    );
}

#[test]
fn guest_profile_feature_matrix_is_monotonic_and_exact() {
    // Pin the full feature matrix: every profile's exact capability set and the
    // monotonic inclusion chain (Minimal < Default < CustomShell).
    let minimal = ForkdGuestProfile::Minimal.capabilities();
    let default = ForkdGuestProfile::Default.capabilities();
    let custom = ForkdGuestProfile::CustomShell.capabilities();
    assert_eq!(minimal, [Capability::Execute, Capability::Health]);
    assert_eq!(
        default,
        [
            Capability::Execute,
            Capability::Health,
            Capability::Stream,
            Capability::Ls,
            Capability::Find,
            Capability::Grep,
            Capability::ReadFile,
            Capability::WriteFile,
        ]
    );
    assert_eq!(
        custom,
        [
            Capability::Execute,
            Capability::Health,
            Capability::Stream,
            Capability::Ls,
            Capability::Find,
            Capability::Grep,
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Eval,
        ]
    );
    for capability in minimal {
        assert!(default.contains(&capability));
    }
    for capability in default {
        assert!(custom.contains(&capability));
    }
}

#[test]
fn configured_capabilities_are_advertised_without_static_filesystem_claims() {
    let client = ForkdClient::new(ForkdConfig {
        base_url: "http://127.0.0.1:8889".into(),
        guest_capabilities: vec![Capability::Execute, Capability::Ls, Capability::ReadFile],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        SandboxProvider::capabilities(&client),
        &[Capability::Execute, Capability::Ls, Capability::ReadFile]
    );
}

#[tokio::test]
async fn provider_requires_configured_snapshot_tag() {
    let client = client("http://127.0.0.1:8889".into());
    let error = match SandboxProvider::create(&client, SandboxSpec::default()).await {
        Ok(_) => panic!(),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        rfb::ProviderError::Unavailable(message)
            if message.contains("snapshot tag is required")
    ));
}

#[tokio::test]
async fn provider_rejects_unsupported_capability() {
    let client = client("http://127.0.0.1:8889".into());
    let mut spec = SandboxSpec::default();
    spec.capabilities.push(Capability::ReadFramebuffer);
    let error = match SandboxProvider::create(&client, spec).await {
        Ok(_) => panic!(),
        Err(e) => e,
    };
    assert!(matches!(
        error,
        rfb::ProviderError::UnsupportedCapability(Capability::ReadFramebuffer)
    ));
}

#[tokio::test]
async fn exec_mapping() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let guest_address = listener.local_addr().unwrap().to_string();
    let guest = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["action"], "exec");
        assert_eq!(request["args"], json!(["printf", "hello"]));
        write.write_all(b"{\"exit_code\":7,\"out\":\"hello\",\"err\":[119,111,114,108,100],\"timed_out\":true}\n").await.unwrap();
    });
    let (base, http) = http_server(
        json!([{"id":"sandbox-1","snapshot_tag":"default","guest_addr":guest_address}]).to_string(),
    )
    .await;
    let sandbox = client(base)
        .create(&CreateSandboxRequest::single("default"))
        .await
        .unwrap();
    let mut spec = ExecSpec::new("printf");
    spec.args = vec!["hello".into()];
    let result = Sandbox::exec(&sandbox, spec).await.unwrap();
    assert_eq!(result.status, Some(7));
    assert_eq!(result.stdout, b"hello");
    assert_eq!(result.stderr, b"world");
    assert!(result.timed_out);
    guest.await.unwrap();
    http.await.unwrap();
}

#[tokio::test]
async fn stdin_is_unsupported() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (base, http) = http_server(
        json!([{"id":"sandbox-stdin","snapshot_tag":"default","guest_addr":address}]).to_string(),
    )
    .await;
    let sandbox = client(base)
        .create(&CreateSandboxRequest::single("default"))
        .await
        .unwrap();
    let mut spec = ExecSpec::new("cat");
    spec.stdin = Some(b"input".to_vec());
    let result = Sandbox::exec(&sandbox, spec).await;
    assert!(
        matches!(result, Err(rfb::SandboxError::Execution(message)) if message.contains("does not support stdin"))
    );
    drop(listener);
    http.await.unwrap();
}

#[tokio::test]
async fn typed_requests_forward_wire_fields_and_map_results() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        for _ in 0..4 {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            match request["action"].as_str().unwrap() {
                "ping" => write.write_all(b"{\"pong\":true}\n").await.unwrap(),
                "read" => {
                    assert_eq!(request["offset"], 3);
                    assert_eq!(request["max_bytes"], 5);
                    write
                        .write_all(b"{\"data\":[97,98],\"truncated\":true,\"total_bytes\":9}\n")
                        .await
                        .unwrap();
                }
                "write" => {
                    assert_eq!(request["data"], json!([120, 121]));
                    assert_eq!(request["append"], true);
                    assert_eq!(request["mode"], 420);
                    write.write_all(b"{\"bytes_written\":2}\n").await.unwrap();
                }
                "eval" => {
                    assert_eq!(request["cwd"], "/tmp");
                    write
                        .write_all(b"{\"out\":\"answer\",\"exit_code\":4,\"timed_out\":false}\n")
                        .await
                        .unwrap();
                }
                action => panic!("unexpected action: {action}"),
            }
        }
    });
    let (base, http) = http_server(
        json!([{"id":"sandbox-typed","snapshot_tag":"default","guest_addr":address}]).to_string(),
    )
    .await;
    let client = ForkdClient::new(ForkdConfig {
        base_url: base,
        guest_capabilities: vec![
            Capability::Health,
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Eval,
        ],
        guest_profile: ForkdGuestProfile::CustomShell,
        ..Default::default()
    })
    .unwrap();
    let sandbox = client
        .create(&CreateSandboxRequest::single("default"))
        .await
        .unwrap();
    let health = Sandbox::health(&sandbox).await.unwrap();
    assert!(health.healthy);
    let mut read = ReadRequest::new("file");
    read.offset = Some(3);
    read.max_bytes = Some(5);
    assert_eq!(Sandbox::read(&sandbox, read).await.unwrap().data, b"ab");
    let mut write = WriteRequest::new("file", b"xy".to_vec());
    write.append = true;
    write.mode = Some(0o644);
    assert_eq!(
        Sandbox::write(&sandbox, write).await.unwrap().bytes_written,
        2
    );
    let mut eval = EvalRequest::new("1 + 1");
    eval.cwd = Some("/tmp".into());
    let result = Sandbox::eval(&sandbox, eval).await.unwrap();
    assert_eq!(result.output, b"answer");
    assert_eq!(result.status, Some(4));
    server.await.unwrap();
    http.await.unwrap();
}

#[tokio::test]
async fn typed_filesystem_and_stream_operations_use_sandbox_trait() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        for _ in 0..4 {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            match request["action"].as_str().unwrap() {
                "ls" => {
                    write
                        .write_all(
                            br#"{"entries":[{"name":"a.txt","is_dir":false}],"truncated":false}
"#,
                        )
                        .await
                        .unwrap();
                }
                "find" => {
                    write
                        .write_all(
                            br#"{"matches":["a.txt"],"truncated":false}
"#,
                        )
                        .await
                        .unwrap();
                }
                "grep" => {
                    write.write_all(br#"{"matches":[{"path":"a.txt","line":1,"column":1,"text":"needle"}],"truncated":false}
"#).await.unwrap();
                }
                "stream" => {
                    write
                        .write_all(
                            br#"{"started":true}
{"stdout":"hello"}
{"exit_code":0}
"#,
                        )
                        .await
                        .unwrap();
                }
                action => panic!("unexpected action: {action}"),
            }
        }
    });
    let (base, http) = http_server(
        json!([{"id":"sandbox-fs","snapshot_tag":"default","guest_addr":address}]).to_string(),
    )
    .await;
    let client = ForkdClient::new(ForkdConfig {
        base_url: base,
        guest_capabilities: vec![
            Capability::Ls,
            Capability::Find,
            Capability::Grep,
            Capability::Stream,
        ],
        ..Default::default()
    })
    .unwrap();
    let sandbox = client
        .create(&CreateSandboxRequest::single("default"))
        .await
        .unwrap();

    let mut ls = LsRequest::new("workspace");
    ls.max_results = 10;
    assert_eq!(
        Sandbox::ls(&sandbox, ls).await.unwrap().entries[0].name,
        "a.txt"
    );
    let find = FindRequest::new("workspace", "*.txt");
    assert_eq!(
        Sandbox::find(&sandbox, find).await.unwrap().matches,
        vec!["a.txt"]
    );
    let grep = GrepRequest::new("workspace", "needle");
    assert_eq!(
        Sandbox::grep(&sandbox, grep).await.unwrap().matches[0].text,
        "needle"
    );

    let stream = Sandbox::stream(&sandbox, StreamSpec::new("echo"))
        .await
        .unwrap();
    let mut stream = stream;
    assert_eq!(
        stream.next_event().await.unwrap(),
        Some(StreamEvent::Started)
    );
    assert_eq!(
        stream.next_event().await.unwrap(),
        Some(StreamEvent::Stdout {
            data: b"hello".to_vec()
        })
    );
    assert_eq!(
        stream.next_event().await.unwrap(),
        Some(StreamEvent::Exit { code: Some(0) })
    );

    server.await.unwrap();
    http.await.unwrap();
}

#[tokio::test]
async fn mock_guest_ping() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["action"],
            "ping"
        );
        write.write_all(b"{\"pong\":true}\n").await.unwrap();
    });
    let result = ForkdGuest {
        address,
        timeout: Duration::from_secs(2),
    }
    .ping()
    .await
    .unwrap();
    assert_eq!(result["pong"], true);
    server.await.unwrap();
}
