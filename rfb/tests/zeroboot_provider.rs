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

#[test]
fn scavenge_never_touches_directories_outside_its_namespace() {
    // Ownership = name prefix + lock-content magic + a free flock, TOGETHER.
    // A third-party directory using the generic `work.lock` name must
    // survive even when its lock happens to be free at probe time.
    let root = tempfile::tempdir().unwrap();

    // 1. Foreign dir, generic name, EMPTY (claimable-looking) work.lock.
    let foreign = root.path().join("someone-elses-build");
    std::fs::create_dir_all(foreign.join("deep/nested")).unwrap();
    std::fs::write(foreign.join("work.lock"), b"").unwrap();
    std::fs::write(foreign.join("deep/nested/precious.txt"), b"data").unwrap();

    // 2. OUR prefix, but the lock content is not our magic.
    let forged = root.path().join("rfb-zeroboot-forged");
    std::fs::create_dir_all(&forged).unwrap();
    std::fs::write(forged.join("work.lock"), b"not-ours\n").unwrap();

    // 3. OUR prefix, our magic, free lock → the one legitimate target.
    let stale = root.path().join("rfb-zeroboot-stale");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("work.lock"), b"rfb-work-lock\n").unwrap();

    // 4. OUR prefix, our magic, but currently locked (live holder).
    let live = root.path().join("rfb-zeroboot-live");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join("work.lock"), b"rfb-work-lock\n").unwrap();
    let guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(live.join("work.lock"))
        .unwrap();
    {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    }

    let removed = rfb::zeroboot::scavenge_stale_in(root.path(), "work.lock", "rfb-zeroboot-");
    assert_eq!(removed, 1, "only the stale own-prefix dir may be removed");
    assert!(
        foreign.join("deep/nested/precious.txt").is_file(),
        "foreign namespace must be untouched"
    );
    assert!(
        forged.join("work.lock").is_file(),
        "foreign lock content must be untouched"
    );
    assert!(
        live.join("work.lock").is_file(),
        "live lock must be untouched"
    );
}
