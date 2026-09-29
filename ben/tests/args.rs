//! Contract tests for `rfb-ben`'s pure helpers (quantiles, level parsing,
//! latency summaries).

use rfb_ben::{latency_summary, parse_levels, parse_mem_mib, parse_vcpus, quantile};

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
fn vcpu_parse_and_reject() {
    assert_eq!(parse_vcpus("1,2,4").unwrap(), vec![1, 2, 4]);
    assert!(parse_vcpus("0").is_err());
    assert!(parse_vcpus("2,2").is_err());
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
