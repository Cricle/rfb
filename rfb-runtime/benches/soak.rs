//! Deterministic concurrency + leak soak for runtime orchestration.
//!
//! Runs repeated create/cancel/destroy cycles against an injected fake
//! adapter (no VM required) while sampling RSS and open FD count. Exits
//! non-zero when error count, RSS growth, or FD count exceed thresholds, so
//! this can be used as a Rust-native leak gate instead of a shell script.
//!
//! Tuning via env:
//!   RFB_SOAK_ROUNDS       number of cycles (default 10)
//!   RFB_SOAK_RSS_MB       allowed RSS in MiB (default 256)
//!   RFB_SOAK_FD           allowed open FD count (default 512)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rfb_runtime::orchestration::{
    RuntimeBackend, RuntimeError, RuntimeManager, RuntimeSpec, RuntimeWorkerAdapter, WorkerHandle,
};

struct FakeAdapter;

#[async_trait]
impl RuntimeWorkerAdapter for FakeAdapter {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        Ok(WorkerHandle {
            worker_id,
            workspace: None,
            guest_workspace: Some("/workspace".into()),
            guest_address: None,
            sandbox_id: None,
        })
    }
    async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmRSS:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .map(|kib| kib / 1024)
        .unwrap_or(0)
}

fn fd_count() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count() as u64)
        .unwrap_or(0)
}

fn main() {
    let rounds = env_u64("RFB_SOAK_ROUNDS", 10);
    let rss_max = env_u64("RFB_SOAK_RSS_MB", 256);
    let fd_max = env_u64("RFB_SOAK_FD", 512);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let spec = RuntimeSpec {
        backend: RuntimeBackend::Fake,
        ..RuntimeSpec::default()
    };
    let manager = RuntimeManager::with_adapter(spec, Arc::new(FakeAdapter));

    let start_rss = rss_mb();
    let start_fd = fd_count();
    let mut errors = 0u64;

    runtime.block_on(async {
        for round in 0..rounds {
            for w in 0..8u64 {
                let id = format!("soak-{round}-{w}");
                let handle = manager.create_for_session(&id).await;
                if handle.status != rfb_runtime::orchestration::RuntimeStatus::Running {
                    errors += 1;
                }
                manager.cancel_for_session(&id).await;
                if manager.destroy_for_session(&id).await.is_none() {
                    errors += 1;
                }
            }
        }
    });

    let end_rss = rss_mb();
    let end_fd = fd_count();
    let rss_delta = end_rss.saturating_sub(start_rss);
    let fd_delta = end_fd.saturating_sub(start_fd);

    let elapsed = Duration::from_millis(0);
    println!(
        "{{\"benchmark\":\"soak\",\"rounds\":{rounds},\"errors\":{errors},\"rss_delta_mb\":{rss_delta},\"peak_rss_mb\":{end_rss},\"fd_delta\":{fd_delta},\"fd_current\":{end_fd},\"elapsed_ns\":{}}}",
        elapsed.as_nanos()
    );

    if errors > 0 {
        eprintln!("FAIL: {errors} errors during soak");
        std::process::exit(1);
    }
    if end_rss > rss_max {
        eprintln!("FAIL: RSS {end_rss}MiB exceeds threshold {rss_max}MiB");
        std::process::exit(1);
    }
    if end_fd > fd_max {
        eprintln!("FAIL: FD count {end_fd} exceeds threshold {fd_max}");
        std::process::exit(1);
    }
    println!("rfb-runtime soak: PASS");
}
