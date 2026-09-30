#![cfg(all(feature = "zeroboot", target_os = "linux"))]

//! Real-VM concurrency gate: N concurrent hot creates (snapshot shards), one
//! exec through every live sandbox, all concurrent. Triple-gated like the
//! other real-VM suites (cfg + #[ignore] + RFB_REAL_E2E=1); scale knobs via
//! env so CI and a workstation can both run it:
//!
//! - `RFB_E2E_CONCURRENCY` — sandboxes to create at once (default 25;
//!   100 with `RFB_ZBRT_VM_MEM_MIB=64` and 4 shards fits a 16 GB host)
//! - `RFB_ZBRT_SNAPSHOT_DIR` / `RFB_ZBRT_SNAPSHOT_SHARDS` — hot path config
//! - `RFB_ZBRT_VM_MEM_MIB` — per-VM memory (default 512 is too big for 100)

mod common;

use common::realvm::{provider, ProviderOpts};
use rfb::{Capability, ExecSpec, SandboxProvider, SandboxSpec};
use std::sync::Arc;
use std::time::Instant;

fn concurrency() -> usize {
    std::env::var("RFB_E2E_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(25)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
async fn concurrent_hot_creates_scale_to_the_requested_count() {
    let provider = Arc::new(provider(ProviderOpts::hot()));
    let spec = || SandboxSpec {
        capabilities: vec![Capability::Execute],
        ..SandboxSpec::default()
    };

    // Warm every shard's parent snapshot first (one boot per shard, one-time).
    let warm = provider.create(spec()).await.expect("warm create");
    drop(warm);

    let n = concurrency();
    let started = Instant::now();
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move { provider.create(spec()).await })
        })
        .collect();
    let mut live = Vec::with_capacity(n);
    for handle in handles {
        live.push(handle.await.expect("create task").expect("create"));
    }
    let create_wall = started.elapsed();
    assert_eq!(live.len(), n, "every concurrent create must succeed");

    // One exec through every live sandbox, all concurrent.
    let started = Instant::now();
    let execs: Vec<_> = live
        .iter()
        .map(|sandbox| {
            let spec = ExecSpec {
                args: vec!["ok".into()],
                ..ExecSpec::new("echo")
            };
            async move { sandbox.exec(spec).await }
        })
        .collect();
    let mut ok = 0;
    for exec in execs {
        let result = exec.await.expect("exec task");
        assert_eq!(result.stdout, b"ok\n");
        ok += 1;
    }
    let exec_wall = started.elapsed();

    println!(
        "concurrency={n}: create wall {create_wall:.0?} ({:.0} ms/create avg), {ok}/{n} execs ok in {exec_wall:.0?}",
        create_wall.as_millis() as f64 / n as f64
    );
}
