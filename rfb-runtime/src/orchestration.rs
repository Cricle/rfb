//! Host-side runtime orchestration: provisioning, deduplication, timeouts and cleanup.
//!
//! [`RuntimeManager`](crate::orchestration::RuntimeManager) is the reusable
//! entry point. It ensures at most one worker is provisioned per session
//! (concurrent [`RuntimeManager::create_for_session`](crate::orchestration::RuntimeManager::create_for_session)
//! calls share a single provision), applies `provision_timeout_seconds`, and
//! retries `cancel`/`destroy` cleanup within `cleanup_timeout_seconds`.
//!
//! The backend is injected as an
//! [`RuntimeWorkerAdapter`](crate::orchestration::RuntimeWorkerAdapter); tests substitute a
//! fake adapter so the lifecycle can be exercised without a real runtime.
//!
//! ```
//! # use std::sync::Arc;
//! use rfb_runtime::orchestration::{RuntimeBackend, RuntimeManager, RuntimeSpec,
//!     RuntimeStatus, RuntimeWorkerAdapter, WorkerHandle, RuntimeError};
//!
//! struct Noop;
//! #[async_trait::async_trait]
//! impl RuntimeWorkerAdapter for Noop {
//!     async fn provision(&self, _spec: RuntimeSpec, _worker_id: String)
//!         -> Result<WorkerHandle, RuntimeError> {
//!         Ok(WorkerHandle { worker_id: "w1".into(), workspace: None,
//!             guest_workspace: None, guest_address: None, sandbox_id: None })
//!     }
//!     async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> { Ok(()) }
//!     async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> { Ok(()) }
//! }
//!
//! let runtime = tokio::runtime::Builder::new_current_thread()
//!     .enable_time()
//!     .build()
//!     .unwrap();
//! let mut spec = RuntimeSpec::default();
//! spec.backend = RuntimeBackend::Fake;
//! let manager = RuntimeManager::with_adapter(spec, Arc::new(Noop));
//! runtime.block_on(async {
//!     let handle = manager.create_for_session("s1").await;
//!     assert_eq!(handle.status, RuntimeStatus::Running);
//!     assert!(manager.cancel_for_session("s1").await.is_some());
//!     assert!(manager.destroy_for_session("s1").await.is_some());
//!     assert!(manager.get("s1").await.is_none());
//! });
//! ```
use async_trait::async_trait;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::RwLock, task::JoinHandle};

/// Backend used to host a runtime worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeBackend {
    #[default]
    /// The forkd guest-agent backend.
    Forkd,
    /// The Firecracker vsock backend.
    Vsock,
    /// The ZeroBoot ZBRT backend: one Firecracker VM per sandbox, no network.
    Zeroboot,
    /// The in-process fake backend.
    Fake,
    /// No supported backend is selected.
    Unsupported,
}
/// A fully-resolved runtime worker specification.
///
/// Every field is required; [`RuntimeSpec::default`] supplies the reference
/// defaults so callers only override what they need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSpec {
    /// Backend used to provision workers.
    pub backend: RuntimeBackend,
    /// Backend operating mode.
    pub mode: String,
    /// Optional host template directory.
    pub template_dir: Option<PathBuf>,
    /// Optional runtime executable path.
    pub runtime_binary: Option<PathBuf>,
    /// Snapshot identifier used by the backend.
    pub snapshot_tag: String,
    /// Guest memory in MiB.
    pub memory_mb: u32,
    /// Guest disk size in MiB.
    pub disk_mb: u32,
    /// Number of virtual CPUs.
    pub vcpu: u8,
    /// Maximum turn runtime in seconds.
    pub max_runtime_seconds: u64,
    /// Provisioning timeout in seconds.
    pub provision_timeout_seconds: u64,
    /// Cleanup timeout in seconds.
    pub cleanup_timeout_seconds: u64,
    /// Maximum cleanup attempts.
    pub cleanup_max_retries: u32,
    /// Delay between cleanup attempts in milliseconds.
    pub cleanup_backoff_ms: u64,
}
impl Default for RuntimeSpec {
    fn default() -> Self {
        Self {
            backend: Default::default(),
            mode: "auto".into(),
            template_dir: None,
            runtime_binary: None,
            snapshot_tag: "rfb".into(),
            memory_mb: 32,
            disk_mb: 32,
            vcpu: 1,
            max_runtime_seconds: 1800,
            provision_timeout_seconds: 30,
            cleanup_timeout_seconds: 10,
            cleanup_max_retries: 2,
            cleanup_backoff_ms: 100,
        }
    }
}
/// A single running (or failed) runtime worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHandle {
    /// Backend-assigned worker identifier.
    pub worker_id: String,
    /// Optional host-side workspace path.
    pub workspace: Option<PathBuf>,
    /// Optional guest workspace path.
    pub guest_workspace: Option<String>,
    /// Optional guest transport address.
    pub guest_address: Option<String>,
    /// Optional sandbox identifier.
    pub sandbox_id: Option<String>,
}
/// Errors raised while provisioning or cleaning up a runtime worker.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// The adapter reported a provisioning failure.
    #[error("runtime provisioning failed: {0}")]
    Provisioning(String),
    /// Provisioning did not complete within `provision_timeout_seconds`.
    #[error("runtime provisioning timed out after {0} seconds")]
    Timeout(u64),
    /// The adapter itself failed (e.g. a cancelled or errored cleanup op).
    #[error("runtime adapter failed: {0}")]
    Adapter(String),
}
/// A host-side adapter that provisions and tears down runtime workers.
///
/// Implementations hide the concrete backend (forkd, vsock, fake); the manager
/// only depends on this interface and never on a specific runtime.
#[async_trait]
pub trait RuntimeWorkerAdapter: Send + Sync {
    /// Provision a worker for the supplied specification.
    async fn provision(
        &self,
        spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError>;
    /// Cancel work on a provisioned worker.
    async fn cancel(&self, worker: &WorkerHandle) -> Result<(), RuntimeError>;
    /// Destroy a provisioned worker and release its resources.
    async fn destroy(&self, worker: &WorkerHandle) -> Result<(), RuntimeError>;
}
/// Lifecycle state of a runtime worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    /// A worker is being provisioned for this session.
    Provisioning,
    /// The worker is provisioned and can accept work.
    Running,
    /// A cancel request is in flight.
    Cancelling,
    /// The worker has been stopped or torn down.
    Stopped,
    /// Provisioning or a worker operation failed.
    Failed,
}
/// A session-scoped handle to a runtime worker plus its lifecycle state.
#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    /// Session this handle is scoped to.
    pub session_id: String,
    /// Backend-assigned virtual machine identifier.
    pub vm_id: String,
    /// Backend this worker runs on.
    pub backend: RuntimeBackend,
    /// Current lifecycle state.
    pub status: RuntimeStatus,
    /// Optional failure detail when `status` is [`RuntimeStatus::Failed`].
    pub error: Option<String>,
    /// Host-side workspace, once the worker is provisioned.
    pub workspace: Option<PathBuf>,
    /// The underlying worker, hidden so lifecycle state cannot drift.
    worker: Option<WorkerHandle>,
}
impl RuntimeHandle {
    /// Access the underlying worker once provisioned.
    pub fn worker(&self) -> Option<&WorkerHandle> {
        self.worker.as_ref()
    }
}

/// A reusable, backend-independent host runtime manager.
#[derive(Clone)]
pub struct RuntimeManager {
    spec: RuntimeSpec,
    adapter: Arc<dyn RuntimeWorkerAdapter>,
    sessions: Arc<RwLock<HashMap<String, RuntimeHandle>>>,
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Detached provisioning tasks retained until they finish after timeout.
    late_workers: Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>,
    /// Per-session lifecycle generations; invalidates only that session's stale completions.
    generations: Arc<RwLock<HashMap<String, u64>>>,
}
impl RuntimeManager {
    /// Construct a manager with an injected worker adapter.
    pub fn with_adapter(spec: RuntimeSpec, adapter: Arc<dyn RuntimeWorkerAdapter>) -> Self {
        Self {
            spec,
            adapter,
            sessions: Default::default(),
            lock: Default::default(),
            late_workers: Default::default(),
            generations: Default::default(),
        }
    }
    /// The configured runtime specification.
    pub fn spec(&self) -> RuntimeSpec {
        self.spec.clone()
    }
    /// Fetch the current handle for a session, if any.
    pub async fn get(&self, id: &str) -> Option<RuntimeHandle> {
        self.sessions.read().await.get(id).cloned()
    }
    /// Provision at most one worker for a session; concurrent callers share the placeholder/result.
    pub async fn create_for_session(&self, id: &str) -> RuntimeHandle {
        if let Some(h) = self.get(id).await {
            return h;
        }
        let generation = self.generations.read().await.get(id).copied().unwrap_or(1);
        let vm_id = format!("vm-{}", next_id());
        let placeholder = RuntimeHandle {
            session_id: id.into(),
            vm_id: vm_id.clone(),
            backend: self.spec.backend,
            status: RuntimeStatus::Provisioning,
            error: None,
            workspace: None,
            worker: None,
        };
        {
            let mut s = self.sessions.write().await;
            if let Some(h) = s.get(id) {
                return h.clone();
            }
            s.insert(id.into(), placeholder.clone());
        }
        let adapter = self.adapter.clone();
        let spec = self.spec.clone();
        let mut task = tokio::spawn(async move { adapter.provision(spec, vm_id).await });
        let result = tokio::time::timeout(
            Duration::from_secs(self.spec.provision_timeout_seconds.max(1)),
            &mut task,
        )
        .await;
        let mut h = placeholder;
        match result {
            Err(_) => {
                h.status = RuntimeStatus::Failed;
                h.error = Some(format!(
                    "runtime provisioning timed out after {} seconds",
                    self.spec.provision_timeout_seconds.max(1)
                ));
                // Keep the task alive: a late worker must be destroyed, not leaked.
                let adapter = self.adapter.clone();
                let reaper = tokio::spawn(async move {
                    if let Ok(Ok(worker)) = task.await {
                        let _ = cleanup_with_retry(10, 2, 100, || adapter.destroy(&worker)).await;
                    }
                });
                self.late_workers.lock().await.push(reaper);
            }
            Ok(inner) => {
                let result = inner.unwrap_or_else(|_| {
                    Err(RuntimeError::Adapter("provision task cancelled".into()))
                });
                match result {
                    Ok(w) => {
                        h.status = RuntimeStatus::Running;
                        h.workspace = w.workspace.clone();
                        h.worker = Some(w);
                    }
                    Err(e) => {
                        h.status = RuntimeStatus::Failed;
                        h.error = Some(e.to_string());
                    }
                }
            }
        }
        let mut s = self.sessions.write().await;
        // A cancel/destroy may have replaced this owner while provisioning ran.
        if self.generations.read().await.get(id).copied().unwrap_or(1) == generation
            && s.get(id).map(|x| x.vm_id.as_str()) == Some(h.vm_id.as_str())
        {
            s.insert(id.into(), h.clone());
        } else if let Some(w) = h.worker.take() {
            let adapter = self.adapter.clone();
            tokio::spawn(async move {
                let _ = cleanup_with_retry(10, 2, 100, || adapter.destroy(&w)).await;
            });
        }
        h
    }
    /// Cancel a running worker and retain its stopped handle for inspection.
    pub async fn cancel_for_session(&self, id: &str) -> Option<RuntimeHandle> {
        let _g = self.lock.lock().await;
        {
            let mut generations = self.generations.write().await;
            let generation = generations.entry(id.to_owned()).or_insert(1);
            *generation = generation.wrapping_add(1);
        }
        let w = self.sessions.read().await.get(id)?.worker.clone();
        {
            let mut s = self.sessions.write().await;
            s.get_mut(id)?.status = RuntimeStatus::Cancelling;
        }
        let cleanup_error = if let Some(w) = w {
            cleanup_with_retry(
                self.spec.cleanup_timeout_seconds,
                self.spec.cleanup_max_retries,
                self.spec.cleanup_backoff_ms,
                || self.adapter.cancel(&w),
            )
            .await
            .err()
        } else {
            None
        };
        let mut s = self.sessions.write().await;
        let h = s.get_mut(id)?;
        h.status = if cleanup_error.is_some() {
            RuntimeStatus::Failed
        } else {
            RuntimeStatus::Stopped
        };
        h.error = cleanup_error.map(|e| e.to_string());
        Some(h.clone())
    }
    /// Destroy a worker and remove its session, retrying cleanup within the deadline.
    pub async fn destroy_for_session(&self, id: &str) -> Option<RuntimeHandle> {
        let _g = self.lock.lock().await;
        {
            let mut generations = self.generations.write().await;
            let generation = generations.entry(id.to_owned()).or_insert(1);
            *generation = generation.wrapping_add(1);
        }
        let mut h = self.sessions.write().await.remove(id)?;
        if let Some(w) = &h.worker {
            if let Err(e) = cleanup_with_retry(
                self.spec.cleanup_timeout_seconds,
                self.spec.cleanup_max_retries,
                self.spec.cleanup_backoff_ms,
                || self.adapter.destroy(w),
            )
            .await
            {
                h.status = RuntimeStatus::Failed;
                h.error = Some(e.to_string());
            } else {
                h.status = RuntimeStatus::Stopped;
            }
        }
        Some(h)
    }
}

fn next_id() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ID: AtomicU64 = AtomicU64::new(1);
    ID.fetch_add(1, Ordering::Relaxed) as u128
}

/// Retry a cleanup operation until it succeeds or the deadline expires.
///
/// `op` is invoked up to `max_retries + 1` times. Between attempts it sleeps
/// with exponential backoff starting at `backoff_ms` (doubling each retry, up
/// to 64x). The whole sequence is bounded by `timeout_seconds` (minimum 1): once
/// the deadline passes the last error is returned, or [`RuntimeError::Timeout`]
/// if the final attempt was still in flight.
///
/// This is used for both `cancel` and `destroy` so a transiently failing
/// cleanup never leaves a session stuck in a non-terminal state.
pub async fn cleanup_with_retry<F, Fut>(
    timeout_seconds: u64,
    max_retries: u32,
    backoff_ms: u64,
    mut op: F,
) -> Result<(), RuntimeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), RuntimeError>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    let mut last = None;
    for i in 0..=max_retries {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, op()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => last = Some(e),
            Err(_) => last = Some(RuntimeError::Timeout(timeout_seconds)),
        }
        if i < max_retries {
            let backoff = Duration::from_millis(backoff_ms.saturating_mul(1u64 << (i.min(6))));
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(backoff.min(remaining)).await
        }
    }
    Err(last.unwrap_or_else(|| RuntimeError::Adapter("cleanup failed".into())))
}
