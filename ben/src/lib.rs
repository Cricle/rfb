//! Pure benchmark helpers for `rfb-ben`: quantiles, level-list parsing, and
//! latency summaries. Kept in a lib target so the contract tests live in
//! `ben/tests/` per the repo convention (tests only in tests/).

/// Nearest-rank quantile on a pre-sorted slice (must be non-empty).
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    debug_assert!(n > 0, "quantile of empty sample set");
    let idx = ((q * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[idx]
}

/// Parse "1,2,4" into [1, 2, 4]; rejects zero, duplicates, and empties.
pub fn parse_levels(spec: &str) -> Result<Vec<usize>, String> {
    parse_list(spec, "concurrency level")
}

/// Parse "512,64,8" into [512, 64, 8]; rejects zero and duplicates.
pub fn parse_mem_mib(spec: &str) -> Result<Vec<u32>, String> {
    parse_list(spec, "memory level")
}

/// Parse "1,2" into [1, 2] vCPU levels; rejects zero and duplicates.
pub fn parse_vcpus(spec: &str) -> Result<Vec<u32>, String> {
    parse_list(spec, "vCPU level")
}

fn parse_list<T>(spec: &str, label: &str) -> Result<Vec<T>, String>
where
    T: Copy + PartialEq + Default + PartialOrd + std::str::FromStr + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    let mut values = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let value: T = part
            .parse()
            .map_err(|error| format!("invalid {label} {part:?}: {error}"))?;
        if value <= T::default() {
            return Err(format!("{label} must be positive, got {value}"));
        }
        if values.contains(&value) {
            return Err(format!("duplicate {label} {value}"));
        }
        values.push(value);
    }
    if values.is_empty() {
        return Err(format!("at least one {label} is required"));
    }
    Ok(values)
}

/// Summary for a batch of round-trip latencies in milliseconds.
pub fn latency_summary(mut latencies: Vec<f64>, failures: usize) -> serde_json::Value {
    latencies.sort_by(|a, b| a.total_cmp(b));
    let attempts = latencies.len() + failures;
    let success_rate = if attempts == 0 {
        0.0
    } else {
        100.0 * latencies.len() as f64 / attempts as f64
    };
    let empty = [0.0];
    let sample = if latencies.is_empty() {
        &empty[..]
    } else {
        &latencies[..]
    };
    serde_json::json!({
        "attempts": attempts,
        "successes": latencies.len(),
        "failures": failures,
        "success_rate_pct": success_rate,
        "p50_ms": quantile(sample, 0.50),
        "p95_ms": quantile(sample, 0.95),
        "p99_ms": quantile(sample, 0.99),
        "max_ms": sample[sample.len() - 1],
    })
}
