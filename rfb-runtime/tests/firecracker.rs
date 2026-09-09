use rfb_runtime::firecracker::{FirecrackerApiRequest, FirecrackerConfig, FirecrackerConfigError};

#[test]
fn default_firecracker_config_generates_boot_plan_with_http_routes() {
    let config = FirecrackerConfig::default();
    let requests = config.api_requests().unwrap();
    // machine-config, boot-source, rootfs, vsock, InstanceStart
    assert_eq!(requests.len(), 5);
    assert!(matches!(
        requests[0],
        FirecrackerApiRequest::BootSource { .. }
    ));
    assert_eq!(requests[0].path(), "/boot-source");
    assert!(matches!(
        requests[2],
        FirecrackerApiRequest::MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 512,
            smt: false
        }
    ));
    assert_eq!(requests[2].path(), "/machine-config");
    assert!(matches!(
        requests[3],
        FirecrackerApiRequest::Vsock { guest_cid: 52, .. }
    ));
    assert_eq!(requests[3].path(), "/vsock");
    // InstanceStart is the final step and targets the actions endpoint.
    assert!(matches!(requests[4], FirecrackerApiRequest::InstanceStart));
    assert_eq!(requests[4].path(), "/actions");
}

#[test]
fn firecracker_routes_are_all_put() {
    let requests = FirecrackerConfig::default().api_requests().unwrap();
    for request in &requests {
        assert_eq!(request.method(), "PUT", "route {}", request.path());
    }
    let mut paths: Vec<&str> = requests.iter().map(|r| r.path()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            "/actions",
            "/boot-source",
            "/drives/rootfs",
            "/machine-config",
            "/vsock"
        ]
    );
}

#[test]
fn firecracker_config_accepts_32_mib_memory() {
    let config = FirecrackerConfig {
        memory_mb: 32,
        ..FirecrackerConfig::default()
    };
    assert!(config.validate().is_ok());
}

#[test]
fn firecracker_config_rejects_unbounded_resources() {
    let config = FirecrackerConfig {
        memory_mb: 16,
        ..FirecrackerConfig::default()
    };
    assert!(matches!(
        config.api_requests(),
        Err(FirecrackerConfigError::Memory)
    ));
    let config = FirecrackerConfig {
        vcpu_count: 0,
        ..FirecrackerConfig::default()
    };
    assert!(matches!(
        config.api_requests(),
        Err(FirecrackerConfigError::Vcpu)
    ));
    let config = FirecrackerConfig {
        vsock_cid: 2,
        ..FirecrackerConfig::default()
    };
    assert!(matches!(
        config.api_requests(),
        Err(FirecrackerConfigError::Cid)
    ));
}
