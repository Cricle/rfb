//! Sanitized aggregate reporting: quantiles and compact summaries.

use crate::cli::error::{validation, CliError};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

/// Compute the p-th percentile (0..=1) over a sorted slice of nanoseconds.
/// Returns `None` when the slice is empty.
pub fn quantile_ns(sorted: &[u64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = (sorted.len() as f64 - 1.0) * p;
    let lo = idx.floor() as usize;
    let hi = (idx.ceil() as usize).min(sorted.len() - 1);
    let frac = idx - idx.floor();
    let value = sorted[lo] as f64 + (sorted[hi] as f64 - sorted[lo] as f64) * frac;
    Some(value)
}

/// Build a `{p50_ms, p95_ms, p99_ms}` summary from nanosecond samples.
pub fn latency_summary_ms(samples: &[u64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let value =
        |p: f64| quantile_ns(&sorted, p).map(|ns| (ns / 1_000_000.0 * 1000.0).round() / 1000.0);
    json!({
        "p50_ms": value(0.50),
        "p95_ms": value(0.95),
        "p99_ms": value(0.99),
        "samples": samples.len(),
    })
}

/// Count successes, timeouts, failures, and cancelled samples.
#[derive(Debug, Default, Clone, Copy)]
pub struct Outcome {
    /// Number of requests that completed successfully.
    pub success: usize,
    /// Number of requests that failed with an error.
    pub failure: usize,
    /// Number of requests that exceeded their deadline.
    pub timeout: usize,
    /// Number of requests that were cancelled before completing.
    pub cancelled: usize,
}

impl Outcome {
    /// Total number of observed samples across all outcome buckets.
    pub fn total(&self) -> usize {
        self.success + self.failure + self.timeout + self.cancelled
    }

    /// Percentage (0..=100) of samples that succeeded; `0.0` when empty.
    pub fn success_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            0.0
        } else {
            100.0 * self.success as f64 / total as f64
        }
    }
}

/// Render a sanitized outcome summary (no payloads, no secrets).
pub fn outcome_json(outcome: &Outcome) -> Value {
    json!({
        "success": outcome.success,
        "failure": outcome.failure,
        "timeout": outcome.timeout,
        "cancelled": outcome.cancelled,
        "total": outcome.total(),
        "success_rate_pct": outcome.success_rate(),
    })
}

/// Per-op sample stats (`{samples, p50_ns, p95_ns, p99_ns}`) shared by the
/// forkd and zeroboot benchmarks; `rate_denominator` adds the forkd shape's
/// `success_rate_pct` (samples collected vs iterations attempted).
pub fn samples_stats_json(
    samples: &std::collections::HashMap<String, Vec<u64>>,
    rate_denominator: Option<usize>,
) -> serde_json::Map<String, Value> {
    let mut stats = serde_json::Map::new();
    for (name, values) in samples {
        let mut sorted = values.clone();
        sorted.sort_unstable();
        let mut entry = json!({
            "samples": values.len(),
            "p50_ns": quantile_ns(&sorted, 0.50),
            "p95_ns": quantile_ns(&sorted, 0.95),
            "p99_ns": quantile_ns(&sorted, 0.99),
        });
        if let Some(denominator) = rate_denominator {
            if let Value::Object(ref mut map) = entry {
                map.insert(
                    "success_rate_pct".into(),
                    json!(100.0 * values.len() as f64 / denominator as f64),
                );
            }
        }
        stats.insert(name.clone(), entry);
    }
    stats
}

/// Count one named op into the workload counters shared by the forkd and
/// zeroboot workload gates.
pub(crate) fn count_op(op_count: &mut std::collections::BTreeMap<String, usize>, op: &str) {
    *op_count.entry(op.to_owned()).or_default() += 1;
}

/// Push one named latency sample (nanoseconds elapsed since `started`) into
/// the benchmark sample map shared by the forkd and zeroboot benchmarks.
pub fn record_sample(samples: &mut HashMap<String, Vec<u64>>, name: &str, started: Instant) {
    samples
        .entry(name.to_owned())
        .or_default()
        .push(started.elapsed().as_nanos() as u64);
}

/// Record one benchmark op: sample + success when `ok`, failure count
/// otherwise (no sample is kept for a failed op).
pub fn record(
    samples: &mut HashMap<String, Vec<u64>>,
    outcome: &mut Outcome,
    name: &str,
    started: Instant,
    ok: bool,
) {
    if ok {
        record_sample(samples, name, started);
        outcome.success += 1;
    } else {
        outcome.failure += 1;
    }
}

/// Workload-gate bookkeeping shared by the forkd and zeroboot backends:
/// count the named op when `ok`, otherwise fail the gate with the exact
/// per-call-site validation message.
pub fn checked(
    op_count: &mut BTreeMap<String, usize>,
    ok: bool,
    name: &str,
    msg: &str,
) -> Result<(), CliError> {
    if ok {
        count_op(op_count, name);
        Ok(())
    } else {
        Err(validation(msg))
    }
}
