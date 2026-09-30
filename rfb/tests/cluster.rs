#![cfg(feature = "forkd")]

//! Cluster scheduling tests against in-process mock controllers: least-inflight
//! and round-robin placement, failure classification (transport / 5xx / decode
//! vs. 4xx), failover, tripping with lazy half-open recovery, post-failure
//! orphan reconciliation, RAII in-flight accounting, preflight digest checks,
//! and cluster-wide list/delete aggregation.

use rfb::cluster::{
    ClusterConfig, ClusterProvider, ClusterSandbox, ScheduleStrategy, DEFAULT_RECOVERY_INTERVAL,
};
use rfb::forkd::ForkdSandbox;
use rfb::{BackendKind, ProviderError, Resources, Sandbox, SandboxProvider, SandboxSpec};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Configuration knobs of one mock controller node.
#[derive(Clone)]
struct MockConfig {
    /// Sandbox ids returned by create (cycled); empty generates `sbx-N`.
    sandbox_ids: Vec<String>,
    /// Literal body for a 200 create response (for example malformed JSON).
    create_body: Option<String>,
    /// Status returned by create when it is not a plain success.
    create_status: u16,
    /// Status returned by the sandbox-listing endpoint.
    list_status: u16,
    /// Number of initial creates answered 503 before the node recovers.
    fail_first: usize,
    /// Close the connection without answering create (transport failure).
    disconnect_create: bool,
    /// Sandboxes silently stored server-side by one disconnecting create.
    store_before_disconnect: usize,
    /// Omit `created_at_unix` on sandboxes stored by a disconnect.
    omit_created_at: bool,
    /// Snapshot tag the node serves.
    snapshot_tag: String,
    /// Whether the node lists the snapshot tag at all.
    snapshot_present: bool,
    /// Snapshot lifecycle status reported by list/info.
    snapshot_status: String,
    /// Whether the snapshot is bootable.
    bootable: bool,
    /// Snapshot digest; `None` means the controller reports no digest.
    digest: Option<String>,
    /// Extra sandboxes returned by the sandbox listing.
    seed: Vec<SeedSandbox>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            sandbox_ids: Vec::new(),
            create_body: None,
            create_status: 200,
            list_status: 200,
            fail_first: 0,
            disconnect_create: false,
            store_before_disconnect: 0,
            omit_created_at: false,
            snapshot_tag: "base".into(),
            snapshot_present: true,
            snapshot_status: "ready".into(),
            bootable: true,
            digest: Some("sha256:test".into()),
            seed: Vec::new(),
        }
    }
}

/// A sandbox pre-seeded into one node's listing.
#[derive(Clone)]
struct SeedSandbox {
    id: String,
    created_at_unix: Option<i64>,
}

/// One sandbox the mock keeps in its (single-client) store.
#[derive(Clone)]
struct Stored {
    id: String,
    snapshot_tag: String,
    created_at_unix: Option<i64>,
}

/// Blocks create responses until the test opens it (`open` by default).
struct Gate {
    open: AtomicBool,
}

impl Gate {
    fn new() -> Self {
        Self {
            open: AtomicBool::new(true),
        }
    }
    fn close(&self) {
        self.open.store(false, Ordering::Release);
    }
    fn release(&self) {
        self.open.store(true, Ordering::Release);
    }
    async fn wait(&self) {
        while !self.open.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

/// Server-side state of one mock node.
struct MockServer {
    cfg: MockConfig,
    creates: AtomicUsize,
    deletes: AtomicUsize,
    lists: AtomicUsize,
    ids: AtomicUsize,
    sandboxes: Mutex<Vec<Stored>>,
    create_bodies: Mutex<Vec<String>>,
    deleted_ids: Mutex<Vec<String>>,
    gate: Gate,
}

impl MockServer {
    fn new(cfg: MockConfig) -> Self {
        let seeded = cfg
            .seed
            .iter()
            .map(|sandbox| Stored {
                id: sandbox.id.clone(),
                snapshot_tag: cfg.snapshot_tag.clone(),
                created_at_unix: sandbox.created_at_unix,
            })
            .collect();
        Self {
            cfg,
            creates: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            ids: AtomicUsize::new(0),
            sandboxes: Mutex::new(seeded),
            create_bodies: Mutex::new(Vec::new()),
            deleted_ids: Mutex::new(Vec::new()),
            gate: Gate::new(),
        }
    }

    fn next_id(&self) -> String {
        let n = self.ids.fetch_add(1, Ordering::Relaxed);
        match self
            .cfg
            .sandbox_ids
            .get(n % self.cfg.sandbox_ids.len().max(1))
        {
            Some(id) => id.clone(),
            None => format!("sbx-{}", n + 1),
        }
    }

    async fn handle(&self, method: &str, path: &str, body: &str) -> Option<String> {
        if method == "POST" && path == "/v1/sandboxes" {
            return self.handle_create(body).await;
        }
        if method == "GET" && path == "/v1/sandboxes" {
            self.lists.fetch_add(1, Ordering::Relaxed);
            if self.cfg.list_status != 200 {
                return Some(json_response(
                    self.cfg.list_status,
                    &serde_json::json!({"error": "node unavailable"}),
                ));
            }
            let rows: Vec<serde_json::Value> = self
                .sandboxes
                .lock()
                .unwrap()
                .iter()
                .map(stored_json)
                .collect();
            return Some(json_response(200, &serde_json::Value::Array(rows)));
        }
        if method == "GET" && path == "/v1/snapshots" {
            if !self.cfg.snapshot_present {
                return Some(json_response(200, &serde_json::Value::Array(Vec::new())));
            }
            return Some(json_response(
                200,
                &serde_json::Value::Array(vec![snapshot_json(&self.cfg)]),
            ));
        }
        if method == "GET" && path.starts_with("/v1/snapshots/") {
            if !self.cfg.snapshot_present {
                return Some(json_response(
                    404,
                    &serde_json::json!({"error": "no such snapshot"}),
                ));
            }
            return Some(json_response(200, &snapshot_json(&self.cfg)));
        }
        if method == "DELETE" && path.starts_with("/v1/sandboxes/") {
            let id = path.trim_start_matches("/v1/sandboxes/").to_string();
            self.deletes.fetch_add(1, Ordering::Relaxed);
            self.sandboxes
                .lock()
                .unwrap()
                .retain(|stored| stored.id != id);
            self.deleted_ids.lock().unwrap().push(id);
            return Some(json_response(200, &serde_json::json!({})));
        }
        Some(json_response(
            404,
            &serde_json::json!({"error": "not found"}),
        ))
    }

    async fn handle_create(&self, body: &str) -> Option<String> {
        self.creates.fetch_add(1, Ordering::Relaxed);
        self.create_bodies.lock().unwrap().push(body.to_string());
        self.gate.wait().await;
        if self.cfg.fail_first > 0 && self.creates.load(Ordering::Relaxed) <= self.cfg.fail_first {
            return Some(json_response(
                503,
                &serde_json::json!({"error": "node unavailable"}),
            ));
        }
        if self.cfg.disconnect_create {
            for _ in 0..self.cfg.store_before_disconnect {
                let id = self.next_id();
                let created_at_unix = if self.cfg.omit_created_at {
                    None
                } else {
                    Some(unix_secs())
                };
                self.sandboxes.lock().unwrap().push(Stored {
                    id,
                    snapshot_tag: self.cfg.snapshot_tag.clone(),
                    created_at_unix,
                });
            }
            // Transport-level failure: the client sees a closed connection.
            return None;
        }
        if let Some(body) = &self.cfg.create_body {
            return Some(json_response(200, &serde_json::Value::String(body.clone())));
        }
        if self.cfg.create_status != 200 {
            return Some(json_response(
                self.cfg.create_status,
                &serde_json::json!({"error": "create rejected"}),
            ));
        }
        let id = self.next_id();
        self.sandboxes.lock().unwrap().push(Stored {
            id: id.clone(),
            snapshot_tag: self.cfg.snapshot_tag.clone(),
            created_at_unix: Some(unix_secs()),
        });
        Some(json_response(
            200,
            &serde_json::json!([{
                "id": id,
                "snapshot_tag": self.cfg.snapshot_tag,
                "guest_addr": "127.0.0.1:9100",
            }]),
        ))
    }
}

/// A mock forkd controller node handling one connection per request.
struct MockNode {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    state: Arc<MockServer>,
}

impl MockNode {
    async fn start(cfg: MockConfig) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(MockServer::new(cfg));
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _ = serve(&mut stream, &state).await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });
        Self {
            address,
            task,
            state,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn create_requests(&self) -> usize {
        self.state.creates.load(Ordering::Relaxed)
    }

    fn delete_requests(&self) -> usize {
        self.state.deletes.load(Ordering::Relaxed)
    }

    fn lists(&self) -> usize {
        self.state.lists.load(Ordering::Relaxed)
    }

    fn deleted_ids(&self) -> Vec<String> {
        self.state.deleted_ids.lock().unwrap().clone()
    }

    fn create_bodies(&self) -> Vec<String> {
        self.state.create_bodies.lock().unwrap().clone()
    }

    /// Hold every subsequent create response until `release_creates`.
    fn hold_creates(&self) {
        self.state.gate.close();
    }

    fn release_creates(&self) {
        self.state.gate.release();
    }
}

impl Drop for MockNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(stream: &mut tokio::net::TcpStream, state: &MockServer) -> std::io::Result<()> {
    let Some((head, body)) = read_request(stream).await else {
        return Ok(());
    };
    let mut parts = head.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    if let Some(response) = state.handle(&method, &path, &body).await {
        stream.write_all(response.as_bytes()).await?;
    }
    Ok(())
}

/// Read one HTTP request (head and full body) from the connection.
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n".as_slice())
        {
            break position + 4;
        }
        if buffer.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Some((head, String::from_utf8_lossy(&body).into_owned()))
}

fn json_response(status: u16, body: &serde_json::Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} MOCK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn stored_json(stored: &Stored) -> serde_json::Value {
    let mut value = serde_json::json!({
        "id": &stored.id,
        "snapshot_tag": &stored.snapshot_tag,
        "guest_addr": "127.0.0.1:9100",
    });
    if let Some(created_at_unix) = stored.created_at_unix {
        value["created_at_unix"] = serde_json::json!(created_at_unix);
    }
    value
}

fn snapshot_json(cfg: &MockConfig) -> serde_json::Value {
    serde_json::json!({
        "tag": &cfg.snapshot_tag,
        "status": &cfg.snapshot_status,
        "bootable": cfg.bootable,
        "digest": cfg.digest,
        "created_at_unix": 1,
    })
}

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn config(nodes: &[String], strategy: ScheduleStrategy) -> ClusterConfig {
    ClusterConfig {
        nodes: nodes.to_vec(),
        token: None,
        timeout: Duration::from_secs(2),
        snapshot_tag: "base".into(),
        strategy,
        guest_timeout: Duration::from_secs(2),
        recovery_interval: DEFAULT_RECOVERY_INTERVAL,
    }
}

fn cluster(nodes: &[String], strategy: ScheduleStrategy) -> ClusterProvider {
    ClusterProvider::new(config(nodes, strategy)).unwrap()
}

/// Create with an explicit (default) spec — the production entry point.
async fn create(provider: &ClusterProvider) -> Result<ClusterSandbox, ProviderError> {
    provider.create_sandbox_with(&SandboxSpec::default()).await
}

fn expect_err(result: Result<ClusterSandbox, ProviderError>) -> ProviderError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected create to fail"),
    }
}

async fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    for _ in 0..2000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("timed out waiting for {what}");
}

/// One full create round per failure needed to trip the counter.
const FAILURE_TRIP_ROUNDS: usize = rfb::cluster::FAILURE_TRIP;

#[tokio::test]
async fn round_robin_spreads_creates_across_nodes() {
    let a = MockNode::start(MockConfig {
        sandbox_ids: vec!["sbx-11".into()],
        ..Default::default()
    })
    .await;
    let b = MockNode::start(MockConfig {
        sandbox_ids: vec!["sbx-22".into()],
        ..Default::default()
    })
    .await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::RoundRobin);
    let first = provider.create_sandbox().await.unwrap();
    let second = provider.create_sandbox().await.unwrap();
    assert_ne!(first.node_base_url(), second.node_base_url());
    assert_eq!(first.id(), "sbx-11");
    assert_eq!(second.id(), "sbx-22");
}

#[tokio::test]
async fn least_inflight_prefers_the_idle_node() {
    let a = MockNode::start(MockConfig::default()).await;
    let b = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::LeastInflight);
    let first = create(&provider).await.unwrap();
    // First create lands on the round-robin tie-break node; the second must
    // go to the other node (inflight 1 vs 0).
    let second = create(&provider).await.unwrap();
    assert_ne!(first.node_base_url(), second.node_base_url());
    let status = provider.node_status();
    assert!(status
        .iter()
        .all(|(_, healthy, inflight)| *healthy && *inflight == 1));
    // Dropping the handle releases the RAII slot (no manual release needed).
    drop(first);
    assert_eq!(provider.node_status()[0].2 + provider.node_status()[1].2, 1);
}

#[tokio::test]
async fn create_fails_over_to_a_healthy_node() {
    let dead = MockNode::start(MockConfig {
        create_status: 503,
        list_status: 503,
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig {
        sandbox_ids: vec!["sbx-77".into()],
        ..Default::default()
    })
    .await;
    let provider = cluster(&[dead.url(), live.url()], ScheduleStrategy::RoundRobin);
    let sandbox = create(&provider).await.unwrap();
    assert_eq!(sandbox.node_base_url(), live.url());
    assert_eq!(sandbox.id(), "sbx-77");
    // The failure was counted on the dead node only, and the placeholder
    // reservation on it was rolled back.
    let statuses = provider.node_statuses();
    assert_eq!(statuses[0].failures, 1);
    assert_eq!(statuses[0].inflight, 0);
    assert_eq!(statuses[1].failures, 0);
    assert_eq!(statuses[1].inflight, 1);
}

#[tokio::test]
async fn a_tripped_node_is_skipped_and_probe_revives_it() {
    let flaky = MockNode::start(MockConfig {
        create_status: 503,
        list_status: 503,
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig {
        sandbox_ids: vec!["sbx-31".into()],
        ..Default::default()
    })
    .await;
    // A long interval keeps the tripped node out of the candidate set for the
    // whole test: no accidental half-open probe.
    let provider = ClusterProvider::new(ClusterConfig {
        recovery_interval: Duration::from_secs(3600),
        ..config(&[flaky.url(), live.url()], ScheduleStrategy::LeastInflight)
    })
    .unwrap();
    // LeastInflight ties are settled by cursor rotation, so with a live
    // sibling the flaky node is only the first candidate on alternating
    // rounds: 2 * FAILURE_TRIP - 1 creates accumulate FAILURE_TRIP failures
    // on it. The long interval keeps it gated out afterwards.
    for _ in 0..(2 * FAILURE_TRIP_ROUNDS - 1) {
        create(&provider).await.unwrap();
    }
    // After FAILURE_TRIP consecutive failures the node is skipped entirely;
    // probes fail (503 on list) and keep it unhealthy.
    assert!(!provider.node_status()[0].1);
    assert!(provider.probe().await[0].is_err());
    let sandbox = create(&provider).await.unwrap();
    assert_eq!(sandbox.node_base_url(), live.url());
    assert_eq!(provider.node_status()[0].2, 0);
    assert!(provider.node_statuses()[0].next_trial_unix_ms.is_some());
}

#[tokio::test]
async fn all_nodes_down_reports_unavailable() {
    let a = MockNode::start(MockConfig {
        create_status: 503,
        ..Default::default()
    })
    .await;
    let b = MockNode::start(MockConfig {
        create_status: 503,
        ..Default::default()
    })
    .await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::RoundRobin);
    let error = expect_err(create(&provider).await);
    assert!(error.to_string().contains("node"), "{error}");
    // Both nodes were attempted exactly once before giving up.
    assert_eq!(a.create_requests(), 1);
    assert_eq!(b.create_requests(), 1);
}

#[test]
fn empty_cluster_is_rejected() {
    let error = match ClusterProvider::new(ClusterConfig::from_urls(Vec::<String>::new(), "base")) {
        Err(error) => error,
        Ok(_) => panic!("empty cluster must be rejected"),
    };
    assert!(error.to_string().contains("at least one node"));
}

#[test]
fn empty_snapshot_tag_is_rejected() {
    let error = match ClusterProvider::new(ClusterConfig::from_urls(["http://127.0.0.1:1"], "   "))
    {
        Err(error) => error,
        Ok(_) => panic!("blank snapshot tag must be rejected"),
    };
    assert!(error.to_string().contains("snapshot tag"), "{error}");
}

#[tokio::test]
async fn drop_releases_inflight_without_underflow() {
    let node = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[node.url()], ScheduleStrategy::RoundRobin);
    let sandbox = create(&provider).await.unwrap();
    assert_eq!(provider.node_status()[0].2, 1);
    // Legacy explicit release, then the RAII guard: saturating, never -1.
    let inner: &ForkdSandbox = sandbox.inner();
    provider.release(inner);
    assert_eq!(provider.node_status()[0].2, 0);
    drop(sandbox);
    assert_eq!(provider.node_status()[0].2, 0);
}

#[tokio::test]
async fn clone_shares_scheduler_state() {
    let a = MockNode::start(MockConfig::default()).await;
    let b = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::RoundRobin);
    let clone = provider.clone();
    let first = clone
        .create_sandbox_with(&SandboxSpec::default())
        .await
        .unwrap();
    // In-flight reservations made through the clone are visible through the
    // original handle: counters live behind shared Arcs, not copies.
    assert_eq!(
        provider
            .node_statuses()
            .iter()
            .map(|status| status.inflight)
            .sum::<usize>(),
        1
    );
    let second = provider
        .create_sandbox_with(&SandboxSpec::default())
        .await
        .unwrap();
    assert_ne!(first.node_base_url(), second.node_base_url());
    drop(first);
    assert_eq!(
        provider
            .node_statuses()
            .iter()
            .map(|status| status.inflight)
            .sum::<usize>(),
        1
    );
    drop(clone);
    // Failure counters are shared too (the legacy Clone copied them).
    let flaky = MockNode::start(MockConfig {
        create_status: 503,
        list_status: 503,
        ..Default::default()
    })
    .await;
    let healthy = MockNode::start(MockConfig::default()).await;
    let other = cluster(&[flaky.url(), healthy.url()], ScheduleStrategy::RoundRobin);
    let other_clone = other.clone();
    other_clone
        .create_sandbox_with(&SandboxSpec::default())
        .await
        .unwrap();
    assert_eq!(other.node_statuses()[0].failures, 1);
    assert_eq!(other.node_statuses()[0].inflight, 0);
}

#[tokio::test]
async fn round_robin_rotates_across_three_nodes() {
    let a = MockNode::start(MockConfig::default()).await;
    let b = MockNode::start(MockConfig::default()).await;
    let c = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[a.url(), b.url(), c.url()], ScheduleStrategy::RoundRobin);
    let mut seen = HashSet::new();
    for _ in 0..3 {
        seen.insert(create(&provider).await.unwrap().node_base_url().to_owned());
    }
    // A fresh provider used to keep scheduling onto node 0; the shared cursor
    // must rotate through all three.
    assert_eq!(seen.len(), 3);
    let fourth = create(&provider).await.unwrap();
    assert!(seen.contains(fourth.node_base_url()));
}

#[tokio::test]
async fn client_error_propagates_without_counting_or_failover() {
    let reject = MockNode::start(MockConfig {
        create_status: 400,
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[reject.url(), live.url()], ScheduleStrategy::RoundRobin);
    let error = expect_err(create(&provider).await);
    let message = error.to_string();
    assert!(message.contains("rejected create"), "{message}");
    assert!(message.contains(&reject.url()), "{message}");
    assert_eq!(reject.create_requests(), 1);
    // 4xx never triggers failover: no duplicate is created elsewhere.
    assert_eq!(live.create_requests(), 0);
    let statuses = provider.node_statuses();
    assert_eq!(statuses[0].failures, 0);
    assert_eq!(statuses[0].inflight, 0);
}

#[tokio::test]
async fn decode_failure_counts_and_fails_over() {
    let broken = MockNode::start(MockConfig {
        create_body: Some("not-json".into()),
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig {
        sandbox_ids: vec!["sbx-9".into()],
        ..Default::default()
    })
    .await;
    let provider = cluster(&[broken.url(), live.url()], ScheduleStrategy::RoundRobin);
    let sandbox = create(&provider).await.unwrap();
    assert_eq!(sandbox.node_base_url(), live.url());
    assert_eq!(sandbox.id(), "sbx-9");
    // Decode is a node fault: counted, reconciled (list attempt), then failed
    // over. Nothing matched, so nothing was deleted.
    assert_eq!(provider.node_statuses()[0].failures, 1);
    assert_eq!(provider.node_statuses()[0].inflight, 0);
    assert!(broken.lists() >= 1);
    assert_eq!(broken.delete_requests(), 0);
}

#[tokio::test]
async fn transport_failure_reconciles_exactly_one_orphan() {
    let flaky = MockNode::start(MockConfig {
        sandbox_ids: vec!["orphan-1".into()],
        disconnect_create: true,
        store_before_disconnect: 1,
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig {
        sandbox_ids: vec!["sbx-ok".into()],
        ..Default::default()
    })
    .await;
    let provider = cluster(&[flaky.url(), live.url()], ScheduleStrategy::RoundRobin);
    let sandbox = create(&provider).await.unwrap();
    // The create landed on the healthy node...
    assert_eq!(sandbox.node_base_url(), live.url());
    assert_eq!(sandbox.id(), "sbx-ok");
    assert_eq!(live.create_requests(), 1);
    // ... and the sandbox that the dead node had already created (the
    // "created, then the connection died" case) was found by reconciliation
    // and deleted exactly once on that node.
    assert_eq!(flaky.create_requests(), 1);
    assert_eq!(flaky.delete_requests(), 1);
    assert_eq!(flaky.deleted_ids(), vec!["orphan-1".to_string()]);
}

#[tokio::test]
async fn reconciliation_refuses_ambiguous_matches() {
    let flaky = MockNode::start(MockConfig {
        sandbox_ids: vec!["orphan-a".into(), "orphan-b".into()],
        disconnect_create: true,
        store_before_disconnect: 2,
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[flaky.url(), live.url()], ScheduleStrategy::RoundRobin);
    let sandbox = create(&provider).await.unwrap();
    assert_eq!(sandbox.node_base_url(), live.url());
    // Two candidates in the window: prefer a leak over a wrong delete.
    assert_eq!(flaky.delete_requests(), 0);
    let listed = provider.list_all().await;
    assert_eq!(listed[0].sandboxes.len(), 2);
}

#[tokio::test]
async fn reconciliation_ignores_missing_created_at() {
    let flaky = MockNode::start(MockConfig {
        sandbox_ids: vec!["orphan-1".into()],
        disconnect_create: true,
        store_before_disconnect: 1,
        omit_created_at: true,
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[flaky.url(), live.url()], ScheduleStrategy::RoundRobin);
    let sandbox = create(&provider).await.unwrap();
    assert_eq!(sandbox.node_base_url(), live.url());
    // Without a timestamp the sandbox is not provably this attempt's orphan.
    assert_eq!(flaky.delete_requests(), 0);
}

#[tokio::test]
async fn half_open_probe_after_interval_recovers() {
    let node = MockNode::start(MockConfig {
        fail_first: FAILURE_TRIP_ROUNDS,
        ..Default::default()
    })
    .await;
    let provider = ClusterProvider::new(ClusterConfig {
        recovery_interval: Duration::from_millis(50),
        ..config(&[node.url()], ScheduleStrategy::RoundRobin)
    })
    .unwrap();
    for _ in 0..FAILURE_TRIP_ROUNDS {
        assert!(create(&provider).await.is_err());
    }
    let tripped = provider.node_statuses();
    assert!(tripped[0].tripped);
    assert!(tripped[0].next_trial_unix_ms.is_some());
    tokio::time::sleep(Duration::from_millis(120)).await;
    // The next create after the interval goes through as a half-open probe.
    let sandbox = create(&provider).await.unwrap();
    assert_eq!(sandbox.node_base_url(), node.url());
    let recovered = provider.node_statuses();
    assert!(!recovered[0].tripped);
    assert_eq!(recovered[0].failures, 0);
    assert_eq!(recovered[0].next_trial_unix_ms, None);
    assert_eq!(node.create_requests(), FAILURE_TRIP_ROUNDS + 1);
}

#[tokio::test]
async fn half_open_probe_runs_at_most_once_per_interval() {
    let node = MockNode::start(MockConfig {
        fail_first: FAILURE_TRIP_ROUNDS,
        ..Default::default()
    })
    .await;
    let provider = ClusterProvider::new(ClusterConfig {
        // Long enough that the second create cannot sneak past the gate
        // claimed by the first probe while the mock holds its response.
        recovery_interval: Duration::from_secs(1),
        ..config(&[node.url()], ScheduleStrategy::RoundRobin)
    })
    .unwrap();
    for _ in 0..FAILURE_TRIP_ROUNDS {
        assert!(create(&provider).await.is_err());
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // Hold the probe's response so the second create races a probe in flight.
    node.hold_creates();
    let probing = tokio::spawn({
        let provider = provider.clone();
        async move { create(&provider).await }
    });
    wait_for(
        || node.create_requests() > FAILURE_TRIP_ROUNDS,
        "held probe",
    )
    .await;
    let second = expect_err(create(&provider).await);
    assert!(
        second
            .to_string()
            .contains("all cluster nodes are unhealthy"),
        "{second}"
    );
    node.release_creates();
    let first = probing.await.unwrap().unwrap();
    assert_eq!(first.node_base_url(), node.url());
    // Exactly one probe request went out for the whole interval.
    assert_eq!(node.create_requests(), FAILURE_TRIP_ROUNDS + 1);
    assert_eq!(provider.node_statuses()[0].failures, 0);
}

#[tokio::test]
async fn node_statuses_report_tripped_node_and_gate() {
    let flaky = MockNode::start(MockConfig {
        create_status: 503,
        list_status: 503,
        ..Default::default()
    })
    .await;
    let live = MockNode::start(MockConfig::default()).await;
    let provider = ClusterProvider::new(ClusterConfig {
        recovery_interval: Duration::from_secs(3600),
        ..config(&[flaky.url(), live.url()], ScheduleStrategy::LeastInflight)
    })
    .unwrap();
    // LeastInflight ties rotate by cursor, so the flaky node only leads on
    // alternating rounds: 2 * FAILURE_TRIP - 1 creates give it
    // FAILURE_TRIP failures.
    for _ in 0..(2 * FAILURE_TRIP_ROUNDS - 1) {
        create(&provider).await.unwrap();
    }
    let statuses = provider.node_statuses();
    assert!(statuses[0].tripped);
    assert!(!statuses[0].healthy);
    assert_eq!(statuses[0].failures, FAILURE_TRIP_ROUNDS);
    let gate = statuses[0]
        .next_trial_unix_ms
        .expect("a trip must arm the half-open gate");
    assert!(gate > unix_millis());
    assert!(!statuses[1].tripped);
    assert_eq!(statuses[1].next_trial_unix_ms, None);
    assert_eq!(provider.node_status()[0].1, statuses[0].healthy);
}

#[tokio::test]
async fn preflight_reports_matching_digests() {
    let a = MockNode::start(MockConfig {
        digest: Some("sha256:abc".into()),
        ..Default::default()
    })
    .await;
    let b = MockNode::start(MockConfig {
        digest: Some("sha256:abc".into()),
        ..Default::default()
    })
    .await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::RoundRobin);
    let report = provider.preflight().await;
    assert_eq!(report.nodes.len(), 2);
    assert!(report
        .nodes
        .iter()
        .all(|node| node.ready && node.error.is_none()));
    assert_eq!(report.nodes[0].digest.as_deref(), Some("sha256:abc"));
    assert_eq!(report.nodes[0].status, "ready");
    assert!(report.digests_agree);
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

#[tokio::test]
async fn preflight_flags_digest_mismatch_and_missing_digest() {
    let a = MockNode::start(MockConfig {
        digest: Some("sha256:abc".into()),
        ..Default::default()
    })
    .await;
    let b = MockNode::start(MockConfig {
        digest: Some("sha256:def".into()),
        ..Default::default()
    })
    .await;
    let c = MockNode::start(MockConfig {
        digest: None,
        ..Default::default()
    })
    .await;
    let provider = cluster(&[a.url(), b.url(), c.url()], ScheduleStrategy::RoundRobin);
    let report = provider.preflight().await;
    assert!(!report.digests_agree);
    assert!(report.nodes.iter().all(|node| node.ready));
    assert_eq!(report.nodes[2].digest, None);
    assert!(report
        .notes
        .iter()
        .any(|note| note.contains("no snapshot digest")));
}

#[tokio::test]
async fn preflight_reports_missing_snapshot() {
    let missing = MockNode::start(MockConfig {
        snapshot_present: false,
        ..Default::default()
    })
    .await;
    let provider = cluster(&[missing.url()], ScheduleStrategy::RoundRobin);
    let report = provider.preflight().await;
    assert!(!report.nodes[0].ready);
    assert!(report.nodes[0].error.is_some());
    assert!(report.notes.iter().any(|note| note.contains("missing")));
    assert!(!report.digests_agree);
}

#[tokio::test]
async fn memory_bytes_become_a_ceiling_in_mib() {
    let node = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[node.url()], ScheduleStrategy::RoundRobin);
    let spec = SandboxSpec {
        resources: Resources {
            memory_bytes: Some(1_500_000),
            ..Default::default()
        },
        ..Default::default()
    };
    let sandbox = provider.create_sandbox_with(&spec).await.unwrap();
    assert_eq!(sandbox.id(), "sbx-1");
    let bodies = node.create_bodies();
    assert_eq!(bodies.len(), 1);
    // 1_500_000 bytes rounds up to the 2 MiB ceiling.
    assert!(
        bodies[0].contains("\"memory_limit_mib\":2"),
        "{}",
        bodies[0]
    );
}

#[tokio::test]
async fn unsupported_cpus_are_rejected_without_a_request() {
    let node = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[node.url()], ScheduleStrategy::RoundRobin);
    let spec = SandboxSpec {
        resources: Resources {
            cpus: Some(2),
            ..Default::default()
        },
        ..Default::default()
    };
    let error = expect_err(provider.create_sandbox_with(&spec).await);
    assert!(error.to_string().contains("cpus"), "{error}");
    assert_eq!(node.create_requests(), 0);
}

#[tokio::test]
async fn list_all_aggregates_nodes_and_delete_on_routes_exactly() {
    let live = MockNode::start(MockConfig {
        seed: vec![SeedSandbox {
            id: "sbx-seed-1".into(),
            created_at_unix: Some(unix_secs()),
        }],
        ..Default::default()
    })
    .await;
    let down = MockNode::start(MockConfig {
        list_status: 503,
        ..Default::default()
    })
    .await;
    let provider = cluster(&[down.url(), live.url()], ScheduleStrategy::RoundRobin);
    let listed = provider.list_all().await;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].base_url, down.url());
    assert!(listed[0].error.is_some());
    assert!(listed[0].sandboxes.is_empty());
    assert_eq!(listed[1].base_url, live.url());
    assert!(listed[1].error.is_none());
    assert_eq!(listed[1].sandboxes.len(), 1);
    assert_eq!(listed[1].sandboxes[0].id, "sbx-seed-1");

    provider.delete_on(&live.url(), "sbx-seed-1").await.unwrap();
    assert_eq!(live.delete_requests(), 1);
    assert_eq!(live.deleted_ids(), vec!["sbx-seed-1".to_string()]);

    let error = match provider.delete_on("http://127.0.0.1:9", "sbx-1").await {
        Err(error) => error,
        Ok(()) => panic!("unknown node must be rejected"),
    };
    assert!(
        error.to_string().contains("unknown cluster node"),
        "{error}"
    );
    // delete_on never touches this process's in-flight counters.
    assert_eq!(
        provider
            .node_statuses()
            .iter()
            .map(|status| status.inflight)
            .sum::<usize>(),
        0
    );
}

#[tokio::test]
async fn sandbox_provider_create_path_uses_the_spec_and_releases_on_drop() {
    let node = MockNode::start(MockConfig::default()).await;
    let provider = cluster(&[node.url()], ScheduleStrategy::RoundRobin);
    let sandbox: Box<dyn Sandbox> = provider.create(SandboxSpec::default()).await.unwrap();
    assert_eq!(sandbox.backend(), BackendKind::VirtualMachine);
    assert_eq!(provider.node_status()[0].2, 1);
    drop(sandbox);
    assert_eq!(provider.node_status()[0].2, 0);
}
