//! Public-API contract tests for `rfb-core`.
//!
//! These are integration tests: they exercise the crate exclusively through its
//! public surface and pin the behavior of the platform-neutral contracts
//! (manifest metadata/defaults, exec round-trips, legacy payloads, cwd
//! validation, and object-safe provider traits).

use rfb::{ExecSpec, ImageManifest, Sandbox, SandboxProvider, SandboxSpec};
use std::time::Duration;

#[test]
fn image_manifest_is_metadata_only() {
    let image = ImageManifest::new("registry.test/app:latest");
    assert!(image.validate().is_ok());
    let json = serde_json::to_value(&image).unwrap();
    assert_eq!(json["image_ref"], "registry.test/app:latest");
    assert!(json.get("data").is_none());
}

#[test]
fn manifest_defaults_from_json() {
    let image: ImageManifest =
        serde_json::from_value(serde_json::json!({"image_ref":"app"})).unwrap();
    assert_eq!(image.transport, "oci");
    assert_eq!(image.protocol, "rfb");
    assert_eq!(image.arch, "unknown");
}

#[test]
fn exec_round_trips_with_millisecond_timeout_and_guest_cwd() {
    let mut spec = ExecSpec::new("run");
    spec.cwd = Some("/workspace/project".into());
    spec.timeout = Some(Duration::from_millis(25));
    let json = serde_json::to_value(&spec).unwrap();
    assert_eq!(json["cwd"], "/workspace/project");
    assert_eq!(serde_json::from_value::<ExecSpec>(json).unwrap(), spec);
}

#[test]
fn exec_cwd_is_optional_for_legacy_payloads() {
    let spec: ExecSpec = serde_json::from_value(serde_json::json!({
        "command": "run"
    }))
    .unwrap();
    assert_eq!(spec.cwd, None);
    assert!(serde_json::to_value(&spec).unwrap().get("cwd").is_none());
}

#[test]
fn validates_specs_and_opaque_guest_cwd() {
    assert!(ExecSpec::new("run").validate().is_ok());
    assert!(ExecSpec::new(" ").validate().is_err());
    for cwd in ["/workspace", "workspace/project", "guest://workspace"] {
        let mut spec = ExecSpec::new("run");
        spec.cwd = Some(cwd.into());
        assert!(spec.validate().is_ok(), "cwd should be accepted: {cwd}");
    }
    for cwd in [
        "",
        "workspace/../escape",
        "../escape",
        "workspace\\escape",
        "C:/workspace",
        "guest\0workspace",
    ] {
        let mut spec = ExecSpec::new("run");
        spec.cwd = Some(cwd.into());
        assert!(spec.validate().is_err(), "cwd should be rejected: {cwd:?}");
    }
    assert!(SandboxSpec::default().validate().is_ok());
}

#[test]
fn traits_are_object_safe() {
    fn assert_object(_: &dyn SandboxProvider, _: &dyn Sandbox) {}
    let _ = assert_object;
}

#[test]
fn new_guest_capabilities_serialize_snake_case() {
    use rfb::Capability;
    assert_eq!(
        serde_json::to_value(Capability::ReadFile).unwrap(),
        "read_file"
    );
    assert_eq!(serde_json::to_value(Capability::Health).unwrap(), "health");
    assert_eq!(serde_json::to_value(Capability::Stream).unwrap(), "stream");
    assert_eq!(
        serde_json::to_value(Capability::WriteFile).unwrap(),
        "write_file"
    );
    assert_eq!(serde_json::to_value(Capability::Eval).unwrap(), "eval");
    assert_eq!(serde_json::to_value(Capability::Cancel).unwrap(), "cancel");
}

#[test]
fn error_display_names_the_failing_part() {
    use rfb::{Capability, ContractError, ProviderError, SandboxError};

    assert_eq!(
        ContractError::EmptyCommand.to_string(),
        "command must not be empty"
    );
    assert_eq!(
        ContractError::InvalidTimeout.to_string(),
        "timeout must be non-zero"
    );
    assert_eq!(
        ContractError::InvalidManifest(rfb::ManifestError::EmptyImageRef).to_string(),
        "invalid image manifest: image_ref must not be empty"
    );
    assert_eq!(
        ProviderError::UnsupportedCapability(Capability::Eval).to_string(),
        "provider does not support Eval"
    );
    assert_eq!(
        ProviderError::Unavailable("kvm unavailable".into()).to_string(),
        "provider unavailable: kvm unavailable"
    );
    assert_eq!(
        SandboxError::UnsupportedCapability(Capability::Eval).to_string(),
        "sandbox does not support Eval"
    );
    assert_eq!(
        SandboxError::Execution("exit 1".into()).to_string(),
        "execution failed: exit 1"
    );
    assert_eq!(
        SandboxError::Timeout.to_string(),
        "operation exceeded its deadline"
    );
}
