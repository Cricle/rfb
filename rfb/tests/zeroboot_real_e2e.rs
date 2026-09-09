#![cfg(all(unix, feature = "cli"))]

// Real-VM ZeroBoot ZBRT E2E: boots a Firecracker microVM via the actual
// rfb-cli and exercises the full ZBRT V1 protocol surface. Triple-gated so a
// default `cargo test` never boots a VM: unix+cli cfg, `#[ignore]`, and
// `RFB_REAL_E2E=1` (checked in-test, never silently skipped). Expected
// check-name sets are pinned from `src/cli/zeroboot.rs`. Driven by
// `tests/run-real.sh` on WSL or native Linux.

mod common;

use serde_json::Value;
use std::collections::BTreeSet;
use std::time::Duration;

const ZBRT_CHECKS: &[&str] = &[
    "echo",
    "true",
    "false",
    "unsupported",
    "deadline-echo",
    "concurrent_8",
    "malformed_execute",
];

fn verify_args(bench: bool) -> Vec<String> {
    let mut args = vec![
        "zeroboot".to_owned(),
        "verify".to_owned(),
        "--require-vm".to_owned(),
        "--kernel".to_owned(),
        common::resx("kernel/vmlinux-arcbox-0.0.24")
            .to_string_lossy()
            .into_owned(),
        "--rootfs".to_owned(),
        common::rootfs("zeroboot-zbrt-e2e.ext4")
            .to_string_lossy()
            .into_owned(),
        "--json".to_owned(),
    ];
    if bench {
        args.push("--bench".to_owned());
    }
    args
}

fn run_verify(bench: bool, label: &str) -> (common::RunOutcome, Value) {
    common::require_real();
    let out = common::run(
        common::cli().args(verify_args(bench)),
        Duration::from_secs(if bench { 900 } else { 600 }),
        label,
    );
    assert_eq!(
        out.code, 0,
        "{label} failed: {} / {}",
        out.stdout, out.stderr
    );
    let value = common::parse_json(&out, label);
    (out, value)
}

#[test]
#[ignore = "boots a real Firecracker VM; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn zbrt_protocol_cases_pass() {
    let (_, value) = run_verify(false, "zeroboot verify");
    assert_eq!(value["status"], "passed", "ZBRT acceptance not passed");
    let checks = value["checks"].as_array().expect("checks array");
    let names: BTreeSet<String> = checks
        .iter()
        .map(|entry| entry["name"].as_str().expect("check name").to_owned())
        .collect();
    let expected: BTreeSet<String> = ZBRT_CHECKS.iter().map(|name| name.to_string()).collect();
    assert_eq!(names, expected, "ZBRT check-name drift");
    for check in checks {
        assert_eq!(check["ok"], true, "ZBRT check {} failed", check["name"]);
    }
    assert!(
        value["kernel_sha256"]
            .as_str()
            .unwrap_or_default()
            .starts_with("sha256:"),
        "kernel digest missing"
    );
    assert!(
        value["rootfs_sha256"]
            .as_str()
            .unwrap_or_default()
            .starts_with("sha256:"),
        "rootfs digest missing"
    );
    assert!(
        value["benchmark"].is_null(),
        "no-bench run must not benchmark"
    );
}

#[test]
#[ignore = "boots a real Firecracker VM; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn zbrt_bench_100_samples() {
    let (_, value) = run_verify(true, "zeroboot verify --bench");
    assert_eq!(value["status"], "passed");
    let bench = value["benchmark"].as_object().expect("benchmark object");
    assert_eq!(bench["samples"], 100, "expected exactly 100 samples");
    assert!(bench["attempts"].as_u64().expect("attempts") >= 100);
    assert_eq!(bench["timeouts"], 0, "bench timeouts");
    assert_eq!(bench["success_rate_pct"], 100.0);
    let p50 = bench["p50_ms"].as_f64().expect("p50_ms");
    let p95 = bench["p95_ms"].as_f64().expect("p95_ms");
    let p99 = bench["p99_ms"].as_f64().expect("p99_ms");
    let max = bench["max_ms"].as_f64().expect("max_ms");
    assert!(p50 > 0.0, "p50 not positive");
    assert!(p95 >= p50 && p99 >= p95 && max >= p99, "quantiles inverted");
}
