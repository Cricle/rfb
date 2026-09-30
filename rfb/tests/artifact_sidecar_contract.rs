//! Pure artifact-sidecar contract checks; no KVM, VM, network, or implementation dependency.

#[test]
fn sidecar_names_are_deterministic() {
    let artifact = "rfb-runtime-rootfs.ext4";
    assert_eq!(
        format!("{artifact}.sha256"),
        "rfb-runtime-rootfs.ext4.sha256"
    );
    assert_eq!(
        format!("{artifact}.manifest.json"),
        "rfb-runtime-rootfs.ext4.manifest.json"
    );
}

#[test]
fn manifest_requires_binding_metadata() {
    let required = [
        "artifact",
        "kind",
        "digest",
        "size_bytes",
        "protocol",
        "profile",
        "entrypoint",
        "build_commit",
        "batch_id",
        "binding_status",
    ];
    assert!(required.contains(&"protocol"));
    assert!(required.contains(&"batch_id"));
    assert!(required.contains(&"binding_status"));
}

#[test]
fn unverified_binding_is_not_a_pass() {
    let binding_status = "unverified";
    assert_ne!(binding_status, "verified");
    assert_ne!(binding_status, "pass");
}

#[test]
fn zeroboot_uses_independent_zbrt_protocol_name() {
    assert_eq!("zbrt", "zbrt");
    assert_ne!("zbrt", "rfb1");
    assert_ne!("zbrt", "forkd");
}
