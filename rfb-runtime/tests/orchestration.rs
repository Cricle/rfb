//! Host-side orchestration tests: provisioning, per-session deduplication,
//! success/failure/timeout lifecycle paths, and cleanup retry semantics. All
//! tests inject fake adapters so no real runtime or VM is required.

use async_trait::async_trait;
use rfb_runtime::orchestration::{
    cleanup_with_retry, RuntimeBackend, RuntimeError, RuntimeManager, RuntimeSpec, RuntimeStatus,
    RuntimeWorkerAdapter, WorkerHandle,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

fn worker(worker_id: String) -> WorkerHandle {
    WorkerHandle {
        worker_id,
        workspace: None,
        guest_workspace: None,
        guest_address: None,
        sandbox_id: None,
    }
}

#[test]
fn runtime_spec_is_public_contract() {
    let spec = RuntimeSpec::default();
    assert_eq!(spec.backend, RuntimeBackend::Forkd);
    assert_eq!(spec.memory_mb, 32);
    assert_eq!(spec.provision_timeout_seconds, 30);
}

#[derive(Default)]
struct NoopAdapter {
    provisions: AtomicUsize,
    cancels: AtomicUsize,
    destroys: AtomicUsize,
}

#[async_trait]
impl RuntimeWorkerAdapter for NoopAdapter {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        self.provisions.fetch_add(1, Ordering::SeqCst);
        Ok(worker(worker_id))
    }
    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        self.destroys.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn success_lifecycle_provisions_runs_and_cleans_up() {
    let adapter = Arc::new(NoopAdapter::default());
    let manager = RuntimeManager::with_adapter(RuntimeSpec::default(), adapter.clone());

    let handle = manager.create_for_session("s1").await;
    assert_eq!(handle.session_id, "s1");
    assert_eq!(handle.status, RuntimeStatus::Running);
    assert!(handle.worker().is_some());
    assert_eq!(
        manager.get("s1").await.unwrap().status,
        RuntimeStatus::Running
    );

    let cancelled = manager.cancel_for_session("s1").await.unwrap();
    assert_eq!(cancelled.status, RuntimeStatus::Stopped);
    assert!(cancelled.error.is_none());
    assert_eq!(adapter.cancels.load(Ordering::SeqCst), 1);
    // The cancelled worker is retained for inspection, then destroyed.
    assert!(manager.get("s1").await.is_some());

    let destroyed = manager.destroy_for_session("s1").await.unwrap();
    assert_eq!(destroyed.status, RuntimeStatus::Stopped);
    assert!(manager.get("s1").await.is_none());
    assert_eq!(adapter.destroys.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn create_is_idempotent_without_reprovisioning() {
    let adapter = Arc::new(NoopAdapter::default());
    let manager = RuntimeManager::with_adapter(RuntimeSpec::default(), adapter.clone());
    let first = manager.create_for_session("same").await;
    let second = manager.create_for_session("same").await;
    assert_eq!(first.vm_id, second.vm_id);
    assert_eq!(adapter.provisions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_create_globally_deduplicates_provisioning() {
    let adapter = Arc::new(NoopAdapter::default());
    let manager = RuntimeManager::with_adapter(RuntimeSpec::default(), adapter.clone());
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let manager = manager.clone();
        tasks.push(tokio::spawn(async move {
            manager.create_for_session("dedup").await
        }));
    }
    let mut vm_ids = Vec::new();
    for task in tasks {
        let handle = task.await.unwrap();
        assert_eq!(handle.session_id, "dedup");
        vm_ids.push(handle.vm_id);
    }
    vm_ids.dedup();
    assert_eq!(
        vm_ids.len(),
        1,
        "all callers must share one worker for a session"
    );
    assert_eq!(adapter.provisions.load(Ordering::SeqCst), 1);
    assert_eq!(
        manager.get("dedup").await.unwrap().status,
        RuntimeStatus::Running
    );
}

#[derive(Clone)]
struct GatedProvision {
    ready: Arc<Notify>,
    provisions: Arc<AtomicUsize>,
    destroys: Arc<AtomicUsize>,
}

#[async_trait]
impl RuntimeWorkerAdapter for GatedProvision {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        self.provisions.fetch_add(1, Ordering::SeqCst);
        self.ready.notified().await;
        Ok(worker(worker_id))
    }
    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        self.destroys.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn interleaved_sessions_isolate_provisioning_generations() {
    let adapter = Arc::new(GatedProvision {
        ready: Arc::new(Notify::new()),
        provisions: Arc::new(AtomicUsize::new(0)),
        destroys: Arc::new(AtomicUsize::new(0)),
    });
    let manager = RuntimeManager::with_adapter(RuntimeSpec::default(), adapter.clone());
    let a_manager = manager.clone();
    let b_manager = manager.clone();
    let a = tokio::spawn(async move { a_manager.create_for_session("session-a").await });
    let b = tokio::spawn(async move { b_manager.create_for_session("session-b").await });
    tokio::task::yield_now().await;
    while adapter.provisions.load(Ordering::SeqCst) != 2 {
        tokio::task::yield_now().await;
    }

    let cancelled = manager.cancel_for_session("session-b").await.unwrap();
    assert_eq!(cancelled.status, RuntimeStatus::Stopped);
    adapter.ready.notify_waiters();
    adapter.ready.notify_waiters();
    let a_handle = a.await.unwrap();
    let b_handle = b.await.unwrap();
    assert_eq!(a_handle.status, RuntimeStatus::Running);
    assert_eq!(
        manager.get("session-a").await.unwrap().status,
        RuntimeStatus::Running
    );
    assert_eq!(b_handle.status, RuntimeStatus::Running);
    assert_eq!(
        manager.get("session-b").await.unwrap().status,
        RuntimeStatus::Stopped
    );
    assert_eq!(adapter.destroys.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn get_returns_none_before_any_provisioning() {
    let manager =
        RuntimeManager::with_adapter(RuntimeSpec::default(), Arc::new(NoopAdapter::default()));
    assert!(manager.get("ghost").await.is_none());
}

struct FailingProvision;

#[async_trait]
impl RuntimeWorkerAdapter for FailingProvision {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        _worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        Err(RuntimeError::Provisioning("no capacity".into()))
    }
    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn provision_failure_is_recorded_and_not_retried() {
    let manager = RuntimeManager::with_adapter(RuntimeSpec::default(), Arc::new(FailingProvision));
    let handle = manager.create_for_session("fail").await;
    assert_eq!(handle.status, RuntimeStatus::Failed);
    assert!(handle.error.unwrap().contains("no capacity"));
    // A later create returns the recorded failure rather than re-provisioning.
    let again = manager.create_for_session("fail").await;
    assert_eq!(again.status, RuntimeStatus::Failed);
}

struct HangingProvision;

#[async_trait]
impl RuntimeWorkerAdapter for HangingProvision {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        _worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        std::future::pending().await
    }
    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn provision_timeout_fails_closed_within_the_deadline() {
    let spec = RuntimeSpec {
        provision_timeout_seconds: 1,
        ..RuntimeSpec::default()
    };
    let manager = RuntimeManager::with_adapter(spec, Arc::new(HangingProvision));
    let start = std::time::Instant::now();
    let handle = manager.create_for_session("slow").await;
    assert_eq!(handle.status, RuntimeStatus::Failed);
    assert!(handle.error.unwrap().contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(5));
}

struct FlakyCancel {
    attempts: AtomicUsize,
    fail_first: usize,
}

#[async_trait]
impl RuntimeWorkerAdapter for FlakyCancel {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        Ok(worker(worker_id))
    }
    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) < self.fail_first {
            Err(RuntimeError::Adapter("transient cancel failure".into()))
        } else {
            Ok(())
        }
    }
    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn cancel_retries_transient_failures_until_success() {
    let adapter = Arc::new(FlakyCancel {
        attempts: AtomicUsize::new(0),
        fail_first: 2,
    });
    let spec = RuntimeSpec {
        cleanup_timeout_seconds: 5,
        cleanup_max_retries: 5,
        cleanup_backoff_ms: 1,
        ..RuntimeSpec::default()
    };
    let manager = RuntimeManager::with_adapter(spec, adapter.clone());
    let handle = manager.create_for_session("flaky").await;
    assert_eq!(handle.status, RuntimeStatus::Running);
    let cancelled = manager.cancel_for_session("flaky").await.unwrap();
    assert_eq!(cancelled.status, RuntimeStatus::Stopped);
    assert!(cancelled.error.is_none());
    assert_eq!(adapter.attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cleanup_with_retry_succeeds_on_the_first_attempt() {
    let result = cleanup_with_retry(1, 3, 1, || async { Ok(()) }).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn cleanup_with_retry_exhausts_retries_and_returns_the_last_error() {
    let result = cleanup_with_retry(2, 2, 1, || async {
        Err(RuntimeError::Adapter("always broken".into()))
    })
    .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("always broken"));
}

#[tokio::test]
async fn cleanup_with_retry_times_out_when_the_op_never_resolves() {
    let start = std::time::Instant::now();
    let result = cleanup_with_retry(1, 0, 0, || {
        std::future::pending::<Result<(), RuntimeError>>()
    })
    .await;
    assert!(matches!(result, Err(RuntimeError::Timeout(_))));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cleanup_with_retry_backoff_never_sleeps_past_the_deadline() {
    let start = std::time::Instant::now();
    let result = cleanup_with_retry(1, 4, 5_000, || async {
        Err(RuntimeError::Adapter("transient".into()))
    })
    .await;
    assert!(result.is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
}
