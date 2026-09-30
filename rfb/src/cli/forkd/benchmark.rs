//! Real forkd microbenchmark with sanitized p50/p95/p99 quantiles. Guest
//! payloads are never logged.

use crate::cli::error::{validation, CliError};
use crate::cli::forkd::preflight::snapshot_ready;
use crate::cli::forkd::sandbox::{
    destroy_sandbox, stream_to_exit, wait_for_guest_ready, GUEST_READY_DEADLINE,
};
use crate::cli::report::{record, record_sample, Outcome};
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
        record_sample(&mut samples, "create", started);

        // controller ping
        let t = Instant::now();
        record(
            &mut samples,
            &mut outcome,
            "controller_ping",
            t,
            client.ping(&sid).await.is_ok(),
        );

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
        record_sample(&mut samples, "ready", ready_started);
        // guest ping (health)
        let t = Instant::now();
        let ping = ForkdGuestClient::new(address.clone()).ping().await;
        record(
            &mut samples,
            &mut outcome,
            "health",
            t,
            ping.is_ok_and(|value| value.get("pong").is_some()),
        );

        // stream + exec (short-lived, terminal frames)
        let t = Instant::now();
        let stream_ok = stream_to_exit(&address, vec!["/bin/true".into()], false).await;
        record(&mut samples, &mut outcome, "stream", t, stream_ok.is_ok());

        let t = Instant::now();
        let exec = ForkdGuestClient::new(address.clone())
            .exec_in("/workspace", vec!["/bin/true".into()], timeout.as_secs())
            .await;
        record(
            &mut samples,
            &mut outcome,
            "exec",
            t,
            exec.is_ok_and(|value| value.get("exit_code").and_then(Value::as_i64) == Some(0)),
        );

        // cleanup with reconciliation
        let t = Instant::now();
        match destroy_sandbox(url, &sid).await {
            Ok(()) => record(&mut samples, &mut outcome, "cleanup", t, true),
            Err(_) => {
                failures += 1;
                outcome.failure += 1;
            }
        }
    }

    let stats = crate::cli::report::samples_stats_json(&samples, Some(n));
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
