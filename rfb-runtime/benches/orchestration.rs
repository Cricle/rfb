//! Orchestration benchmark: measures single-session and concurrent
//! create/cancel/destroy against an injected fake adapter, so it runs anywhere
//! without a real VM. Output is aggregate JSON; never secrets or full payloads.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rfb_runtime::orchestration::{
    RuntimeBackend, RuntimeError, RuntimeManager, RuntimeSpec, RuntimeWorkerAdapter, WorkerHandle,
};

const WARMUP: usize = 20;
const SAMPLES: usize = 200;

struct FakeAdapter {
    provision_latency: Duration,
}

#[async_trait]
impl RuntimeWorkerAdapter for FakeAdapter {
    async fn provision(
        &self,
        _spec: RuntimeSpec,
        worker_id: String,
    ) -> Result<WorkerHandle, RuntimeError> {
        if !self.provision_latency.is_zero() {
            tokio::time::sleep(self.provision_latency).await;
        }
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

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p / 100.0).round() as usize;
    sorted[index]
}

fn stats(mut samples: Vec<f64>) -> (f64, f64, f64, f64) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let sum: f64 = samples.iter().sum();
    (
        sum / samples.len() as f64,
        percentile(&samples, 50.0),
        percentile(&samples, 95.0),
        percentile(&samples, 99.0),
    )
}

async fn run_concurrent(manager: &RuntimeManager) {
    let workers = 16;
    for round in 0..(WARMUP + SAMPLES) {
        let mut tasks = Vec::new();
        for w in 0..workers {
            let manager = manager.clone();
            tasks.push(tokio::spawn(async move {
                let id = format!("c-{round}-{w}");
                let handle = manager.create_for_session(&id).await;
                assert!(manager.destroy_for_session(&id).await.is_some());
                handle
            }));
        }
        for task in tasks {
            task.await.expect("concurrent worker task");
        }
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let spec = RuntimeSpec {
        backend: RuntimeBackend::Fake,
        provision_timeout_seconds: 30,
        ..RuntimeSpec::default()
    };
    let adapter = Arc::new(FakeAdapter {
        provision_latency: Duration::from_micros(200),
    });

    // Single-session latency.
    let manager = RuntimeManager::with_adapter(spec.clone(), adapter.clone());
    let mut single = Vec::with_capacity(SAMPLES);
    runtime.block_on(async {
        for round in 0..(WARMUP + SAMPLES) {
            let id = format!("s-{round}");
            let t = Instant::now();
            let handle = manager.create_for_session(&id).await;
            let create_ns = t.elapsed().as_nanos() as f64;
            let _ = manager.destroy_for_session(&id).await;
            if round >= WARMUP {
                single.push(create_ns / 1e6);
            }
            assert_eq!(
                handle.status,
                rfb_runtime::orchestration::RuntimeStatus::Running
            );
        }
    });

    // Concurrent throughput (create+destroy pairs per second).
    let manager2 = RuntimeManager::with_adapter(spec.clone(), adapter.clone());
    let mut conc = Vec::with_capacity(SAMPLES);
    runtime.block_on(async {
        for round in 0..(WARMUP + SAMPLES) {
            let t = Instant::now();
            run_concurrent(&manager2).await;
            let elapsed_ms = t.elapsed().as_nanos() as f64 / 1e6;
            if round >= WARMUP {
                conc.push(1000.0 / (elapsed_ms / 16.0)); // pairs/sec per slot
            }
        }
    });

    let (avg, p50, p95, p99) = stats(single);
    let (cavg, cp50, cp95, cp99) = stats(conc);
    println!(
        "{{\"benchmark\":\"orchestration\",\"single_create_ms\":{{\"avg\":{avg:.3},\"p50\":{p50:.3},\"p95\":{p95:.3},\"p99\":{p99:.3}}},\"concurrent_pairs_per_sec\":{{\"avg\":{cavg:.1},\"p50\":{cp50:.1},\"p95\":{cp95:.1},\"p99\":{cp99:.1}}},\"samples\":{SAMPLES}}}"
    );
}
