//! Sanitized aggregate reporting: quantiles and compact summaries.

use serde_json::{json, Value};

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
