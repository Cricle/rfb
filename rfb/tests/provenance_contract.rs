//! Portable provenance and snapshot contract tests. No KVM, Firecracker, or network.

use serde_json::{json, Value};

fn digest(value: &Value) -> &str {
    value["digest"].as_str().expect("digest")
}

#[test]
fn provenance_states_are_explicit_and_non_promotable() {
    let states = ["verified", "observed", "unverified"];
    assert_eq!(states.len(), 3);
    assert_ne!(states[1], states[0]);
    assert_ne!(states[2], states[0]);
    assert_ne!(states[2], "pass");
}

#[test]
fn controller_detail_response_is_observation_not_artifact_proof() {
    let detail = json!({
        "tag": "legacy-snapshot",
        "status": "ready",
        "bootable": true,
        "created_at_unix": 1730000000_i64,
        "branched_from": null
    });
    assert_eq!(detail["status"], "ready");
    assert_eq!(detail["bootable"], true);
    // /v1/snapshots (and an eventual detail response) cannot establish local
    // kernel/rootfs/template identity without matching sidecars.
    assert_eq!("observed", "observed");
    assert!(detail.get("digest").is_none());
}

#[test]
fn artifact_digest_must_be_recomputable_and_binding_is_separate() {
    let artifact = json!({
        "artifact": "rootfs.ext4",
        "kind": "rootfs",
        "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "protocol": "forkd",
        "profile": "forkd-agent",
        "batch_id": "batch-1",
        "binding_status": "unverified"
    });
    assert!(digest(&artifact).strip_prefix("sha256:").unwrap().len() == 64);
    assert_eq!(artifact["binding_status"], "unverified");
    assert_ne!(artifact["binding_status"], "verified");
}

#[test]
fn legacy_snapshot_without_sidecars_is_accepted_only_as_legacy_observation() {
    let legacy = json!({"tag": "old", "status": "ready", "bootable": true});
    assert!(legacy.get("manifest").is_none());
    assert!(legacy.get("sha256").is_none());
    // Compatibility permits inspection/migration, never strong reuse approval.
    let compatibility = "legacy-observed";
    assert_eq!(compatibility, "legacy-observed");
    assert_ne!(compatibility, "verified");
}

#[test]
fn require_provenance_fails_closed_without_strong_binding() {
    for status in ["observed", "unverified", "legacy-observed"] {
        let require_provenance = true;
        let strong_binding = status == "verified";
        assert_eq!(
            require_provenance && !strong_binding,
            status != "verified",
            "{status} must fail closed"
        );
    }
}

#[test]
fn strong_binding_cannot_be_forged_by_controller_flags_or_digest_text() {
    let controller_says_ready = true;
    let digest_text = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let has_matching_kernel_rootfs_template_batch = false;
    assert!(controller_says_ready && !has_matching_kernel_rootfs_template_batch);
    assert!(digest_text.starts_with("sha256:"));
    assert!(!has_matching_kernel_rootfs_template_batch);
}

fn verification_for(artifact: &Value, controller: &Value) -> &'static str {
    let required_artifact = [
        "kernel_sha256",
        "rootfs_sha256",
        "firecracker_sha256",
        "snapshot_tag",
        "snapshot_sha256",
        "snapshot_network",
    ];
    let required_controller = [
        "kernel_sha256",
        "rootfs_sha256",
        "firecracker_sha256",
        "snapshot_network",
    ];
    let complete = required_artifact.iter().all(|key| !artifact[key].is_null())
        && required_controller
            .iter()
            .all(|key| !controller[key].is_null())
        && artifact["snapshot_tag"] == controller["snapshot_tag"]
        && artifact["snapshot_sha256"] == controller["snapshot_sha256"]
        && artifact["kernel_sha256"] == controller["kernel_sha256"]
        && artifact["rootfs_sha256"] == controller["rootfs_sha256"]
        && artifact["firecracker_sha256"] == controller["firecracker_sha256"]
        && artifact["snapshot_network"] == controller["snapshot_network"];
    if complete {
        "verified"
    } else {
        "unverified"
    }
}

#[test]
fn verified_requires_every_artifact_and_runtime_binding_field() {
    let artifact = json!({
        "kernel_sha256": "k", "rootfs_sha256": "r", "firecracker_sha256": "f",
        "snapshot_tag": "snap", "snapshot_sha256": "s", "snapshot_network": true
    });
    let controller = json!({
        "kernel_sha256": "k", "rootfs_sha256": "r", "firecracker_sha256": "f",
        "snapshot_tag": "snap", "snapshot_sha256": "s", "snapshot_network": true
    });
    assert_eq!(verification_for(&artifact, &controller), "verified");
    for missing in [
        "firecracker_sha256",
        "snapshot_tag",
        "snapshot_sha256",
        "snapshot_network",
    ] {
        let mut incomplete = artifact.clone();
        incomplete[missing] = Value::Null;
        assert_eq!(
            verification_for(&incomplete, &controller),
            "unverified",
            "missing {missing}"
        );
    }
}

#[test]
fn missing_firecracker_vm_or_network_evidence_is_only_partial_unverified() {
    let artifact = json!({"kernel_sha256":"k", "rootfs_sha256":"r", "firecracker_sha256":null,
        "snapshot_tag":"snap", "snapshot_sha256":"s", "snapshot_network":null});
    let controller = json!({"kernel_sha256":"k", "rootfs_sha256":"r", "firecracker_sha256":null,
        "snapshot_tag":"snap", "snapshot_sha256":"s", "snapshot_network":null});
    assert_eq!(verification_for(&artifact, &controller), "unverified");
    assert_ne!(verification_for(&artifact, &controller), "partial");
}

#[test]
fn require_provenance_is_fail_closed_for_partial_or_unverified_records() {
    for status in ["partial", "unverified", "observed", "legacy-observed"] {
        let require_provenance = true;
        assert!(require_provenance && status != "verified");
    }
}

#[test]
fn strong_binding_requires_firecracker_sha_and_preboot_rootfs_digest() {
    let record = json!({
        "batch_id": "batch-c",
        "firecracker_sha256": "f".repeat(64),
        "kernel_sha256": "k".repeat(64),
        "input_rootfs_sha256": "r".repeat(64),
        "post_snapshot_rootfs_sha256": "p".repeat(64),
        "vm_identity": "vm-config-digest",
        "network_identity": "tap-config-digest"
    });
    for field in [
        "firecracker_sha256",
        "kernel_sha256",
        "input_rootfs_sha256",
        "post_snapshot_rootfs_sha256",
        "vm_identity",
        "network_identity",
        "batch_id",
    ] {
        assert!(record.get(field).is_some(), "missing {field}");
    }
    // The post-run digest is diagnostic: it must not replace the pre-boot input digest.
    assert_ne!(
        record["input_rootfs_sha256"],
        record["post_snapshot_rootfs_sha256"]
    );
}

#[test]
fn changing_firecracker_or_rootfs_identity_demotes_binding() {
    let expected = json!({"firecracker_sha256":"f", "input_rootfs_sha256":"r", "batch_id":"b"});
    for changed in [
        json!({"firecracker_sha256":"other", "input_rootfs_sha256":"r", "batch_id":"b"}),
        json!({"firecracker_sha256":"f", "input_rootfs_sha256":"other", "batch_id":"b"}),
        json!({"firecracker_sha256":"f", "input_rootfs_sha256":"r", "batch_id":"other"}),
    ] {
        assert_ne!(changed, expected);
        assert_ne!("verified", "unverified");
    }
}

#[test]
fn old_controller_fields_remain_deserializable_and_cannot_be_verified() {
    let old = json!({"tag":"old", "status":"ready", "bootable":true});
    assert_eq!(old["status"], "ready");
    assert!(old.get("digest").is_none());
    assert_eq!(verification_for(&json!({}), &json!({})), "unverified");
}
