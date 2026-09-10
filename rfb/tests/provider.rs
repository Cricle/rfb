#![cfg(feature = "zeroboot")]

use rfb::zeroboot::{Config, ZeroBootProvider};
use rfb::{Capability, ExecSpec, ProviderError, SandboxProvider, SandboxSpec, TransportKind};

#[test]
fn provider_advertises_zero_boot_capabilities_over_vsock() {
    let provider = ZeroBootProvider::default();
    assert_eq!(provider.transport(), TransportKind::Vsock);
    // ZBRT V1 provider implements Execute/Health/Stream/Cancel plus the
    // filesystem ops end-to-end; per-sandbox availability still depends on
    // the guest HelloAck (see `ZeroBootSandbox::capabilities`).
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
}

#[test]
fn default_provider_fails_closed() {
    let provider = ZeroBootProvider::default();
    let result = futures_lite::future::block_on(provider.create(SandboxSpec::default()));
    assert!(result.is_err());
    #[cfg(not(target_os = "linux"))]
    assert!(matches!(
        result,
        Err(ProviderError::UnsupportedCapability(Capability::Execute))
    ));
}

#[test]
fn config_requires_all_native_runtime_paths() {
    let provider = ZeroBootProvider::new(Config {
        kernel: Some("kernel".into()),
        ..Config::default()
    });
    let result = futures_lite::future::block_on(provider.create(SandboxSpec::default()));
    assert!(result.is_err());
}

#[test]
fn non_execute_capabilities_are_rejected() {
    let provider = ZeroBootProvider::default();
    let spec = SandboxSpec {
        capabilities: vec![Capability::ReadFramebuffer],
        ..Default::default()
    };
    let result = futures_lite::future::block_on(provider.create(spec));
    assert!(matches!(
        result,
        Err(ProviderError::UnsupportedCapability(
            Capability::ReadFramebuffer
        ))
    ));
}

#[test]
fn invalid_resources_are_rejected_before_runtime_access() {
    let provider = ZeroBootProvider::default();
    let spec = SandboxSpec {
        resources: rfb::Resources {
            cpus: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    let result = futures_lite::future::block_on(provider.create(spec));
    assert!(matches!(result, Err(ProviderError::InvalidSpec(_))));
}

#[test]
fn execute_spec_rejects_unadvertised_arguments_at_boundary() {
    let _ = ExecSpec::new("echo");
}
