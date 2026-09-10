#![cfg(feature = "cli")]

use rfb::cli::error::{self, CliError};
use rfb::cli::{cleanup, host, localhost, report, tool};
use serde_json::json;
use std::fs;
use std::path::Path;

#[test]
fn error_helpers_have_stable_codes_and_messages() {
    let cases: [(CliError, i32, &str); 5] = [
        (error::usage("bad flag"), error::EXIT_USAGE, "bad flag"),
        (
            error::validation("bad input"),
            error::EXIT_VALIDATION,
            "bad input",
        ),
        (error::io("read failed"), error::EXIT_IO, "read failed"),
        (
            error::external("tool failed"),
            error::EXIT_EXTERNAL,
            "tool failed",
        ),
        (error::no_vm("no kvm"), error::EXIT_NOVM, "no kvm"),
    ];
    for (err, code, message) in cases {
        assert_eq!(err.code, code);
        assert_eq!(err.message, message);
    }
    let converted = error::CliError::new(9, String::from("owned"));
    assert_eq!(converted.code, 9);
    assert_eq!(converted.message, "owned");
}

#[test]
fn localhost_and_snapshot_validation_cover_policy_edges() {
    assert_eq!(
        localhost::require_localhost(" https://LOCALHOST:8080///").unwrap(),
        "https://LOCALHOST:8080"
    );
    for url in [
        "",
        "ftp://localhost",
        "http://example.com",
        "http://127.0.0.2",
        "http://localhost.evil",
    ] {
        assert!(localhost::require_localhost(url).is_err(), "accepted {url}");
    }
    // The parser accepts the documented IPv4/hostname loopback forms; malformed
    // bracketed forms are rejected rather than being treated as remote hosts.
    assert!(localhost::require_localhost("http://[::1]:8080/api").is_err());
    for tag in ["", "bad tag", "bad/tag", "bad?tag"] {
        assert!(localhost::require_snapshot_tag(tag).is_err());
    }
    assert_eq!(
        localhost::require_snapshot_tag("ok-1_2.test").unwrap(),
        "ok-1_2.test"
    );
    let long = "a".repeat(129);
    assert!(localhost::require_snapshot_tag(&long).is_err());
}

#[test]
fn report_quantiles_latency_and_outcomes_are_deterministic() {
    assert_eq!(report::quantile_ns(&[], 0.5), None);
    assert_eq!(report::quantile_ns(&[10, 20, 30], 0.5), Some(20.0));
    assert_eq!(report::quantile_ns(&[10, 20, 30], 0.25), Some(15.0));
    assert_eq!(
        report::latency_summary_ms(&[]),
        json!({"p50_ms":null,"p95_ms":null,"p99_ms":null,"samples":0})
    );
    assert_eq!(
        report::latency_summary_ms(&[2_000_000, 1_000_000]),
        json!({"p50_ms":1.5,"p95_ms":1.95,"p99_ms":1.99,"samples":2})
    );
    let empty = report::Outcome::default();
    assert_eq!(empty.total(), 0);
    assert_eq!(empty.success_rate(), 0.0);
    let outcome = report::Outcome {
        success: 3,
        failure: 1,
        timeout: 1,
        cancelled: 1,
    };
    assert_eq!(outcome.total(), 6);
    assert_eq!(outcome.success_rate(), 50.0);
    assert_eq!(report::outcome_json(&outcome)["success_rate_pct"], 50.0);
}

#[test]
fn tool_paths_and_resolution_handle_windows_paths_and_missing_names() {
    #[cfg(unix)]
    assert_eq!(
        tool::normalize_for_wsl("C:\\Users\\test"),
        "/mnt/c/Users/test"
    );
    #[cfg(windows)]
    assert_eq!(
        tool::normalize_for_wsl("C:\\Users\\test"),
        "C:\\Users\\test"
    );
    #[cfg(unix)]
    assert_eq!(tool::normalize_for_wsl("relative\\path"), "relative/path");
    #[cfg(windows)]
    assert_eq!(tool::normalize_for_wsl("relative\\path"), "relative\\path");
    assert!(tool::resolve_binary("definitely-not-a-real-rfb-tool").is_none());
    assert!(!tool::tool_available("definitely-not-a-real-rfb-tool"));
    let matrix = tool::tool_matrix();
    assert!(matrix.as_object().unwrap().contains_key("firecracker"));
    assert!(tool::format_matrix().contains("cargo:"));
    assert!(matches!(
        tool::host_kind(),
        tool::HostKind::Linux | tool::HostKind::Wsl | tool::HostKind::Windows
    ));
}

#[test]
fn host_rendering_and_arch_validation_cover_success_and_failure() {
    let caps = host::HostCapabilities {
        kind: tool::HostKind::Linux,
        arch: "x86_64".into(),
        kvm: false,
    };
    let tools = json!({"z-tool": true, "a-tool": false});
    let value = host::capability_json(&caps, &tools);
    assert_eq!(value["platform"], "linux");
    let text = host::capability_text(&caps, &tools);
    assert!(text.contains("a-tool: missing"));
    assert!(text.find("a-tool").unwrap() < text.find("z-tool").unwrap());
    assert!(host::capability_text(&caps, &json!(null)).contains("tools: unknown"));
    assert!(host::require_arch("not-the-host").is_err());
    assert!(host::require_arch(&host::detect().arch).is_ok());
    assert!(host::preflight(false).is_ok());
}

#[test]
fn cleanup_discovers_only_top_level_artifacts_and_respects_confirmation() {
    let root = std::env::temp_dir().join(format!("rfb-runtime-cli-core-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let target = root.join("rfb-runtime");
    fs::create_dir_all(target.join("nested")).unwrap();
    fs::write(target.join("disk.ext4"), b"x").unwrap();
    fs::write(target.join("disk.sha256"), b"x").unwrap();
    fs::write(target.join("disk.manifest.json"), b"x").unwrap();
    fs::write(target.join("keep.txt"), b"x").unwrap();
    fs::write(target.join("nested").join("deep.ext4"), b"x").unwrap();
    let found = cleanup::discover(&target).unwrap();
    assert_eq!(found.len(), 3);
    assert!(cleanup::cleanup(&target, false, false).is_err());
    let dry = cleanup::cleanup(&target, true, false).unwrap();
    assert_eq!(dry["deleted"], 0);
    let real = cleanup::cleanup(&target, false, true).unwrap();
    assert_eq!(real["deleted"], 3);
    assert!(target.join("keep.txt").exists());
    assert!(target.join("nested").join("deep.ext4").exists());
    assert!(cleanup::validate_target(Path::new("relative/rfb-runtime")).is_err());
    let _ = fs::remove_dir_all(&root);
}
