//! Real forkd microbenchmark with sanitized p50/p95/p99 quantiles. Guest
//! payloads are never logged.

use crate::cli::error::{external, validation, CliError};
use crate::cli::forkd::preflight::snapshot_ready;
use crate::cli::forkd::sandbox::{destroy_sandbox, wait_for_guest_ready, GUEST_READY_DEADLINE};
use crate::cli::report::{quantile_ns, Outcome};
use crate::controller::{CreateSandboxRequest, ForkdClient};
use crate::forkd_guest::ForkdGuestClient;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Real forkd microbenchmark: N iterations of create/ping/health/stream/exec/
/// cleanup with sanitized quantiles. Guest payloads are never logged.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn benchmark(
    url: &str,
    tag: &str,
    n: usize,
    timeout: Duration,
) -> Result<Value, CliError> {
    let client = ForkdClient::new(url.to_owned(), None, timeout)
        .map_err(|error| validation(error.to_string()))?;
    let snapshots = client
        .list_snapshots()
        .await
        .map_err(|error| validation(format!("forkd unavailable: {error}")))?;
    if !snapshot_ready(&snapshots, tag) {
        return Err(validation(format!(
            "bootable ready snapshot '{tag}' is unavailable"
        )));
    }

    let mut samples: std::collections::HashMap<String, Vec<u64>> = Default::default();
    let mut outcome = Outcome::default();
    let mut failures = 0usize;

    for _ in 0..n {
        let started = Instant::now();
        let created = client
            .create_sandbox(&CreateSandboxRequest {
                snapshot_tag: tag,
                n: 1,
                per_child_netns: false,
                memory_limit_mib: Some(512),
                prewarm: false,
                live_fork: false,
                hugepages: false,
            })
            .await;
        let sandbox = created.ok().and_then(|v| v.into_iter().next());
        let Some(sandbox) = sandbox else {
            failures += 1;
            outcome.failure += 1;
            continue;
        };
        let sid = sandbox.id.clone();
        let address = sandbox.guest_addr.clone();
        samples
            .entry("create".into())
            .or_default()
            .push(started.elapsed().as_nanos() as u64);

        // controller ping
        let t = Instant::now();
        if client.ping(&sid).await.is_ok() {
            samples
                .entry("controller_ping".into())
                .or_default()
                .push(t.elapsed().as_nanos() as u64);
            outcome.success += 1;
        } else {
            outcome.failure += 1;
        }

        // Wait for the guest listener before measuring RPC latency; the wait
        // is itself part of the start path users feel, so record it.
        let ready_started = Instant::now();
        if wait_for_guest_ready(&address, GUEST_READY_DEADLINE)
            .await
            .is_err()
        {
            outcome.failure += 3;
            let _ = destroy_sandbox(url, &sid).await;
            continue;
        }
        samples
            .entry("ready".into())
            .or_default()
            .push(ready_started.elapsed().as_nanos() as u64);
        // guest ping (health)
        let t = Instant::now();
        let ping = ForkdGuestClient::new(address.clone()).ping().await;
        if let Ok(value) = ping {
            if value.get("pong").is_some() {
                samples
                    .entry("health".into())
                    .or_default()
                    .push(t.elapsed().as_nanos() as u64);
                outcome.success += 1;
            } else {
                outcome.failure += 1;
            }
        } else {
            outcome.failure += 1;
        }

        // stream + exec (short-lived, terminal frames)
        let t = Instant::now();
        let stream_ok = async {
            let mut stream = ForkdGuestClient::new(address.clone())
                .stream(vec!["/bin/true".into()], None, Some(false), None)
                .await
                .map_err(|e| external(e.to_string()))?;
            loop {
                match stream
                    .next_event()
                    .await
                    .map_err(|e| external(e.to_string()))?
                {
                    Some(value) => {
                        if value.get("exit_code").is_some() {
                            break;
                        }
                    }
                    None => return Err(validation("stream closed")),
                }
            }
            Ok::<_, CliError>(())
        }
        .await;
        if stream_ok.is_ok() {
            samples
                .entry("stream".into())
                .or_default()
                .push(t.elapsed().as_nanos() as u64);
            outcome.success += 1;
        } else {
            outcome.failure += 1;
        }

        let t = Instant::now();
        let exec = ForkdGuestClient::new(address.clone())
            .exec_in("/workspace", vec!["/bin/true".into()], timeout.as_secs())
            .await;
        if let Ok(value) = exec {
            if value.get("exit_code").and_then(Value::as_i64) == Some(0) {
                samples
                    .entry("exec".into())
                    .or_default()
                    .push(t.elapsed().as_nanos() as u64);
                outcome.success += 1;
            } else {
                outcome.failure += 1;
            }
        } else {
            outcome.failure += 1;
        }

        // cleanup with reconciliation
        let t = Instant::now();
        match destroy_sandbox(url, &sid).await {
            Ok(()) => {
                samples
                    .entry("cleanup".into())
                    .or_default()
                    .push(t.elapsed().as_nanos() as u64);
                outcome.success += 1;
            }
            Err(_) => {
                failures += 1;
                outcome.failure += 1;
            }
        }
    }

    let mut stats = serde_json::Map::new();
    for (name, values) in &samples {
        let mut sorted = values.clone();
        sorted.sort_unstable();
        stats.insert(
            name.clone(),
            json!({
                "samples": values.len(),
                "success_rate_pct": 100.0 * values.len() as f64 / n as f64,
                "p50_ns": quantile_ns(&sorted, 0.50),
                "p95_ns": quantile_ns(&sorted, 0.95),
                "p99_ns": quantile_ns(&sorted, 0.99),
            }),
        );
    }
    Ok(json!({
        "result": if failures == 0 && outcome.success == n * 5 { "PASS" } else { "FAIL" },
        "snapshot_tag": tag,
        "iterations": n,
        "failures": failures,
        "outcome": crate::cli::report::outcome_json(&outcome),
        "stats": stats,
        "payload": "suppressed",
        "snapshot_deletion": "none",
    }))
}
