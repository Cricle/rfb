//! Strict integration tests for the public RFB contracts.
//!
//! These tests use only exported types/functions.  Network and VM tests use
//! validation seams or local fixtures; no real guest, VM, or external service
//! is required.

use rfb::guest::{
    CancelRequest, EvalRequest, FindRequest, GrepRequest, LsRequest, ReadRequest, StreamSpec,
    WriteRequest,
};
// `Capability`/`SandboxError` are only referenced behind zeroboot/Linux cfg
// branches; the import is intentionally unconditional so `--all-features`
// CI compiles it.
#[allow(unused_imports)]
use rfb::{
    BackendKind, Capability, ContractError, ExecSpec, ImageManifest, PixelFormat, Resources,
    SandboxError, SandboxSpec, TransportKind,
};
use std::time::Duration;

#[test]
fn core_enums_and_resource_limits_have_stable_wire_forms() {
    assert_eq!(PixelFormat::default(), PixelFormat::Rgba8);
    assert_eq!(PixelFormat::Rgb8.bytes_per_pixel(), 3);
    assert_eq!(PixelFormat::Bgra8.bytes_per_pixel(), 4);
    assert_eq!(PixelFormat::Gray8.bytes_per_pixel(), 1);
    assert_eq!(
        serde_json::to_value(BackendKind::VirtualMachine).unwrap(),
        "virtual_machine"
    );
    assert_eq!(serde_json::to_value(TransportKind::Vsock).unwrap(), "vsock");

    for field in [
        Resources {
            cpus: Some(0),
            ..Default::default()
        },
        Resources {
            memory_bytes: Some(0),
            ..Default::default()
        },
        Resources {
            disk_bytes: Some(0),
            ..Default::default()
        },
        Resources {
            pids: Some(0),
            ..Default::default()
        },
    ] {
        assert!(matches!(
            field.validate(),
            Err(ContractError::InvalidResource(_))
        ));
    }
    assert!(Resources {
        cpus: Some(1),
        memory_bytes: Some(1),
        disk_bytes: Some(1),
        pids: Some(1)
    }
    .validate()
    .is_ok());
}

#[test]
fn manifest_and_spec_reject_invalid_nested_public_values() {
    let fixture = tempfile::tempdir().unwrap();
    let manifest_path = fixture.path().join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&ImageManifest::new("guest")).unwrap(),
    )
    .unwrap();
    let _: ImageManifest = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();

    let mut image = ImageManifest::new("guest");
    for (field, expected) in [
        ("transport", "image transport must not be empty"),
        ("protocol", "image protocol must not be empty"),
        ("arch", "image arch must not be empty"),
    ] {
        image.transport = "oci".into();
        image.protocol = "rfb".into();
        image.arch = "x86_64".into();
        match field {
            "transport" => image.transport.clear(),
            "protocol" => image.protocol.clear(),
            _ => image.arch.clear(),
        }
        assert_eq!(image.validate().unwrap_err().to_string(), expected);
    }
    let spec = SandboxSpec {
        image: Some(ImageManifest::new(" ")),
        ..Default::default()
    };
    assert!(matches!(
        spec.validate(),
        Err(ContractError::InvalidManifest(_))
    ));
}

#[test]
fn guest_requests_enforce_paths_patterns_ids_and_payload_caps() {
    for path in ["../secret", "/etc/passwd", r"..\secret"] {
        assert!(LsRequest::new(path).validate().is_err());
    }
    for path in ["../secret", r"..\secret", r"\windows\system32"] {
        assert!(ReadRequest::new(path).validate().is_err());
    }
    assert!(LsRequest::new("/workspace").validate().is_ok());
    let mut find = FindRequest::new(".", "ok");
    find.max_results = 0;
    assert!(matches!(find.validate(), Err(ContractError::LimitExceeded)));
    find.pattern = "x".repeat(rfb::guest::MAX_GUEST_PATTERN_BYTES + 1);
    assert!(matches!(find.validate(), Err(ContractError::LimitExceeded)));
    let mut grep = GrepRequest::new(".", "ok");
    grep.max_bytes = rfb::guest::MAX_GUEST_RESULT_BYTES + 1;
    assert!(matches!(grep.validate(), Err(ContractError::LimitExceeded)));
    let oversized = WriteRequest::new(
        "/workspace/x",
        vec![0; rfb::guest::MAX_GUEST_RESULT_BYTES + 1],
    );
    assert!(matches!(
        oversized.validate(),
        Err(ContractError::LimitExceeded)
    ));
    assert!(matches!(
        CancelRequest::with_id("bad/id").validate(),
        Err(ContractError::InvalidId)
    ));
}

#[test]
fn guest_serialization_defaults_and_unknown_fields_are_strict() {
    let ls: LsRequest = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(ls.path, ".");
    assert_eq!(ls.max_results, rfb::guest::MAX_GUEST_RESULTS);
    let eval = EvalRequest {
        cwd: Some("workspace".into()),
        code: "1+1".into(),
        timeout: Some(Duration::from_millis(9)),
    };
    let json = serde_json::to_value(&eval).unwrap();
    assert_eq!(json["timeout"], 9);
    assert_eq!(serde_json::from_value::<EvalRequest>(json).unwrap(), eval);
    assert!(
        serde_json::from_value::<ReadRequest>(serde_json::json!({"path":"x","future":true}))
            .is_err()
    );
}

#[test]
fn stream_and_exec_boundaries_reject_invalid_contracts() {
    let mut exec = ExecSpec::new(" ");
    assert!(matches!(exec.validate(), Err(ContractError::EmptyCommand)));
    exec.command = "true".into();
    exec.timeout = Some(Duration::ZERO);
    assert!(matches!(
        exec.validate(),
        Err(ContractError::InvalidTimeout)
    ));
    for cwd in ["", "../x", "C:/x", "a\\b", "guest\0x"] {
        exec.timeout = None;
        exec.cwd = Some(cwd.into());
        assert!(matches!(exec.validate(), Err(ContractError::InvalidCwd)));
    }
    let mut stream = StreamSpec::new("sh");
    stream.env = vec![("A\0B".into(), "x".into())];
    assert!(matches!(stream.validate(), Err(ContractError::InvalidEnv)));
    stream.env.clear();
    stream.cwd = Some("../escape".into());
    assert!(stream.validate().is_err());
}

#[cfg(feature = "zeroboot")]
mod zeroboot_public {
    use super::*;
    use rfb::zeroboot::{Config, Error, ZeroBootProvider, GUEST_PORT};
    use rfb::{ProviderError, SandboxProvider};

    #[test]
    fn provider_metadata_and_fail_closed_configuration() {
        assert_eq!(GUEST_PORT, 5000);
        let provider = ZeroBootProvider::default();
        assert_eq!(provider.backend(), BackendKind::VirtualMachine);
        assert_eq!(provider.transport(), TransportKind::Vsock);
        // Execute/Health/Stream/Cancel plus the filesystem ops are implemented
        // end-to-end by the ZBRT V1 provider; per-sandbox availability still
        // depends on guest HelloAck.
        assert_eq!(
            provider.capabilities(),
            &[
                Capability::Execute,
                Capability::Health,
                Capability::Stream,
                Capability::Cancel,
                Capability::Ls,
                Capability::Find,
                Capability::Grep,
                Capability::ReadFile,
                Capability::WriteFile,
            ]
        );
        assert_eq!(
            Error::Unsupported(Capability::Execute).kind(),
            "unsupported"
        );
        let invalid = futures_lite::future::block_on(provider.create(SandboxSpec {
            resources: Resources {
                cpus: Some(0),
                ..Default::default()
            },
            ..Default::default()
        }));
        assert!(matches!(invalid, Err(ProviderError::InvalidSpec(_))));
        let unsupported = futures_lite::future::block_on(provider.create(SandboxSpec {
            capabilities: vec![Capability::ReadFile],
            ..Default::default()
        }));
        // Capability validation is deterministic and precedes backend checks:
        // ReadFile is advertised now, so the failure comes from the runtime
        // environment — missing config paths on Linux, no vsock backend on
        // non-Linux.
        #[cfg(target_os = "linux")]
        assert!(matches!(unsupported, Err(ProviderError::Unavailable(_))));
        #[cfg(not(target_os = "linux"))]
        assert!(matches!(
            unsupported,
            Err(ProviderError::UnsupportedCapability(Capability::Execute))
        ));
        let config = Config {
            guest_port: 0,
            ..Default::default()
        };
        assert_eq!(ZeroBootProvider::new(config).config().guest_port, 0);
    }

    #[test]
    fn zero_boot_exec_rejects_non_command_fields_before_backend() {
        let provider = ZeroBootProvider::default();
        let result = futures_lite::future::block_on(provider.create(SandboxSpec::default()));
        if let Ok(sandbox) = result {
            let mut spec = ExecSpec::new("echo");
            spec.args.push("x".into());
            let error = match futures_lite::future::block_on(sandbox.exec(spec)) {
                Ok(_) => panic!("arguments must be rejected before backend access"),
                Err(error) => error,
            };
            assert!(matches!(error, SandboxError::Execution(_)));
        }
    }
}

#[cfg(feature = "forkd")]
mod forkd_public {
    use super::*;
    use rfb::forkd::{ForkdClient, ForkdConfig, ForkdGuestProfile};

    #[test]
    fn forkd_public_validation_and_profiles_fail_closed() {
        let config = ForkdConfig::default();
        assert!(ForkdClient::new(config).is_ok());
        assert!(ForkdClient::validate_sandbox_id(&"x".repeat(129)).is_err());
        assert!(ForkdClient::validate_guest_address("not-an-address").is_err());
        // The external forkd binaries expose only TCP guest endpoints; a vsock
        // endpoint must fail closed rather than be parsed as a guessed wire.
        assert!(ForkdClient::validate_guest_address("vsock://3:8888").is_err());
        assert_eq!(
            ForkdGuestProfile::Minimal.capabilities(),
            [Capability::Execute, Capability::Health]
        );
        assert!(ForkdConfig::default()
            .with_guest_profile(ForkdGuestProfile::CustomShell)
            .guest_capabilities
            .contains(&Capability::Eval));
    }
}
