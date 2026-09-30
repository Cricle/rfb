#![cfg(feature = "forkd")]

mod common;

use common::http::mock_once::{captured_json, mock_http, CapturedRequest};
use common::ndjson::mock_once::{mock_ndjson_lines, mock_ndjson_once};
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

/// One-shot HTTP mock pinned to the shape this file's tests expect:
/// `200 OK` + the response as the JSON body (`common::http::mock_once`).
async fn http_server(response: String) -> (String, tokio::task::JoinHandle<()>) {
    let (addr, task) = mock_http("200 OK", response, None, None).await;
    (format!("http://{addr}"), task)
}

/// The repeated setup template: one NDJSON guest mock answering
/// `guest_response`, plus a controller mock whose sandbox list serves `id`
/// pointed at the guest address. Returns the controller base URL, the guest
/// request task, and the controller task.
async fn guest_controller_fixture(
    id: &str,
    guest_response: String,
) -> (
    String,
    tokio::task::JoinHandle<Value>,
    tokio::task::JoinHandle<()>,
) {
    let (guest_address, guest) = mock_ndjson_once(guest_response).await;
    let (base, http) = http_server(
        json!([{"id": id, "snapshot_tag": "default", "guest_addr": guest_address}]).to_string(),
    )
    .await;
    (base, guest, http)
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
    spec.capabilities.push(Capability::Eval);
    let error = match SandboxProvider::create(&client, spec).await {
        Ok(_) => panic!(),
        Err(e) => e,
    };
    assert!(matches!(
        error,
        rfb::ProviderError::UnsupportedCapability(Capability::Eval)
    ));
}

#[tokio::test]
async fn exec_mapping() {
    let (base, guest, http) = guest_controller_fixture(
        "sandbox-1",
        "{\"exit_code\":7,\"out\":\"hello\",\"err\":[119,111,114,108,100],\"timed_out\":true}\n"
            .to_owned(),
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
    let request = guest.await.unwrap();
    assert_eq!(request["action"], "exec");
    assert_eq!(request["args"], json!(["printf", "hello"]));
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
    let (address, server) = mock_ndjson_lines(vec![
        "{\"pong\":true}\n".to_owned(),
        "{\"data\":[97,98],\"truncated\":true,\"total_bytes\":9}\n".to_owned(),
        "{\"bytes_written\":2}\n".to_owned(),
        "{\"out\":\"answer\",\"exit_code\":4,\"timed_out\":false}\n".to_owned(),
    ])
    .await;
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
    // The captured requests must carry the same wire fields the inline
    // servers used to assert per action.
    let requests = server.await.unwrap();
    assert_eq!(requests[0]["action"], "ping");
    assert_eq!(requests[1]["action"], "read");
    assert_eq!(requests[1]["offset"], 3);
    assert_eq!(requests[1]["max_bytes"], 5);
    assert_eq!(requests[2]["action"], "write");
    assert_eq!(requests[2]["data"], json!([120, 121]));
    assert_eq!(requests[2]["append"], true);
    assert_eq!(requests[2]["mode"], 420);
    assert_eq!(requests[3]["action"], "eval");
    assert_eq!(requests[3]["cwd"], "/tmp");
    http.await.unwrap();
}

#[tokio::test]
async fn typed_filesystem_and_stream_operations_use_sandbox_trait() {
    let (address, server) = mock_ndjson_lines(vec![
        "{\"entries\":[{\"name\":\"a.txt\",\"is_dir\":false}],\"truncated\":false}\n".to_owned(),
        "{\"matches\":[\"a.txt\"],\"truncated\":false}\n".to_owned(),
        "{\"matches\":[{\"path\":\"a.txt\",\"line\":1,\"column\":1,\"text\":\"needle\"}],\"truncated\":false}\n"
            .to_owned(),
        "{\"started\":true}\n{\"stdout\":\"hello\"}\n{\"exit_code\":0}\n".to_owned(),
    ]).await;
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

    // The four connections must arrive in the asserted action order.
    let requests = server.await.unwrap();
    let actions: Vec<&str> = requests
        .iter()
        .map(|request| request["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["ls", "find", "grep", "stream"]);
    http.await.unwrap();
}

#[tokio::test]
async fn mock_guest_ping() {
    let (address, server) = mock_ndjson_once("{\"pong\":true}\n".to_owned()).await;
    let result = ForkdGuest {
        address,
        timeout: Duration::from_secs(2),
    }
    .ping()
    .await
    .unwrap();
    assert_eq!(result["pong"], true);
    let request = server.await.unwrap();
    assert_eq!(request["action"], "ping");
}

// ---------------------------------------------------------------------------
// P1-7 / P1-6 / P2 regressions
// ---------------------------------------------------------------------------

/// P1-7: the forkd trait create must fail closed on spec fields the backend
/// cannot honor — same semantics as cluster.rs and the zeroboot provider.
#[tokio::test]
async fn provider_create_rejects_unmapped_resources_fail_closed() {
    let client = client("http://127.0.0.1:8889".into());
    let mut spec = SandboxSpec::default();
    spec.resources.cpus = Some(2);
    assert!(matches!(
        SandboxProvider::create(&client, spec).await,
        Err(rfb::ProviderError::UnsupportedResource("cpus"))
    ));
    let mut spec = SandboxSpec::default();
    spec.resources.disk_bytes = Some(1024);
    assert!(matches!(
        SandboxProvider::create(&client, spec).await,
        Err(rfb::ProviderError::UnsupportedResource("disk_bytes"))
    ));
    let mut spec = SandboxSpec::default();
    spec.resources.pids = Some(64);
    assert!(matches!(
        SandboxProvider::create(&client, spec).await,
        Err(rfb::ProviderError::UnsupportedResource("pids"))
    ));
    let spec = SandboxSpec {
        image: Some(rfb::ImageManifest::new("registry.example/img")),
        ..Default::default()
    };
    assert!(matches!(
        SandboxProvider::create(&client, spec).await,
        Err(rfb::ProviderError::UnsupportedResource("image"))
    ));
}

/// P1-7: `resources.memory_bytes` maps onto the controller's per-sandbox
/// `memory_limit_mib` (ceil to MiB), like cluster.rs.
#[tokio::test]
async fn provider_create_injects_memory_limit_mib() {
    let (captured_tx, captured_rx) = std::sync::mpsc::channel::<CapturedRequest>();
    let (address, server) = mock_http(
        "200 OK",
        json!([{"id":"sandbox-mem","snapshot_tag":"default","guest_addr":"127.0.0.1:1"}])
            .to_string(),
        None,
        Some(captured_tx),
    )
    .await;
    let client = ForkdClient::new(ForkdConfig {
        base_url: format!("http://{address}"),
        snapshot_tag: Some("default".into()),
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap();
    let mut spec = SandboxSpec::default();
    spec.resources.memory_bytes = Some(64 << 20);
    SandboxProvider::create(&client, spec).await.unwrap();
    server.await.unwrap();
    let request = captured_json(&captured_rx.recv().unwrap());
    assert_eq!(request["memory_limit_mib"], 64, "create body: {request}");
}

/// P2: an eval output element outside 0..=255 is an error (never silently
/// dropped), and an out-of-i32 exit_code/status maps to -1, never 0.
#[tokio::test]
async fn eval_invalid_byte_element_and_huge_exit_code_fail_closed() {
    let (address, server) = mock_ndjson_lines(vec![
        "{\"out\":[104,300],\"status\":0}\n".to_owned(),
        "{\"out\":\"x\",\"exit_code\":4294967296}\n".to_owned(),
    ])
    .await;
    let (base, http) = http_server(
        json!([{"id":"sandbox-eval","snapshot_tag":"default","guest_addr":address}]).to_string(),
    )
    .await;
    let client = ForkdClient::new(ForkdConfig {
        base_url: base,
        guest_profile: ForkdGuestProfile::CustomShell,
        ..Default::default()
    })
    .unwrap();
    let sandbox = client
        .create(&CreateSandboxRequest::single("default"))
        .await
        .unwrap();
    // Element 300 is not a byte: an error, not a silently dropped element.
    let error = Sandbox::eval(&sandbox, EvalRequest::new("1 + 1"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, rfb::SandboxError::Execution(ref message) if message.contains("invalid output")),
        "got {error:?}"
    );
    // 2^32 wraps to 0 ("success") with `as i32`; it must map to -1.
    let result = Sandbox::eval(&sandbox, EvalRequest::new("1 + 1"))
        .await
        .unwrap();
    assert_eq!(result.status, Some(-1));
    let requests = server.await.unwrap();
    assert_eq!(requests[0]["action"], "eval");
    assert_eq!(requests[1]["action"], "eval");
    http.await.unwrap();
}

/// P1-6: a 50 KiB `data` payload (the guest-side cap) must be accepted: the
/// response limit is on raw payload bytes, not the JSON-encoded byte array.
#[tokio::test]
async fn read_50kib_payload_survives_the_ndjson_response_limit() {
    let payload = vec![7u8; 50 * 1024];
    let mut response = String::from("{\"data\":[");
    let numbers = payload
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    response.push_str(&numbers);
    response.push_str("],\"truncated\":true,\"total_bytes\":60000}\n");
    let (base, server, http) = guest_controller_fixture("sandbox-read", response).await;
    let client = client(base);
    let sandbox = client
        .create(&CreateSandboxRequest::single("default"))
        .await
        .unwrap();
    let read = Sandbox::read(&sandbox, ReadRequest::new("big.bin"))
        .await
        .unwrap();
    assert_eq!(read.data.len(), 50 * 1024);
    assert!(read.truncated);
    let request = server.await.unwrap();
    assert_eq!(request["action"], "read");
    http.await.unwrap();
}

/// P2: FORKD_URL is trimmed exactly like client/facade.rs.
#[test]
fn forkd_url_env_is_trimmed_like_facade() {
    let _guard = env_lock();
    std::env::set_var("FORKD_URL", "  http://127.0.0.1:8889  ");
    let config = ForkdConfig::from_env();
    std::env::remove_var("FORKD_URL");
    assert_eq!(config.base_url, "http://127.0.0.1:8889");
    // P2: the default controller timeout covers snapshot-restore creates.
    assert_eq!(ForkdConfig::default().timeout, Duration::from_secs(60));
}
