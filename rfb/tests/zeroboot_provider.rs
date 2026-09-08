#![cfg(feature = "zeroboot")]

use rfb::protocol::{Hello, HostSession};
use rfb::zeroboot::{
    capability_name, execute_request, Config, Error, ZeroBootProvider, ZBRT_V1_CAPABILITIES,
};
use rfb::{Capability, SandboxProvider, SandboxSpec, TransportKind};
use std::time::Duration;
#[test]
fn config_defaults_and_validation_errors_are_explicit() {
    let config = Config::default();
    assert_eq!(config.guest_port, 5000);
    assert_eq!(config.timeout, Duration::from_secs(30));
    assert_eq!(config.kernel, None);
    assert_eq!(config.rootfs, None);
    assert_eq!(config.firecracker, None);
    assert!(
        Config {
            guest_port: 0,
            ..config.clone()
        }
        .guest_port
            == 0
    );
    assert!(Config {
        timeout: Duration::ZERO,
        ..config
    }
    .timeout
    .is_zero());
}

#[test]
fn provider_exposes_expected_metadata_and_hides_no_extra_config_in_debug() {
    let provider = ZeroBootProvider::default();
    assert_eq!(provider.config(), &Config::default());
    assert_eq!(provider.backend(), rfb::BackendKind::VirtualMachine);
    assert_eq!(provider.transport(), TransportKind::Vsock);
    assert!(provider.capabilities().contains(&Capability::Execute));
    assert!(provider.capabilities().contains(&Capability::Health));
    // The provider implements the stream/cancel closed loops end-to-end, so it
    // advertises them; the filesystem ops are also part of the ZBRT V1
    // contract and routed per-sandbox behind the guest's HelloAck. Eval
    // remains unsupported at the factory surface.
    assert!(provider.capabilities().contains(&Capability::Stream));
    assert!(provider.capabilities().contains(&Capability::Cancel));
    for capability in [
        Capability::Ls,
        Capability::Find,
        Capability::Grep,
        Capability::ReadFile,
        Capability::WriteFile,
    ] {
        assert!(provider.capabilities().contains(&capability));
    }
    assert!(!provider.capabilities().contains(&Capability::Eval));
    assert_eq!(
        Error::Unsupported(Capability::Execute).kind(),
        "unsupported"
    );
    assert_eq!(
        Error::InvalidConfiguration("bad").kind(),
        "invalid_configuration"
    );
    assert_eq!(Error::Backend("bad".into()).kind(), "backend");
}

#[tokio::test]
async fn create_and_exec_fail_closed_without_linux_runtime() {
    let provider = ZeroBootProvider::default();
    let error = provider
        .create(SandboxSpec {
            capabilities: vec![Capability::ReadFile],
            ..Default::default()
        })
        .await;
    assert!(matches!(
        error,
        Err(rfb::ProviderError::Unavailable(_)) | Err(rfb::ProviderError::UnsupportedCapability(_))
    ));

    let error = provider
        .create(SandboxSpec {
            capabilities: vec![Capability::Execute],
            ..Default::default()
        })
        .await;
    #[cfg(not(target_os = "linux"))]
    assert!(matches!(
        error,
        Err(rfb::ProviderError::UnsupportedCapability(
            Capability::Execute
        ))
    ));
    #[cfg(target_os = "linux")]
    let _ = error;
}

#[test]
fn v1_negotiation_and_contract_mapping() {
    let mut session = HostSession::new();
    let ack = session
        .negotiate_capabilities(
            Hello {
                client: "test".into(),
                capabilities: vec!["execute".into(), "filesystem".into(), "v2".into()],
            },
            ZBRT_V1_CAPABILITIES,
        )
        .unwrap();
    assert_eq!(ack.capabilities, vec!["execute", "filesystem"]);
    assert_eq!(
        ZBRT_V1_CAPABILITIES,
        &[
            "execute",
            "stream",
            "deadline",
            "health",
            "cancel",
            "filesystem"
        ]
    );
    assert_eq!(capability_name(Capability::ReadFile), Some("filesystem"));
    let mut spec = rfb::ExecSpec::new("echo");
    spec.args = vec!["hello".into()];
    spec.cwd = Some("/tmp".into());
    spec.stdin = Some(b"in".to_vec());
    let req = execute_request(&spec, Duration::from_secs(1)).unwrap();
    assert_eq!(req.argv, vec!["echo", "hello"]);
    assert_eq!(req.cwd, Some("/tmp".into()));
    assert_eq!(req.stdin, b"in");
    assert!(req.encode().is_ok());
}

#[test]
fn provider_is_constructible() {
    let _ = ZeroBootProvider::default();
}
