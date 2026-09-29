#![cfg(feature = "forkd")]

//! Cluster scheduling tests against in-process mock controllers: least-inflight
//! and round-robin placement, failover on node failure, failure tripping, and
//! probe-driven recovery.

use rfb::cluster::{ClusterConfig, ClusterProvider, ScheduleStrategy};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A mock forkd controller node. `status_code` selects the response status for
/// the create endpoint; every endpoint answers JSON.
struct MockNode {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl MockNode {
    async fn spawn(status_code: u16, sandbox_ids: &[usize]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let ids: Vec<usize> = sandbox_ids.to_vec();
        let task = tokio::spawn(async move {
            // Serve requests until the test drops the node; each accepted
            // connection is one HTTP/1.0-style request answered and closed.
            let mut round = 0usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let id = ids.get(round % ids.len().max(1)).copied().unwrap_or(0);
                round += 1;
                let mut request = [0u8; 8192];
                let _ = stream.read(&mut request).await;
                let request_head = String::from_utf8_lossy(&request);
                let body = if request_head.starts_with("POST /v1/sandboxes") {
                    if status_code == 200 {
                        format!(
                            "[{{\"id\":\"sbx-{id}\",\"snapshot_tag\":\"base\",\"guest_addr\":\"127.0.0.1:{id}\"}}]"
                        )
                    } else {
                        format!("{{\"error\":\"node {id} unavailable\"}}")
                    }
                } else {
                    "[]".to_string()
                };
                let response = format!(
                    "HTTP/1.1 {status_code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Self { address, task }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for MockNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn cluster(nodes: &[String], strategy: ScheduleStrategy) -> ClusterProvider {
    ClusterProvider::new(ClusterConfig {
        nodes: nodes.to_vec(),
        token: None,
        timeout: Duration::from_secs(2),
        snapshot_tag: "base".into(),
        strategy,
        guest_timeout: Duration::from_secs(2),
    })
    .unwrap()
}

#[tokio::test]
async fn round_robin_spreads_creates_across_nodes() {
    let a = MockNode::spawn(200, &[11]).await;
    let b = MockNode::spawn(200, &[22]).await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::RoundRobin);
    let first = provider.create_sandbox().await.unwrap();
    let second = provider.create_sandbox().await.unwrap();
    assert_ne!(first.node_base_url(), second.node_base_url());
    assert_eq!(first.id(), "sbx-11");
    assert_eq!(second.id(), "sbx-22");
}

#[tokio::test]
async fn least_inflight_prefers_the_idle_node() {
    let a = MockNode::spawn(200, &[11]).await;
    let b = MockNode::spawn(200, &[22]).await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::LeastInflight);
    let first = provider.create_sandbox().await.unwrap();
    // First create lands on the round-robin tie-break node; the second must
    // go to the other node (inflight 1 vs 0).
    let second = provider.create_sandbox().await.unwrap();
    assert_ne!(first.node_base_url(), second.node_base_url());
    let status = provider.node_status();
    assert!(status
        .iter()
        .all(|(_, healthy, inflight)| *healthy && *inflight == 1));
    provider.release(&first);
    assert_eq!(provider.node_status()[0].2 + provider.node_status()[1].2, 1);
}

#[tokio::test]
async fn create_fails_over_to_a_healthy_node() {
    let dead = MockNode::spawn(503, &[]).await;
    let live = MockNode::spawn(200, &[77]).await;
    let provider = cluster(&[dead.url(), live.url()], ScheduleStrategy::RoundRobin);
    let sandbox = provider.create_sandbox().await.unwrap();
    assert_eq!(sandbox.node_base_url(), live.url());
    assert_eq!(sandbox.id(), "sbx-77");
    // The failure was counted on the dead node only.
    let status = provider.node_status();
    assert!(status[0].2 == 0 && status[1].2 == 1);
}

#[tokio::test]
async fn a_tripped_node_is_skipped_and_probe_revives_it() {
    let flaky = MockNode::spawn(503, &[]).await;
    let live = MockNode::spawn(200, &[31, 32, 33]).await;
    let provider = cluster(&[flaky.url(), live.url()], ScheduleStrategy::LeastInflight);
    for _ in 0..FAILURE_TRIP_ROUNDS {
        provider.create_sandbox().await.unwrap();
    }
    // After FAILURE_TRIP consecutive failures the node is skipped entirely;
    // probes fail (503 on list) and keep it unhealthy.
    assert!(!provider.node_status()[0].1);
    assert!(provider.probe().await[0].is_err());
    let sandbox = provider.create_sandbox().await.unwrap();
    assert_eq!(sandbox.node_base_url(), live.url());
}

#[tokio::test]
async fn all_nodes_down_reports_unavailable() {
    let a = MockNode::spawn(503, &[]).await;
    let b = MockNode::spawn(503, &[]).await;
    let provider = cluster(&[a.url(), b.url()], ScheduleStrategy::RoundRobin);
    let error = match provider.create_sandbox().await {
        Err(error) => error,
        Ok(_) => panic!("all-nodes-down create must fail"),
    };
    assert!(error.to_string().contains("node"));
}

#[tokio::test]
async fn empty_cluster_is_rejected() {
    let error = match ClusterProvider::new(ClusterConfig::from_urls(Vec::<String>::new(), "base")) {
        Err(error) => error,
        Ok(_) => panic!("empty cluster must be rejected"),
    };
    assert!(error.to_string().contains("at least one node"));
}

/// One full create+release round per failure needed to trip the counter
/// (failover means each round increments the dead node's failures by one).
const FAILURE_TRIP_ROUNDS: usize = rfb::cluster::FAILURE_TRIP;
