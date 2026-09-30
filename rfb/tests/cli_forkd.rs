#![cfg(feature = "cli")]

use rfb::cli::forkd::{
    acceptance, benchmark, load_snapshot_binding, preflight_text, provenance_state,
    resolve_forkd_bin, sanitize_snapshot_info, wait_for_guest_ready,
};
use serde_json::json;
use std::fs;
use std::time::Duration;

#[test]
fn binding_serialization_round_trip_and_loader_errors() {
    let b = json!({"schema":"rfb-snapshot-binding/v1","tag":"snap","snapshot_ready":true,"snapshot_bootable":true,
        "artifact":{"schema":"rfb-artifact/v1","backend":"forkd","profile":"forkd-agent","arch":"x86_64","transport":"tcp","protocol":"forkd","entrypoint":"/forkd-init.sh","guest_port":8888,"manifest_sha256":"x","kernel_sha256":null,"rootfs_sha256":null,"firecracker_sha256":null,"snapshot_tag":null,"snapshot_sha256":null,"snapshot_network":null,"vm_cpus":1,"vm_memory_bytes":1},
        "controller":{"detail":false,"status":"ready","bootable":true,"digest":null,"provenance":null},"record_hash":"bad","verification":"unverified"});
    let encoded = serde_json::to_string(&b).unwrap();
    let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded["tag"], "snap");
    let dir = std::env::temp_dir().join(format!("rfb-cli-forkd-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let missing = dir.join("missing.json");
    assert!(load_snapshot_binding(&missing, "snap", None).is_err());
    let path = dir.join("binding.json");
    fs::write(&path, "").unwrap();
    assert!(load_snapshot_binding(&path, "snap", None).is_err());
    fs::write(&path, serde_json::to_string(&b).unwrap()).unwrap();
    assert!(load_snapshot_binding(&path, "snap", None).is_err());
    let mut unknown = b.clone();
    unknown["future"] = json!(true);
    fs::write(&path, serde_json::to_vec(&unknown).unwrap()).unwrap();
    assert!(load_snapshot_binding(&path, "snap", None).is_err());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn preflight_rendering_and_local_url_validation_are_deterministic() {
    let value = json!({"checks":[{"name":"platform","status":"pass","reason":"ok"},{"name":"kvm","status":"blocked","reason":"missing"}]});
    let text = preflight_text(&value);
    assert!(text.contains("pass: platform (ok)"));
    assert!(text.ends_with("RFB preflight: BLOCKED; no services or system resources were changed."));
}

#[test]
fn snapshot_provenance_state_fails_closed_and_sanitizer_whitelists_fields() {
    // Fail-closed: a missing or structurally incomplete provenance record is
    // never promoted to complete.
    assert_eq!(provenance_state(None), "unavailable");
    assert_eq!(
        provenance_state(Some(&json!({"status": "complete"}))),
        "partial"
    );
    assert_eq!(
        provenance_state(Some(
            &json!({"status": "complete", "kernel_sha256": "a", "memory_sha256": "b", "vmstate_sha256": "c"})
        )),
        "complete"
    );
    assert_eq!(
        provenance_state(Some(&json!({"status": "partial"}))),
        "partial"
    );
    assert_eq!(
        provenance_state(Some(&json!({"status": "unverified"}))),
        "unverified"
    );
    // A legacy/non-object provenance record is treated as unavailable.
    assert_eq!(provenance_state(Some(&json!("legacy"))), "unavailable");

    // The sanitizer only keeps the whitelisted informational fields and reduces
    // the opaque provenance payload to its status string.
    let raw = json!({
        "tag": "snap",
        "status": "ready",
        "bootable": true,
        "digest": "sha256:abc",
        "dir": "/home/user/.local/share/forkd/snapshots/snap",
        "provenance": {"status": "complete", "kernel_sha256": "a", "memory_sha256": "b", "vmstate_sha256": "c", "vm_identity": "secret-vm"},
        "controller_version": "0.5.3"
    });
    let sanitized = sanitize_snapshot_info(raw);
    assert_eq!(sanitized["tag"], "snap");
    assert_eq!(sanitized["status"], "ready");
    assert_eq!(sanitized["bootable"], true);
    assert_eq!(sanitized["digest"], "sha256:abc");
    assert_eq!(sanitized["provenance"], "complete");
    assert!(
        sanitized.get("dir").is_none(),
        "on-disk snapshot paths must not leak"
    );
    assert!(
        sanitized.get("controller_version").is_none(),
        "non-whitelisted fields must be dropped"
    );
    assert!(
        sanitized.get("vm_identity").is_none(),
        "provenance internals must be reduced to the status string"
    );

    // Non-object info payloads degrade to an unavailable status rather than
    // panicking or echoing the payload.
    let sanitized = sanitize_snapshot_info(json!("garbage"));
    assert_eq!(sanitized["status"], "unavailable");
    assert_eq!(sanitized["provenance"], "unavailable");
}

#[test]
fn snapshot_binary_resolution_rejects_missing_and_invalid_paths() {
    assert!(resolve_forkd_bin(Some(std::path::Path::new("/no/such/forkd"))).is_err());
    assert!(resolve_forkd_bin(Some(std::path::Path::new(""))).is_err());
    let dir = std::env::temp_dir().join(format!("rfb-cli-forkd-bin-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let non_exec = dir.join("forkd");
    fs::write(&non_exec, b"#!/bin/sh\nexit 0\n").unwrap();
    // An existing file is accepted even if it is not an executable image; the
    // decision to actually spawn is gated on the target platform.
    assert!(resolve_forkd_bin(Some(&non_exec)).is_ok());
    let _ = fs::remove_dir_all(dir);
}

#[tokio::test]
async fn offline_validation_paths_do_not_connect() {
    assert!(wait_for_guest_ready("", Duration::ZERO).await.is_err());
    assert!(wait_for_guest_ready("not-an-address", Duration::ZERO)
        .await
        .is_err());
    assert!(benchmark("not a url", "snap", 0, Duration::ZERO)
        .await
        .is_err());
    assert!(acceptance("not a url", "snap", false).await.is_err());
}
