use async_trait::async_trait;
use rfb_runtime::orchestration::{
    cleanup_with_retry, RuntimeError, RuntimeManager, RuntimeSpec, RuntimeStatus,
    RuntimeWorkerAdapter, WorkerHandle,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct FailingCleanup;

#[async_trait]
impl RuntimeWorkerAdapter for FailingCleanup {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        Ok(WorkerHandle {
            worker_id,
            workspace: None,
            guest_workspace: None,
            guest_address: None,
            sandbox_id: None,
        })
    }

    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Err(RuntimeError::Adapter("cancel failed".into()))
    }

    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Err(RuntimeError::Adapter("destroy failed".into()))
    }
}

#[tokio::test]
async fn retry_backoff_never_sleeps_past_deadline() {
    let started = Instant::now();
    let result = cleanup_with_retry(1, 3, 5_000, || async {
        Err(RuntimeError::Adapter("transient".into()))
    })
    .await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn cancel_and_destroy_surface_cleanup_errors() {
    let manager = RuntimeManager::with_adapter(
        RuntimeSpec {
            cleanup_timeout_seconds: 1,
            cleanup_max_retries: 0,
            ..RuntimeSpec::default()
        },
        Arc::new(FailingCleanup),
    );
    let handle = manager.create_for_session("cancel").await;
    assert_eq!(handle.status, RuntimeStatus::Running);
    let cancelled = manager.cancel_for_session("cancel").await.unwrap();
    assert_eq!(cancelled.status, RuntimeStatus::Failed);
    assert!(cancelled.error.unwrap().contains("cancel failed"));

    let _ = manager.create_for_session("destroy").await;
    let destroyed = manager.destroy_for_session("destroy").await.unwrap();
    assert_eq!(destroyed.status, RuntimeStatus::Failed);
    assert!(destroyed.error.unwrap().contains("destroy failed"));
    assert!(manager.get("destroy").await.is_none());
}

struct FlakyDestroy {
    attempts: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl RuntimeWorkerAdapter for FlakyDestroy {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        Ok(WorkerHandle {
            worker_id,
            workspace: None,
            guest_workspace: None,
            guest_address: None,
            sandbox_id: None,
        })
    }
    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        if self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            Err(RuntimeError::Adapter("transient destroy failure".into()))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn destroy_retries_transient_failure_and_reaches_stopped() {
    let adapter = Arc::new(FlakyDestroy {
        attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    let manager = RuntimeManager::with_adapter(
        RuntimeSpec {
            cleanup_max_retries: 2,
            cleanup_backoff_ms: 1,
            ..RuntimeSpec::default()
        },
        adapter.clone(),
    );
    manager.create_for_session("destroy-retry").await;
    let result = manager.destroy_for_session("destroy-retry").await.unwrap();
    assert_eq!(result.status, RuntimeStatus::Stopped);
    assert_eq!(
        adapter.attempts.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert!(manager.get("destroy-retry").await.is_none());
}

#[tokio::test]
async fn repeated_cancel_and_destroy_are_terminal_and_idempotent() {
    let manager = RuntimeManager::with_adapter(RuntimeSpec::default(), Arc::new(FailingCleanup));
    manager.create_for_session("terminal").await;
    let cancelled = manager.cancel_for_session("terminal").await.unwrap();
    assert_eq!(cancelled.status, RuntimeStatus::Failed);
    let cancelled_again = manager.cancel_for_session("terminal").await.unwrap();
    assert_eq!(cancelled_again.status, RuntimeStatus::Failed);
    let destroyed = manager.destroy_for_session("terminal").await.unwrap();
    assert_eq!(destroyed.status, RuntimeStatus::Failed);
    assert!(manager.destroy_for_session("terminal").await.is_none());
}
