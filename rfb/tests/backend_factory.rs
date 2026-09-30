#![cfg(all(feature = "forkd", feature = "zeroboot"))]

//! Unit-level tests for the unified backend factory: environment selection,
//! capability advertisement, and prerequisite reporting. No VM is booted.

mod common;

use common::cli::with_env;
use rfb::backend::{SandboxBackendConfig, BACKEND_ENV};

#[test]
fn unset_backend_selector_fails_closed_with_hint() {
    with_env(&[(BACKEND_ENV, None)], || {
        let error = SandboxBackendConfig::from_environment().unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(BACKEND_ENV) && message.contains("zeroboot"),
            "error must name the env var and valid values: {message}"
        );
    });
}

#[test]
fn unknown_backend_name_fails_closed() {
    with_env(&[(BACKEND_ENV, Some("docker"))], || {
        let error = SandboxBackendConfig::from_environment().unwrap_err();
        assert!(error.to_string().contains("docker"));
    });
}

#[test]
fn zeroboot_selection_reports_unmet_asset_prerequisites() {
    with_env(&[(BACKEND_ENV, Some("zeroboot"))], || {
        let backend = SandboxBackendConfig::from_environment().unwrap();
        assert!(matches!(
            backend,
            SandboxBackendConfig::ZeroBoot(rfb::zeroboot::Config { .. })
        ));
        let names: Vec<&str> = backend.prerequisites().iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["firecracker", "kernel", "rootfs"]);
        // Default config has no asset paths, so everything is unmet with
        // an actionable detail.
        for prereq in backend.unmet_prerequisites() {
            assert!(!prereq.detail.is_empty());
        }
        // Capability surface matches the provider contract.
        let caps = backend.capabilities();
        assert!(caps.contains(&rfb::Capability::Execute));
        assert!(caps.contains(&rfb::Capability::WriteFile));
        assert!(caps.contains(&rfb::Capability::Cancel));
    });
}

#[test]
fn forkd_selection_reads_snapshot_tag_and_profile() {
    with_env(
        &[
            (BACKEND_ENV, Some("Forkd")),
            ("FORKD_SNAPSHOT_TAG", Some("rfb-fresh")),
            ("FORKD_GUEST_PROFILE", Some("minimal")),
        ],
        || {
            let backend = SandboxBackendConfig::from_environment().unwrap();
            assert!(matches!(
                backend,
                SandboxBackendConfig::Forkd(rfb::forkd::ForkdConfig { .. })
            ));
            // Case-insensitive selection: "Forkd" selected the forkd arm.
            let unmet = backend.unmet_prerequisites();
            assert!(unmet.is_empty(), "snapshot_tag must be met: {unmet:?}");
            // Minimal profile deliberately excludes filesystem capabilities.
            let caps = backend.capabilities();
            assert!(caps.contains(&rfb::Capability::Execute));
            assert!(!caps.contains(&rfb::Capability::ReadFile));
        },
    );
}

#[test]
fn forkd_without_snapshot_tag_reports_unmet_prerequisite() {
    with_env(
        &[(BACKEND_ENV, Some("forkd")), ("FORKD_SNAPSHOT_TAG", None)],
        || {
            let backend = SandboxBackendConfig::from_environment().unwrap();
            let unmet = backend.unmet_prerequisites();
            assert_eq!(unmet.len(), 1);
            assert_eq!(unmet[0].name, "snapshot_tag");
        },
    );
}
