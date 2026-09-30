//! Per-level sanitized aggregation (quantiles, resources, status).

use crate::cli::report::quantile_ns;
use crate::cli::web_bench::types::{ResourceSample, Sample};
use serde_json::{json, Value};

pub(super) fn aggregate(
    warmup: usize,
    level: usize,
    elapsed_s: f64,
    samples: &[Sample],
    resources: &[ResourceSample],
) -> Value {
    let good: Vec<&Sample> = samples.iter().filter(|sample| sample.ok).collect();
    let success = good.len();
    let timeout = samples.iter().filter(|sample| sample.timeout).count();
    let cancelled = samples.iter().filter(|sample| sample.cancelled).count();
    let failure = samples
        .iter()
        .filter(|sample| !sample.ok && !sample.timeout && !sample.cancelled)
        .count();
    let status = if success >= 100 {
        "PASS"
    } else {
        "INSUFFICIENT_SAMPLE"
    };

    let quant = |values: &[u64]| -> Value {
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let value = |p: f64| quantile_ns(&sorted, p).map(|ns| ((ns / 1e6) * 100.0).round() / 100.0);
        json!({"p50_ms": value(0.50), "p95_ms": value(0.95), "p99_ms": value(0.99)})
    };

    let ttfb: Vec<u64> = good.iter().filter_map(|sample| sample.ttfb).collect();
    let complete: Vec<u64> = good.iter().filter_map(|sample| sample.complete).collect();
    let provider: Vec<u64> = good
        .iter()
        .filter_map(|sample| sample.segments.provider)
        .collect();
    let sandbox: Vec<u64> = good
        .iter()
        .filter_map(|sample| sample.segments.sandbox)
        .collect();
    let tool: Vec<u64> = good
        .iter()
        .filter_map(|sample| sample.segments.tool)
        .collect();
    let sse: Vec<u64> = good
        .iter()
        .filter_map(|sample| sample.segments.sse)
        .collect();
    let rss_max = resources.iter().map(|resource| resource.rss_bytes).max();
    let cpu_max = resources.iter().map(|resource| resource.cpu_ticks).max();
    let fd_max = resources.iter().map(|resource| resource.fd_count).max();

    json!({
        "concurrency": level,
        "warmup": warmup,
        "requests": samples.len(),
        "success": success,
        "failure": failure,
        "timeout": timeout,
        "cancelled": cancelled,
        "latency": {"ttfb": quant(&ttfb), "complete": quant(&complete)},
        "segments": {
            "provider": quant(&provider),
            "sandbox": quant(&sandbox),
            "tool": quant(&tool),
            "sse": quant(&sse),
        },
        "resources": {
            "samples": resources.len(),
            "cpu_ticks_max": cpu_max,
            "rss_bytes_max": rss_max,
            "fd_count_max": fd_max,
        },
        "elapsed_s": (elapsed_s * 1000.0).round() / 1000.0,
        "status": status,
    })
}
