//! Distributed sandbox cluster: one [`SandboxProvider`] spanning N forkd
//! controller nodes.
//!
//! Each node is a standard forkd controller (`forkd-controller serve`, the
//! same endpoint a single-host deployment uses) reachable over HTTP. The
//! provider schedules each `create` onto a node — least in-flight by default,
//! round-robin as a fallback strategy — and retries the remaining nodes when
//! the picked one fails at transport level or returns 5xx, so a dead node
//! degrades capacity instead of failing the create.
//!
//! The returned sandbox is the plain [`ForkdSandbox`]: exec/stream/filesystem
//! route to the sandbox's guest address directly (never through the
//! controller), and teardown deletes the sandbox on the node that owns it,
//! which the sandbox handle carries. Sandboxes therefore keep working across
//! node health flaps and no per-sandbox cluster state is required.

use crate::core::{
    BackendKind, BoxFuture, Capability, ProviderError, Sandbox, SandboxProvider, SandboxSpec,
    TransportKind,
};
use crate::forkd::{CreateSandboxRequest, ForkdClient, ForkdConfig, ForkdSandbox};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How the provider picks a node for the next sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScheduleStrategy {
    /// Schedule onto the node with the fewest live sandboxes created through
    /// this provider. Balances bursty usage automatically.
    #[default]
    LeastInflight,
    /// Strict round-robin across healthy nodes.
    RoundRobin,
}

/// Configuration for a cluster of forkd controller nodes.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// One controller endpoint per node (`http://host:8889`). At least one.
    pub nodes: Vec<String>,
    /// Bearer token sent to every node (unset for unauthenticated loops).
    pub token: Option<String>,
    /// Per-request controller timeout.
    pub timeout: Duration,
    /// Snapshot tag every sandbox boots from (the same image must be
    /// provisioned on all nodes).
    pub snapshot_tag: String,
    /// Node selection strategy.
    pub strategy: ScheduleStrategy,
    /// Guest-side NDJSON timeout (mirrors [`ForkdConfig::timeout`]).
    pub guest_timeout: Duration,
}

impl ClusterConfig {
    /// Build a config from controller URLs and the rest defaults. The guest
    /// timeout derives from [`ForkdConfig::default`] so the per-backend
    /// defaults stay aligned when they change.
    pub fn from_urls(
        urls: impl IntoIterator<Item = impl Into<String>>,
        snapshot_tag: impl Into<String>,
    ) -> Self {
        let forkd = ForkdConfig::default();
        Self {
            nodes: urls.into_iter().map(Into::into).collect(),
            token: None,
            timeout: forkd.timeout,
            snapshot_tag: snapshot_tag.into(),
            strategy: ScheduleStrategy::default(),
            guest_timeout: forkd.guest_timeout,
        }
    }
}

/// Consecutive failures after which a node is skipped until a probe succeeds.
pub const FAILURE_TRIP: usize = 3;

struct NodeState {
    client: ForkdClient,
    /// Live sandboxes this provider created on the node (scheduling signal).
    inflight: AtomicUsize,
    /// Consecutive create failures; reset on success or successful probe.
    failures: AtomicUsize,
}

impl Clone for NodeState {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            inflight: AtomicUsize::new(self.inflight.load(Ordering::Relaxed)),
            failures: AtomicUsize::new(self.failures.load(Ordering::Relaxed)),
        }
    }
}

/// A sandbox cluster: schedules sandbox creation across forkd controller
/// nodes with failover.
pub struct ClusterProvider {
    nodes: Vec<NodeState>,
    snapshot_tag: String,
    strategy: ScheduleStrategy,
    /// Round-robin cursor (also the least-inflight tie breaker).
    cursor: AtomicUsize,
}

impl Clone for ClusterProvider {
    fn clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
            snapshot_tag: self.snapshot_tag.clone(),
            strategy: self.strategy,
            cursor: AtomicUsize::new(self.cursor.load(Ordering::Relaxed)),
        }
    }
}

impl ClusterProvider {
    /// Construct a cluster provider from the given node configs.
    ///
    /// # Errors
    ///
    /// Returns `Err` when any node URL is malformed.
    pub fn new(config: ClusterConfig) -> Result<Self, ProviderError> {
        if config.nodes.is_empty() {
            return Err(ProviderError::Unavailable(
                "cluster requires at least one node URL".into(),
            ));
        }
        let mut nodes = Vec::with_capacity(config.nodes.len());
        for url in &config.nodes {
            let client = ForkdClient::new(ForkdConfig {
                base_url: url.clone(),
                token: config.token.clone(),
                timeout: config.timeout,
                snapshot_tag: Some(config.snapshot_tag.clone()),
                guest_timeout: config.guest_timeout,
                ..ForkdConfig::default()
            })
            .map_err(|e| ProviderError::Unavailable(e.to_string()))?;
            nodes.push(NodeState {
                client,
                inflight: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
            });
        }
        Ok(Self {
            nodes,
            snapshot_tag: config.snapshot_tag,
            strategy: config.strategy,
            cursor: AtomicUsize::new(0),
        })
    }

    /// Per-node health/inflight snapshot for ops dashboards:
    /// `(base_url, healthy, live_created)` in node order.
    pub fn node_status(&self) -> Vec<(String, bool, usize)> {
        self.nodes
            .iter()
            .map(|node| {
                (
                    node.client.base_url().to_owned(),
                    node.failures.load(Ordering::Relaxed) < FAILURE_TRIP,
                    node.inflight.load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// Probe every node's controller and reset failure counters on success.
    /// Returns one `Result` per node, in node order.
    pub async fn probe(&self) -> Vec<Result<(), String>> {
        let handles = self.nodes.iter().map(|node| {
            let client = node.client.clone();
            tokio::spawn(async move {
                client
                    .list_sandboxes()
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        });
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            results.push(
                handle
                    .await
                    .unwrap_or_else(|e| Err(format!("probe task failed: {e}"))),
            );
        }
        for (node, result) in self.nodes.iter().zip(&results) {
            if result.is_ok() {
                node.failures.store(0, Ordering::Relaxed);
            }
        }
        results
    }

    /// Next node index for the strategy, ignoring health.
    fn next_index(&self) -> usize {
        match self.strategy {
            ScheduleStrategy::RoundRobin => {
                self.cursor.fetch_add(1, Ordering::Relaxed) % self.nodes.len()
            }
            ScheduleStrategy::LeastInflight => {
                let cursor = self.cursor.fetch_add(1, Ordering::Relaxed) % self.nodes.len();
                let mut best = cursor;
                let mut best_load = self.nodes[cursor].inflight.load(Ordering::Relaxed);
                for (offset, node) in self.nodes.iter().enumerate() {
                    let load = node.inflight.load(Ordering::Relaxed);
                    if load < best_load {
                        best = offset;
                        best_load = load;
                    }
                }
                best
            }
        }
    }

    /// Create one sandbox: schedule onto a healthy node, fail over through
    /// the remaining nodes on transport/5xx errors.
    ///
    /// # Errors
    ///
    /// Returns `Err` when every node attempt fails (the last error is kept).
    pub async fn create_sandbox(&self) -> Result<ForkdSandbox, ProviderError> {
        let len = self.nodes.len();
        let start = self.next_index();
        let mut last: Option<ProviderError> = None;
        for offset in 0..len {
            let index = (start + offset) % len;
            let node = &self.nodes[index];
            if node.failures.load(Ordering::Relaxed) >= FAILURE_TRIP {
                continue;
            }
            let request = CreateSandboxRequest::single(&self.snapshot_tag);
            match node.client.create(&request).await {
                Ok(sandbox) => {
                    node.failures.store(0, Ordering::Relaxed);
                    node.inflight.fetch_add(1, Ordering::Relaxed);
                    return Ok(sandbox);
                }
                Err(error) => {
                    node.failures.fetch_add(1, Ordering::Relaxed);
                    last = Some(ProviderError::Unavailable(format!(
                        "node {} failed: {error}",
                        node.client.base_url()
                    )));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            ProviderError::Unavailable("all cluster nodes are unhealthy".into())
        }))
    }

    /// Report that a previously created sandbox ended (close/destroy), so
    /// least-inflight scheduling sees the true load.
    pub fn release(&self, sandbox: &ForkdSandbox) {
        let url = sandbox.node_base_url();
        if let Some(node) = self.nodes.iter().find(|n| n.client.base_url() == url) {
            node.inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl SandboxProvider for ClusterProvider {
    fn backend(&self) -> BackendKind {
        BackendKind::VirtualMachine
    }
    fn transport(&self) -> TransportKind {
        TransportKind::Tcp
    }
    fn capabilities(&self) -> &[Capability] {
        // The guest capability set is a property of the image, identical on
        // every node by construction (one snapshot tag).
        self.nodes
            .first()
            .map(|node| node.client.capabilities())
            .unwrap_or(&[])
    }
    fn create<'a>(
        &'a self,
        spec: SandboxSpec,
    ) -> BoxFuture<'a, Result<Box<dyn Sandbox>, ProviderError>> {
        Box::pin(async move {
            crate::core::check_create_spec(&spec, self.capabilities())?;
            Ok(Box::new(self.create_sandbox().await?) as Box<dyn Sandbox>)
        })
    }
}
