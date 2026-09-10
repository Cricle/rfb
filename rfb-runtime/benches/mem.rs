//! Minimal, dependency-free memory sampling for benchmarks.
//!
//! Linux exposes current and peak RSS in `/proc/self/status` (`VmRSS` and
//! `VmHWM`); on other platforms the helpers return `None` so benches degrade
//! gracefully without a `libc` dependency.

fn from_proc_status(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kib| kib * 1024)
}

/// Current resident set size in bytes (`None` on non-Linux).
pub fn current_rss_bytes() -> Option<u64> {
    from_proc_status("VmRSS:")
}

/// Peak resident set size (high-water mark) in bytes (`None` on non-Linux).
pub fn peak_rss_bytes() -> Option<u64> {
    from_proc_status("VmHWM:")
}

// `benches/codec.rs` pulls this file in as `mod mem`, so this standalone bench
// entrypoint is dead code in that binary; clippy flags it under `-D warnings`.
#[allow(dead_code)]
fn main() {
    use std::hint::black_box;
    use std::time::Instant;

    const ITERATIONS: usize = 1_000;
    const PAYLOAD: usize = 64 * 1024;
    let baseline = current_rss_bytes();
    let started = Instant::now();
    let mut retained = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let mut value = vec![0_u8; PAYLOAD];
        value[0] = 1;
        retained.push(black_box(value));
    }
    let elapsed = started.elapsed();
    let current = current_rss_bytes();
    let delta = current
        .zip(baseline)
        .map(|(now, before)| now.saturating_sub(before));
    let peak = peak_rss_bytes();
    println!(
        "{{\"benchmark\":\"memory\",\"iterations\":{ITERATIONS},\"payload_bytes\":{PAYLOAD},\"elapsed_ns\":{},\"allocs_per_sec\":{:.0},\"rss_delta_bytes\":{},\"peak_rss_bytes\":{}}}",
        elapsed.as_nanos(),
        ITERATIONS as f64 / elapsed.as_secs_f64(),
        delta.map_or_else(|| "null".into(), |bytes| bytes.to_string()),
        peak.map_or_else(|| "null".into(), |bytes| bytes.to_string()),
    );
    black_box(retained);
}
