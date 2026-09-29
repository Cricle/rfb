//! Distributed sandbox cluster: one [`SandboxProvider`] spanning N forkd
//! controller nodes.
//!
//! Each node is a standard forkd controller (`forkd-controller serve`, the
//! same endpoint a single-host deployment uses) reachable over HTTP. The
//! provider schedules each create onto a node, fails over to the remaining
//! nodes on node-level faults, and trips a node after [`FAILURE_TRIP`]
//! consecutive failures. The returned [`ClusterSandbox`] wraps the plain
//! [`ForkdSandbox`]: exec/stream/filesystem route to the sandbox's guest
//! address directly (never through the controller), teardown deletes the
//! sandbox on the node that owns it, and dropping the handle releases the
//! node's in-flight slot.
//!
//! # Scheduling signals are per-process
//!
//! `inflight` counts sandboxes this provider created and has not seen dropped
//! (RAII guard) or explicitly released ([`ClusterProvider::release`]); it is a
//! scheduling hint, never cluster truth. One process (or one shared
//! [`ClusterProvider`] handle) should own the schedule loop: with several
//! independent process schedulers, least-in-flight degrades to loose
//! round-robin. Use [`ClusterProvider::list_all`] plus
//! [`ClusterProvider::delete_on`] against the controllers themselves to find
//! and clean up orphans, for example after an owner process died.
//!
//! # Failure classification
//!
//! Every create attempt is classified:
//!
//! - transport failure, HTTP 5xx, or an undecodable response: the *node*
//!   faulted. It takes a failure strike and the create moves on to the next
//!   candidate.
//! - HTTP 4xx: the node understood the request and rejected it. The error
//!   propagates immediately (`node <url> rejected create: ...`) without a
//!   failure strike and without failover: retrying elsewhere risks a
//!   duplicate the caller never learns about.
//!
//! # Half-open recovery
//!
//! After [`FAILURE_TRIP`] consecutive failures a node is skipped, but only
//! until its recovery gate elapses: at most once per `recovery_interval`
//! (default [`DEFAULT_RECOVERY_INTERVAL`], 30 s) a single create is let
//! through as a lazy half-open probe. A successful probe clears the counter;
//! a failed one only re-arms the gate. [`ClusterProvider::probe`] forces the
//! same check immediately (and never counts a failure). No background task is
//! involved: probes happen on the create path.
//!
//! # Reconciliation after indeterminate failures
//!
//! A transport failure or an undecodable response may still have created the
//! sandbox on the node, so the provider lists that node afterwards and
//! deletes a *unique* orphan: the reported sandbox must match the configured
//! snapshot tag and its `created_at_unix` must fall inside this attempt's
//! window. Zero matches, several matches, a missing timestamp, or a failed
//! listing are only warned about (prefer a leaked sandbox over a wrongly
//! deleted one), and assume this process is the node's only client.
//!
//! # In-flight accounting is RAII
//!
//! Dropping the [`ClusterSandbox`] returned by create releases the node's
//! in-flight slot. [`ClusterProvider::release`] exists only for callers that
//! hold a bare [`ForkdSandbox`]; it decrements saturating, so a double release
//! can never underflow the counter.

use crate::core::{
    check_create_resources, check_create_spec, guest, BackendKind, BoxFuture, Capability,
    ExecResult, ExecSpec, ProviderError, Sandbox, SandboxError, SandboxProvider, SandboxSpec,
    TransportKind,
};
use crate::forkd::{
    CreateSandboxRequest, ForkdClient, ForkdClientError, ForkdConfig, ForkdSandbox, SandboxInfo,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Default lazy half-open probe interval for a tripped node: after
/// [`FAILURE_TRIP`] consecutive failures, the next create is allowed onto the
/// node at most once per interval until a probe succeeds.
pub const DEFAULT_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);

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
    /// Lazy half-open probe interval for tripped nodes
    /// (see [`DEFAULT_RECOVERY_INTERVAL`]).
    pub recovery_interval: Duration,
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
            recovery_interval: DEFAULT_RECOVERY_INTERVAL,
        }
    }

    /// Override the lazy half-open probe interval for tripped nodes.
    #[must_use]
    pub fn with_recovery_interval(mut self, recovery_interval: Duration) -> Self {
        self.recovery_interval = recovery_interval;
        self
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
    /// Unix-ms gate for the next lazy half-open probe (0 = none armed).
    next_trial_ms: AtomicU64,
}

/// A sandbox cluster: schedules sandbox creation across forkd controller
/// nodes with failover.
///
/// Cloning shares the node list, counters, and cursor behind `Arc`s, so every
/// clone schedules through the same state.
#[derive(Clone)]
pub struct ClusterProvider {
    nodes: Vec<Arc<NodeState>>,
    snapshot_tag: String,
    strategy: ScheduleStrategy,
    /// Round-robin cursor (also the least-inflight tie breaker).
    cursor: Arc<AtomicUsize>,
    /// Lazy half-open probe interval for tripped nodes.
    recovery_interval: Duration,
}

/// Per-node status snapshot for ops dashboards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    /// Controller base URL of the node.
    pub base_url: String,
    /// Whether the node is currently admitted for scheduling.
    pub healthy: bool,
    /// Live sandboxes this provider created on the node.
    pub inflight: usize,
    /// Consecutive create failures observed on the node.
    pub failures: usize,
    /// Whether the failure count has reached [`FAILURE_TRIP`].
    pub tripped: bool,
    /// Unix-ms of the next allowed half-open probe, when one is armed.
    pub next_trial_unix_ms: Option<u64>,
}

/// Cross-node preflight result: every node's view of the configured snapshot.
#[derive(Debug, Clone)]
pub struct PreflightReport {
    /// One entry per node, in configured node order.
    pub nodes: Vec<PreflightNode>,
    /// True only when every node reported a digest and all digests are equal.
    pub digests_agree: bool,
    /// Human-readable caveats (missing digest, unavailable snapshot, ...).
    pub notes: Vec<String>,
}

/// One node's preflight result.
#[derive(Debug, Clone)]
pub struct PreflightNode {
    /// Controller base URL of the node.
    pub base_url: String,
    /// Whether the node lists the snapshot tag as ready and bootable.
    pub ready: bool,
    /// Snapshot digest reported by the node's detail endpoint, when present.
    pub digest: Option<String>,
    /// Snapshot lifecycle status reported by the node.
    pub status: String,
    /// Listing/detail failure, when the node could not be inspected.
    pub error: Option<String>,
}

/// Live sandboxes reported by one node (`list_all`).
#[derive(Debug, Clone)]
pub struct NodeSandboxes {
    /// Controller base URL of the node.
    pub base_url: String,
    /// Sandboxes the node reports; empty when the listing failed.
    pub sandboxes: Vec<SandboxInfo>,
    /// Listing failure, kept per node so one dead node cannot hide the rest.
    pub error: Option<String>,
}

/// RAII reservation of one in-flight slot on a node. `arm` reserves the slot
/// before the create request, `Drop` releases it saturating, and a failed
/// attempt rolls back by dropping the guard early.
struct InflightGuard {
    node: Arc<NodeState>,
    armed: bool,
}

impl InflightGuard {
    fn arm(node: Arc<NodeState>) -> Self {
        node.inflight.fetch_add(1, Ordering::AcqRel);
        Self { node, armed: true }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.armed {
            saturating_decrement(&self.node.inflight);
            self.armed = false;
        }
    }
}

/// A sandbox created through the cluster: the plain forkd sandbox plus the
/// node's in-flight reservation. Dropping it releases that reservation.
pub struct ClusterSandbox {
    inner: ForkdSandbox,
    _guard: InflightGuard,
}

impl ClusterSandbox {
    /// Return the sandbox identifier.
    pub fn id(&self) -> &str {
        self.inner.id()
    }
    /// Base URL of the controller node that owns this sandbox.
    pub fn node_base_url(&self) -> &str {
        self.inner.node_base_url()
    }
    /// Raw controller metadata for this sandbox.
    pub fn info(&self) -> &SandboxInfo {
        self.inner.info()
    }
    /// The wrapped forkd sandbox (guest access, node-scoped operations).
    pub fn inner(&self) -> &ForkdSandbox {
        &self.inner
    }
}

impl Sandbox for ClusterSandbox {
    fn backend(&self) -> BackendKind {
        self.inner.backend()
    }
    fn transport(&self) -> TransportKind {
        self.inner.transport()
    }
    fn capabilities(&self) -> &[Capability] {
        self.inner.capabilities()
    }
    fn exec<'a>(&'a self, spec: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        self.inner.exec(spec)
    }
    fn health<'a>(&'a self) -> BoxFuture<'a, Result<guest::Health, SandboxError>> {
        self.inner.health()
    }
    fn ping<'a>(&'a self) -> BoxFuture<'a, Result<guest::Health, SandboxError>> {
        self.inner.ping()
    }
    fn stream<'a>(
        &'a self,
        spec: guest::StreamSpec,
    ) -> BoxFuture<'a, Result<Box<dyn guest::GuestStream + 'a>, SandboxError>> {
        self.inner.stream(spec)
    }
    fn ls<'a>(
        &'a self,
        request: guest::LsRequest,
    ) -> BoxFuture<'a, Result<guest::LsResult, SandboxError>> {
        self.inner.ls(request)
    }
    fn find<'a>(
        &'a self,
        request: guest::FindRequest,
    ) -> BoxFuture<'a, Result<guest::FindResult, SandboxError>> {
        self.inner.find(request)
    }
    fn grep<'a>(
        &'a self,
        request: guest::GrepRequest,
    ) -> BoxFuture<'a, Result<guest::GrepResult, SandboxError>> {
        self.inner.grep(request)
    }
    fn read<'a>(
        &'a self,
        request: guest::ReadRequest,
    ) -> BoxFuture<'a, Result<guest::ReadResult, SandboxError>> {
        self.inner.read(request)
    }
    fn read_file<'a>(
        &'a self,
        request: guest::ReadRequest,
    ) -> BoxFuture<'a, Result<guest::ReadResult, SandboxError>> {
        self.inner.read_file(request)
    }
    fn write<'a>(
        &'a self,
        request: guest::WriteRequest,
    ) -> BoxFuture<'a, Result<guest::WriteResult, SandboxError>> {
        self.inner.write(request)
    }
    fn write_file<'a>(
        &'a self,
        request: guest::WriteRequest,
    ) -> BoxFuture<'a, Result<guest::WriteResult, SandboxError>> {
        self.inner.write_file(request)
    }
    fn eval<'a>(
        &'a self,
        request: guest::EvalRequest,
    ) -> BoxFuture<'a, Result<guest::EvalResult, SandboxError>> {
        self.inner.eval(request)
    }
    fn cancel<'a>(
        &'a self,
        request: guest::CancelRequest,
    ) -> BoxFuture<'a, Result<guest::CancelResult, SandboxError>> {
        self.inner.cancel(request)
    }
}

/// How one failed create attempt is treated.
enum CreateFault {
    /// The node itself is failing: transport error, 5xx, or an undecodable
    /// response. `reconcile` is true for the indeterminate variants, where
    /// the node may have created the sandbox anyway.
    NodeFault { reconcile: bool },
    /// The node answered 4xx: it refused this request. Propagate to the
    /// caller instead of retrying elsewhere (double-create risk).
    Rejected,
}

fn classify_create_error(error: &ForkdClientError) -> CreateFault {
    match error {
        ForkdClientError::Transport(_) | ForkdClientError::Decode(_) => {
            CreateFault::NodeFault { reconcile: true }
        }
        // A controller-request deadline is indeterminate: the node may have
        // created the sandbox anyway (it answers for the request's lifetime,
        // not ours).
        ForkdClientError::Timeout => CreateFault::NodeFault { reconcile: true },
        // forkd answered and refused outside HTTP status semantics: retrying
        // the same request elsewhere risks a second create.
        ForkdClientError::Remote(_) => CreateFault::Rejected,
        ForkdClientError::Http { status, .. } if status.is_client_error() => CreateFault::Rejected,
        ForkdClientError::Http { .. } => CreateFault::NodeFault { reconcile: false },
    }
}

impl ClusterProvider {
    /// Construct a cluster provider from the given node configs.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the node list or the snapshot tag is empty, or when
    /// any node URL is malformed.
    pub fn new(config: ClusterConfig) -> Result<Self, ProviderError> {
        if config.nodes.is_empty() {
            return Err(ProviderError::Unavailable(
                "cluster requires at least one node URL".into(),
            ));
        }
        if config.snapshot_tag.trim().is_empty() {
            return Err(ProviderError::Unavailable(
                "cluster requires a non-empty snapshot tag".into(),
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
            nodes.push(Arc::new(NodeState {
                client,
                inflight: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
                next_trial_ms: AtomicU64::new(0),
            }));
        }
        Ok(Self {
            nodes,
            snapshot_tag: config.snapshot_tag,
            strategy: config.strategy,
            cursor: Arc::new(AtomicUsize::new(0)),
            recovery_interval: config.recovery_interval,
        })
    }

    /// The half-open probe interval in milliseconds.
    fn interval_ms(&self) -> u64 {
        self.recovery_interval.as_millis().min(u128::from(u64::MAX)) as u64
    }

    /// Candidate order for one create: the shared cursor anchors a rotation
    /// (kept stable), least-inflight re-orders it by current load, and
    /// currently-gated nodes are dropped. A tripped node whose gate elapsed
    /// stays in the list; the probe budget is claimed when it is actually
    /// attempted (see [`Self::claim_trial`]).
    fn candidate_order(&self, now_ms: u64) -> Vec<usize> {
        let len = self.nodes.len();
        let anchor = self.cursor.fetch_add(1, Ordering::Relaxed) % len;
        let mut order: Vec<usize> = (0..len).map(|offset| (anchor + offset) % len).collect();
        if self.strategy == ScheduleStrategy::LeastInflight {
            // Stable sort: equal loads keep the rotation order, so ties are
            // settled fairly by the cursor instead of sticking to node 0.
            order.sort_by_key(|&index| self.nodes[index].inflight.load(Ordering::Acquire));
        }
        order.retain(|&index| admissible(&self.nodes[index], now_ms));
        order
    }

    /// Claim the right to attempt one create on a node. Healthy nodes are
    /// always admissible; a tripped node is only admissible when its gate
    /// elapsed, and then exactly one caller wins the CAS that pushes the gate
    /// one interval out (so a burst of creates probes at most once).
    fn claim_trial(&self, node: &NodeState, now_ms: u64) -> bool {
        if node.failures.load(Ordering::Acquire) < FAILURE_TRIP {
            return true;
        }
        let gate = node.next_trial_ms.load(Ordering::Acquire);
        if gate != 0 && now_ms < gate {
            return false;
        }
        node.next_trial_ms
            .compare_exchange(
                gate,
                now_ms.saturating_add(self.interval_ms()),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Per-node health/inflight snapshot for ops dashboards:
    /// `(base_url, healthy, inflight)` in node order.
    pub fn node_status(&self) -> Vec<(String, bool, usize)> {
        self.nodes
            .iter()
            .map(|node| {
                (
                    node.client.base_url().to_owned(),
                    node.failures.load(Ordering::Acquire) < FAILURE_TRIP,
                    node.inflight.load(Ordering::Acquire),
                )
            })
            .collect()
    }

    /// Full per-node status, including the failure counter and the half-open
    /// gate, in node order.
    pub fn node_statuses(&self) -> Vec<NodeStatus> {
        self.nodes
            .iter()
            .map(|node| {
                let failures = node.failures.load(Ordering::Acquire);
                let gate = node.next_trial_ms.load(Ordering::Acquire);
                NodeStatus {
                    base_url: node.client.base_url().to_owned(),
                    healthy: failures < FAILURE_TRIP,
                    inflight: node.inflight.load(Ordering::Acquire),
                    failures,
                    tripped: failures >= FAILURE_TRIP,
                    next_trial_unix_ms: (gate != 0).then_some(gate),
                }
            })
            .collect()
    }

    /// Cross-node consistency check for online/offline transitions: every
    /// node must list the configured snapshot tag as ready and bootable, and
    /// the reported snapshot digests must agree. A node that reports no
    /// digest is a note (older controllers omit it), never a hard failure.
    /// Read-only: no scheduling counter is touched.
    pub async fn preflight(&self) -> PreflightReport {
        let mut nodes = Vec::with_capacity(self.nodes.len());
        let mut notes = Vec::new();
        for node in &self.nodes {
            let mut entry = PreflightNode {
                base_url: node.client.base_url().to_owned(),
                ready: false,
                digest: None,
                status: String::new(),
                error: None,
            };
            match node.client.list_snapshots().await {
                Ok(snapshots) => match snapshots.iter().find(|s| s.tag == self.snapshot_tag) {
                    Some(snapshot) => {
                        entry.status = snapshot.status.clone();
                        entry.ready =
                            snapshot.status.eq_ignore_ascii_case("ready") && snapshot.bootable;
                        if !entry.ready {
                            notes.push(format!(
                                "{}: snapshot `{}` is not ready/bootable",
                                entry.base_url, self.snapshot_tag
                            ));
                        }
                    }
                    None => {
                        entry.error = Some(format!(
                            "snapshot tag `{}` is not provisioned",
                            self.snapshot_tag
                        ));
                        notes.push(format!(
                            "{}: snapshot tag `{}` is missing",
                            entry.base_url, self.snapshot_tag
                        ));
                    }
                },
                Err(error) => {
                    entry.error = Some(error.to_string());
                    notes.push(format!(
                        "{}: list_snapshots failed: {error}",
                        entry.base_url
                    ));
                }
            }
            match node.client.snapshot_info(&self.snapshot_tag).await {
                Ok(Some(info)) => {
                    if entry.status.is_empty() {
                        entry.status = info.status.clone();
                    }
                    entry.digest = info.digest.clone();
                }
                Ok(None) => notes.push(format!(
                    "{}: controller has no snapshot detail for `{}`",
                    entry.base_url, self.snapshot_tag
                )),
                Err(error) => {
                    if entry.error.is_none() {
                        entry.error = Some(error.to_string());
                    }
                    notes.push(format!("{}: snapshot_info failed: {error}", entry.base_url));
                }
            }
            nodes.push(entry);
        }
        // digests_agree = every node reported a digest and all are equal.
        let mut digests_agree = true;
        let mut expected: Option<String> = None;
        for node in &nodes {
            match node.digest.as_deref() {
                Some(digest) => match &expected {
                    None => expected = Some(digest.to_owned()),
                    Some(first) if first != digest => digests_agree = false,
                    _ => {}
                },
                None => {
                    digests_agree = false;
                    notes.push(format!("{}: no snapshot digest reported", node.base_url));
                }
            }
        }
        PreflightReport {
            nodes,
            digests_agree,
            notes,
        }
    }

    /// List live sandboxes on every node, one entry per node in node order.
    /// A node that cannot answer is reported with its error instead of
    /// hiding the other nodes.
    ///
    /// Typical orphan flow after an owner crash: `list_all` to find sandboxes
    /// nothing holds any more, then [`Self::delete_on`] with the node URL and
    /// sandbox id.
    pub async fn list_all(&self) -> Vec<NodeSandboxes> {
        let urls: Vec<String> = self
            .nodes
            .iter()
            .map(|node| node.client.base_url().to_owned())
            .collect();
        let handles: Vec<_> = self
            .nodes
            .iter()
            .map(|node| {
                let client = node.client.clone();
                tokio::spawn(async move { client.list_sandboxes().await })
            })
            .collect();
        let mut results = Vec::with_capacity(handles.len());
        for (base_url, handle) in urls.into_iter().zip(handles) {
            results.push(match handle.await {
                Ok(Ok(sandboxes)) => NodeSandboxes {
                    base_url,
                    sandboxes,
                    error: None,
                },
                Ok(Err(error)) => NodeSandboxes {
                    base_url,
                    sandboxes: Vec::new(),
                    error: Some(error.to_string()),
                },
                Err(error) => NodeSandboxes {
                    base_url,
                    sandboxes: Vec::new(),
                    error: Some(format!("list task failed: {error}")),
                },
            });
        }
        results
    }

    /// Delete one sandbox on the node named by `node_base_url` (exact match
    /// against the configured node URLs; unknown nodes are an error).
    ///
    /// This talks to the controller directly, so it also works for orphans of
    /// this process; it deliberately does not touch this process's in-flight
    /// counters (the owning handle's RAII guard does that if it still exists).
    ///
    /// # Errors
    ///
    /// Returns `Err` for an unknown node or a controller delete failure.
    pub async fn delete_on(
        &self,
        node_base_url: &str,
        sandbox_id: &str,
    ) -> Result<(), ProviderError> {
        let node = self
            .nodes
            .iter()
            .find(|node| node.client.base_url() == node_base_url)
            .ok_or_else(|| {
                ProviderError::Unavailable(format!("unknown cluster node {node_base_url}"))
            })?;
        node.client
            .delete_sandbox(sandbox_id)
            .await
            .map_err(|error| {
                ProviderError::Unavailable(format!("node {node_base_url} delete failed: {error}"))
            })
    }

    /// Probe every node's controller and reset failure counters and half-open
    /// gates on success. A failed probe adds no failure; it only re-arms the
    /// tripped node's gate. Returns one `Result` per node, in node order.
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
        let mut results = Vec::with_capacity(self.nodes.len());
        for handle in handles {
            results.push(
                handle
                    .await
                    .unwrap_or_else(|e| Err(format!("probe task failed: {e}"))),
            );
        }
        let now_ms = unix_millis();
        for (node, result) in self.nodes.iter().zip(&results) {
            if result.is_ok() {
                node.failures.store(0, Ordering::Release);
                node.next_trial_ms.store(0, Ordering::Release);
            } else if node.failures.load(Ordering::Acquire) >= FAILURE_TRIP {
                // Keep the tripped node out until at least one interval from
                // this failed probe.
                node.next_trial_ms
                    .fetch_max(now_ms.saturating_add(self.interval_ms()), Ordering::AcqRel);
            }
        }
        results
    }

    /// Create one sandbox with a default (empty) [`SandboxSpec`].
    ///
    /// # Errors
    ///
    /// Returns `Err` when every node attempt fails (the last error is kept).
    pub async fn create_sandbox(&self) -> Result<ClusterSandbox, ProviderError> {
        self.create_sandbox_with(&SandboxSpec::default()).await
    }

    /// Create one sandbox honoring `spec`: the spec is validated fail-closed
    /// (capabilities and the shared create-resource guard) before any request,
    /// and a `resources.memory_bytes` ceiling is injected as the node's
    /// per-sandbox `memory_limit_mib`.
    ///
    /// Scheduling is a reservation, not a guess: the candidate node's
    /// in-flight slot is taken *before* the request, so concurrent creates
    /// count it as busy; the returned [`ClusterSandbox`] owns the slot and
    /// releases it on drop.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the spec is unsupported, when a node rejects the
    /// request with 4xx (propagated immediately), or when every attempted
    /// node fails ("all cluster nodes are unhealthy" when no node was even
    /// admissible).
    pub async fn create_sandbox_with(
        &self,
        spec: &SandboxSpec,
    ) -> Result<ClusterSandbox, ProviderError> {
        check_create_spec(spec, self.capabilities())?;
        let memory_limit_mib = check_create_resources(spec, true)?;
        let now_ms = unix_millis();
        let candidates = self.candidate_order(now_ms);
        let mut last: Option<ProviderError> = None;
        for index in candidates {
            let node = Arc::clone(&self.nodes[index]);
            if !self.claim_trial(&node, now_ms) {
                // Another create claimed this half-open probe; leave the
                // node gated for this attempt.
                continue;
            }
            // Reserve the slot before the request so concurrent creates see
            // the node as busy; the guard rolls back on failure.
            let guard = InflightGuard::arm(Arc::clone(&node));
            let attempt_started_unix = unix_secs();
            let mut request = CreateSandboxRequest::single(&self.snapshot_tag);
            request.memory_limit_mib = memory_limit_mib;
            match node.client.create(&request).await {
                Ok(inner) => {
                    node.failures.store(0, Ordering::Release);
                    node.next_trial_ms.store(0, Ordering::Release);
                    return Ok(ClusterSandbox {
                        inner,
                        _guard: guard,
                    });
                }
                Err(error) => {
                    // Roll the reservation back before classifying/handling.
                    drop(guard);
                    match classify_create_error(&error) {
                        CreateFault::Rejected => {
                            return Err(ProviderError::Unavailable(format!(
                                "node {} rejected create: {error}",
                                node.client.base_url()
                            )));
                        }
                        CreateFault::NodeFault { reconcile } => {
                            let failures = node.failures.fetch_add(1, Ordering::AcqRel) + 1;
                            if failures >= FAILURE_TRIP {
                                // Trip now, or (already tripped) push the next
                                // lazy probe out one interval.
                                node.next_trial_ms.fetch_max(
                                    now_ms.saturating_add(self.interval_ms()),
                                    Ordering::AcqRel,
                                );
                            }
                            if reconcile {
                                self.reconcile(&node, attempt_started_unix).await;
                            }
                            last = Some(ProviderError::Unavailable(format!(
                                "node {} failed: {error}",
                                node.client.base_url()
                            )));
                        }
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            ProviderError::Unavailable("all cluster nodes are unhealthy".into())
        }))
    }

    /// After an indeterminate create failure (transport error or undecodable
    /// response), ask the node what it actually created and delete a unique
    /// orphan. "Prefer a miss over a false delete": the sandbox must match the
    /// snapshot tag and have been created inside this attempt's window
    /// (`attempt_started_unix - 1s ..= now + 1s`); no match, several matches,
    /// a missing timestamp, or a failed listing are only reported.
    ///
    /// Assumes this process is the node's only client: a concurrently created
    /// sandbox that matches tag and window would be treated as this attempt's
    /// orphan.
    async fn reconcile(&self, node: &NodeState, attempt_started_unix: i64) {
        let now_unix = unix_secs();
        let sandboxes = match node.client.list_sandboxes().await {
            Ok(sandboxes) => sandboxes,
            Err(error) => {
                eprintln!(
                    "rfb cluster: {} unreachable for post-failure reconciliation: {error}",
                    node.client.base_url()
                );
                return;
            }
        };
        let mut matches = sandboxes.iter().filter(|sandbox| {
            sandbox.snapshot_tag == self.snapshot_tag
                && sandbox.created_at_unix.is_some_and(|created| {
                    created >= attempt_started_unix - 1 && created <= now_unix + 1
                })
        });
        let first = matches.next();
        match (first, matches.next()) {
            (Some(orphan), None) => {
                if let Err(error) = node.client.delete_sandbox(&orphan.id).await {
                    eprintln!(
                        "rfb cluster: failed to delete orphan {} on {}: {error}",
                        orphan.id,
                        node.client.base_url()
                    );
                }
            }
            (Some(_), Some(_)) => eprintln!(
                "rfb cluster: {} lists several sandboxes matching tag `{}` and the create window; leaving them alone",
                node.client.base_url(),
                self.snapshot_tag
            ),
            (None, _) => {}
        }
    }

    /// Report that a previously created sandbox ended (close/destroy), so
    /// least-inflight scheduling sees the true load.
    ///
    /// Prefer dropping the [`ClusterSandbox`] returned by
    /// [`Self::create_sandbox_with`] — its RAII guard releases automatically.
    /// This compatibility path is saturating, so a double release can never
    /// underflow the counter.
    pub fn release(&self, sandbox: &ForkdSandbox) {
        let url = sandbox.node_base_url();
        if let Some(node) = self.nodes.iter().find(|n| n.client.base_url() == url) {
            saturating_decrement(&node.inflight);
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
        Box::pin(
            async move { Ok(Box::new(self.create_sandbox_with(&spec).await?) as Box<dyn Sandbox>) },
        )
    }
}

/// Whether a node may be scheduled onto right now. A tripped node (failures
/// at or above [`FAILURE_TRIP`]) is skipped until its half-open gate elapses;
/// a gate of zero means none is armed.
fn admissible(node: &NodeState, now_ms: u64) -> bool {
    if node.failures.load(Ordering::Acquire) < FAILURE_TRIP {
        return true;
    }
    let gate = node.next_trial_ms.load(Ordering::Acquire);
    gate == 0 || now_ms >= gate
}

/// Decrement without wrapping: a stray extra release leaves zero at zero.
fn saturating_decrement(counter: &AtomicUsize) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        Some(value.saturating_sub(1))
    });
}

/// Wall-clock milliseconds since the Unix epoch (0 before it).
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Wall-clock seconds since the Unix epoch (0 before it).
fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}
