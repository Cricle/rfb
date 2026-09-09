//! `rfb-ben`: real-VM sandbox capacity benchmarks.
//!
//! Fills the measurement gap between `rfb-cli zeroboot verify --bench`
//! (single-session round-trip only) and `rfb-cli forkd benchmark` (forkd
//! backend only): the ZeroBoot/ZBRT backend gets a cold-boot measurement,
//! a single-session round-trip sample, a concurrency ladder, and Firecracker
//! RSS sampling across a guest-memory ladder.
//!
//! Benchmarks drive the same public [`rfb::zeroboot`] provider the SDK uses,
//! so numbers reflect the application-layer contract, not internal fast paths.
//! Everything is linux-only by nature (`/dev/kvm` + Firecracker).

use clap::Parser;
use std::path::PathBuf;
// The RSS sampler is linux-only; the report helpers are also compiled for
// unit tests on every platform, so their imports split the same way.
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "rfb-ben",
    about = "RFB sandbox capacity benchmarks (real Firecracker VMs)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// ZeroBoot/ZBRT capacity bench: cold boot, round-trip samples, a
    /// concurrency ladder, and Firecracker RSS sampling per memory level.
    Zbrt(ZbrtArgs),
}

#[derive(clap::Args, Debug)]
struct ZbrtArgs {
    /// Guest kernel image path.
    #[arg(long)]
    kernel: PathBuf,
    /// ZBRT rootfs ext4 path (build with `rfb-cli image build-rootfs --mode zeroboot-zbrt`).
    #[arg(long)]
    rootfs: PathBuf,
    /// Firecracker binary path (v1.12.x gate applies).
    #[arg(long)]
    firecracker: PathBuf,
    /// Guest vsock port.
    #[arg(long, default_value_t = 5000)]
    guest_port: u32,
    /// Single-session round-trip samples (after warmup).
    #[arg(long, default_value_t = 100)]
    samples: usize,
    /// Warmup round-trips (excluded from aggregates).
    #[arg(long, default_value_t = 5)]
    warmup: usize,
    /// Concurrency ladder levels (concurrent echo workers).
    #[arg(long, default_value = "1,2,4,8,16")]
    levels: String,
    /// Echo rounds each worker runs per ladder level.
    #[arg(long, default_value_t = 4)]
    rounds: usize,
    /// Comma-separated VM memory ladder in MiB (overrides RFB_ZBRT_VM_MEM_MIB).
    #[arg(long, default_value = "512")]
    mem_mib: String,
    /// Per-exec timeout in seconds.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
    /// Also write the JSON report to this file.
    #[arg(long)]
    json_out: Option<PathBuf>,
}

/// Nearest-rank quantile on a pre-sorted slice (must be non-empty).
#[cfg(any(target_os = "linux", test))]
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    debug_assert!(n > 0, "quantile of empty sample set");
    let idx = ((q * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[idx]
}

/// Parse "1,2,4" into [1, 2, 4]; rejects zero, duplicates, and empties.
#[cfg(any(target_os = "linux", test))]
fn parse_levels(spec: &str) -> Result<Vec<usize>, String> {
    parse_list(spec, "concurrency level")
}

/// Parse "512,64,8" into [512, 64, 8]; rejects zero and duplicates.
#[cfg(any(target_os = "linux", test))]
fn parse_mem_mib(spec: &str) -> Result<Vec<u32>, String> {
    parse_list(spec, "memory level")
}

#[cfg(any(target_os = "linux", test))]
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

/// Peak VmRSS (KiB) across Firecracker processes spawned from
/// `rfb-zeroboot-*` work dirs — the ones this benchmark (or the provider)
/// created, not unrelated Firecracker instances.
#[cfg(target_os = "linux")]
fn firecracker_rss_kib() -> Option<u64> {
    let mut peak = 0u64;
    let mut found = false;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else {
            continue;
        };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if !cmdline.contains("firecracker") || !cmdline.contains("rfb-zeroboot") {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Ok(kib) = rest.trim().trim_end_matches("kB").trim().parse::<u64>() {
                    peak = peak.max(kib);
                    found = true;
                }
            }
        }
    }
    found.then_some(peak)
}

/// Samples Firecracker RSS every 100 ms until dropped; exposes the peak via
/// [`AtomicU64`] (KiB).
#[cfg(target_os = "linux")]
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak_kib: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl RssSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak_kib = Arc::new(AtomicU64::new(0));
        let handle = {
            let stop = Arc::clone(&stop);
            let peak_kib = Arc::clone(&peak_kib);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(rss) = firecracker_rss_kib() {
                        peak_kib.fetch_max(rss, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        Self {
            stop,
            peak_kib,
            handle: Some(handle),
        }
    }

    fn peak_kib(&self) -> u64 {
        self.peak_kib.load(Ordering::Relaxed)
    }
}

#[cfg(target_os = "linux")]
impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Summary for a batch of round-trip latencies in milliseconds.
#[cfg(any(target_os = "linux", test))]
fn latency_summary(mut latencies: Vec<f64>, failures: usize) -> serde_json::Value {
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

#[cfg(target_os = "linux")]
mod zbrt {
    use super::{latency_summary, parse_levels, RssSampler, ZbrtArgs};
    use rfb::zeroboot::{Config as ZbrtConfig, ZeroBootProvider};
    use rfb::{Capability, ExecSpec, Sandbox, SandboxSpec};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    async fn echo_once(
        sandbox: &rfb::zeroboot::ZeroBootSandbox,
        timeout: u64,
    ) -> Result<f64, rfb::SandboxError> {
        let request = ExecSpec {
            timeout: Some(Duration::from_secs(timeout)),
            ..ExecSpec::new("echo")
        };
        let started = Instant::now();
        let result = sandbox.exec(request).await?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        if result.status != Some(0) {
            return Err(rfb::SandboxError::Execution(format!(
                "echo exited with {:?}",
                result.status
            )));
        }
        Ok(elapsed_ms)
    }

    /// Run one full pass of the benchmark at a single guest memory level.
    pub(super) async fn run_level(
        args: &ZbrtArgs,
        mem_mib: u32,
    ) -> Result<serde_json::Value, String> {
        // The provider reads this at boot time, so per-level overrides work.
        std::env::set_var("RFB_ZBRT_VM_MEM_MIB", mem_mib.to_string());
        let config = ZbrtConfig {
            kernel: Some(args.kernel.clone()),
            rootfs: Some(args.rootfs.clone()),
            firecracker: Some(args.firecracker.clone()),
            guest_port: args.guest_port,
            timeout: Duration::from_secs(args.timeout_secs),
        };
        let provider = ZeroBootProvider::new(config);
        let levels = parse_levels(&args.levels)?;

        let sampler = RssSampler::start();
        let boot_started = Instant::now();
        let sandbox = provider
            .create_zero_boot(SandboxSpec {
                capabilities: vec![Capability::Execute],
                ..SandboxSpec::default()
            })
            .await
            .map_err(|error| format!("cold boot failed at {mem_mib} MiB: {error}"))?;
        let cold_boot_ms = boot_started.elapsed().as_secs_f64() * 1000.0;
        let sandbox = Arc::new(sandbox);

        for _ in 0..args.warmup {
            echo_once(&sandbox, args.timeout_secs)
                .await
                .map_err(|error| format!("warmup failed: {error}"))?;
        }

        let mut single_latencies = Vec::with_capacity(args.samples);
        let mut single_failures = 0usize;
        for _ in 0..args.samples {
            match echo_once(&sandbox, args.timeout_secs).await {
                Ok(ms) => single_latencies.push(ms),
                Err(_) => single_failures += 1,
            }
        }

        let mut ladder = Vec::with_capacity(levels.len());
        for &level in &levels {
            let mut handles = Vec::with_capacity(level);
            let rounds = args.rounds;
            let timeout = args.timeout_secs;
            let ladder_started = Instant::now();
            for _ in 0..level {
                let worker = Arc::clone(&sandbox);
                handles.push(tokio::spawn(async move {
                    let mut latencies = Vec::with_capacity(4);
                    let mut failures = 0usize;
                    for _ in 0..rounds {
                        match echo_once(&worker, timeout).await {
                            Ok(ms) => latencies.push(ms),
                            Err(_) => failures += 1,
                        }
                    }
                    (latencies, failures)
                }));
            }
            let mut latencies = Vec::with_capacity(level * args.rounds);
            let mut failures = 0usize;
            for handle in handles {
                match handle.await {
                    Ok((mut ops, worker_failures)) => {
                        latencies.append(&mut ops);
                        failures += worker_failures;
                    }
                    Err(_) => failures += 1,
                }
            }
            let wall_ms = ladder_started.elapsed().as_secs_f64() * 1000.0;
            let ops = latencies.len() as f64;
            ladder.push(serde_json::json!({
                "level": level,
                "rounds_per_worker": args.rounds,
                "wall_ms": wall_ms,
                "ops_per_sec": if wall_ms > 0.0 { ops / (wall_ms / 1000.0) } else { 0.0 },
                "latency": latency_summary(latencies, failures),
            }));
        }
        drop(sandbox);
        std::thread::sleep(Duration::from_millis(200));
        let peak_rss_kib = sampler.peak_kib();
        drop(sampler);

        Ok(serde_json::json!({
            "mem_mib": mem_mib,
            "cold_boot_ms": cold_boot_ms,
            "single": latency_summary(single_latencies, single_failures),
            "ladder": ladder,
            "firecracker_peak_rss_kib": peak_rss_kib,
        }))
    }
}

#[cfg(target_os = "linux")]
async fn run_zbrt(args: ZbrtArgs) -> Result<serde_json::Value, String> {
    let mem_levels = parse_mem_mib(&args.mem_mib)?;
    let mut reports = Vec::with_capacity(mem_levels.len());
    for mem_mib in mem_levels {
        eprintln!("rfb-ben: running zbrt ladder level {mem_mib} MiB");
        reports.push(zbrt::run_level(&args, mem_mib).await?);
    }
    Ok(serde_json::json!({
        "scenario": "zbrt",
        "kernel": args.kernel.display().to_string(),
        "rootfs": args.rootfs.display().to_string(),
        "firecracker": args.firecracker.display().to_string(),
        "levels": reports,
    }))
}

#[cfg(not(target_os = "linux"))]
async fn run_zbrt(_args: ZbrtArgs) -> Result<serde_json::Value, String> {
    Err("rfb-ben zbrt requires Linux with /dev/kvm".into())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Zbrt(args) => {
            let json_out = args.json_out.clone();
            let report = run_zbrt(args).await;
            if let (Ok(value), Some(path)) = (&report, &json_out) {
                if let Ok(text) = serde_json::to_string_pretty(value) {
                    if let Err(error) = std::fs::write(path, text) {
                        eprintln!("rfb-ben: failed to write {path:?}: {error}");
                    }
                }
            }
            report
        }
    };
    match result {
        Ok(report) => {
            let text = serde_json::to_string_pretty(&report).expect("report is valid JSON");
            println!("{text}");
        }
        Err(message) => {
            eprintln!("rfb-ben: {message}");
            std::process::exit(12);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{latency_summary, parse_levels, parse_mem_mib, quantile};

    #[test]
    fn quantile_is_nearest_rank() {
        let sorted = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(quantile(&sorted, 0.5), 2.0);
        assert_eq!(quantile(&sorted, 0.95), 4.0);
        assert_eq!(quantile(&sorted, 0.0), 1.0);
        assert_eq!(quantile(&sorted, 1.0), 4.0);
    }

    #[test]
    fn levels_parse_and_reject() {
        assert_eq!(parse_levels("1,2,4").unwrap(), vec![1, 2, 4]);
        assert!(parse_levels("0").is_err());
        assert!(parse_levels("1,1").is_err());
        assert!(parse_levels("x").is_err());
        assert!(parse_levels("").is_err());
    }

    #[test]
    fn mem_mib_parse_and_reject() {
        assert_eq!(parse_mem_mib("512,64").unwrap(), vec![512, 64]);
        assert!(parse_mem_mib("0").is_err());
        assert!(parse_mem_mib("64,64").is_err());
    }

    #[test]
    fn latency_summary_tracks_success_rate() {
        let summary = latency_summary(vec![1.0, 2.0, 3.0], 1);
        assert_eq!(summary["attempts"], 4);
        assert_eq!(summary["successes"], 3);
        assert_eq!(summary["failures"], 1);
        assert_eq!(summary["success_rate_pct"], 75.0);
        assert_eq!(summary["p50_ms"], 2.0);
    }
}
