//! Real-VM ZeroBoot provider assembly shared by the `zeroboot_*` real-VM
//! suites. Triple gating itself stays with the callers (`#[ignore]` +
//! `RFB_REAL_E2E=1`, via [`super::require_real`] / [`require_real_hot`]).

use rfb::zeroboot::{Config, ZeroBootProvider};
use std::path::PathBuf;
use std::time::Duration;

/// Knobs for [`provider`]. [`Default`] reproduces the pinned stack
/// `zeroboot_provider_real` boots against; [`ProviderOpts::hot`] reproduces
/// the env-overridable hot-snapshot stack of `zeroboot_concurrency` /
/// `zeroboot_fork`. Fields stay public so a suite can override one value
/// without re-plumbing the rest.
pub struct ProviderOpts {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub firecracker: PathBuf,
    pub guest_port: u32,
    pub timeout: Duration,
}

/// The e2e rootfs image, honouring `RFB_E2E_ZBRT_ROOTFS` (shared by both
/// constructor styles — this fallback was identical in all three suites).
fn e2e_rootfs() -> PathBuf {
    std::env::var_os("RFB_E2E_ZBRT_ROOTFS")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::resx("rootfs/zeroboot-zbrt-e2e.ext4"))
}

impl Default for ProviderOpts {
    /// `zeroboot_provider_real`'s hardcoded stack: arcbox kernel (no snapshot
    /// restore, so the non-virtio-net panic does not apply), firecracker at
    /// its installed path, guest port 5000, 30 s execution timeout.
    fn default() -> Self {
        Self {
            kernel: super::resx("kernel/vmlinux-arcbox-0.0.24"),
            rootfs: e2e_rootfs(),
            firecracker: PathBuf::from("/usr/local/bin/firecracker"),
            guest_port: 5000,
            timeout: Duration::from_secs(30),
        }
    }
}

impl ProviderOpts {
    /// Hot-snapshot suites: hot restore needs a virtio-net-capable kernel
    /// (arcbox panics on snapshot-restored vsock), so the kernel defaults to
    /// the 5.10.225 build the e2e workflow downloads into resx and both the
    /// kernel and firecracker paths are env-overridable
    /// (`RFB_E2E_KERNEL` / `RFB_E2E_FIRECRACKER`).
    pub fn hot() -> Self {
        Self {
            kernel: std::env::var_os("RFB_E2E_KERNEL")
                .map(PathBuf::from)
                .unwrap_or_else(|| super::resx("kernel/vmlinux-5.10.225")),
            firecracker: std::env::var_os("RFB_E2E_FIRECRACKER")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("firecracker")),
            ..Default::default()
        }
    }
}

/// Assemble the [`ZeroBootProvider`] from resolved paths. Every suite used
/// guest port 5000 / 30 s either explicitly or via `Config::default()`, so
/// both values are always set.
pub fn provider(opts: ProviderOpts) -> ZeroBootProvider {
    ZeroBootProvider::new(Config {
        kernel: Some(opts.kernel),
        rootfs: Some(opts.rootfs),
        firecracker: Some(opts.firecracker),
        guest_port: opts.guest_port,
        timeout: opts.timeout,
    })
}

/// [`super::require_real`] plus the hot-mode prerequisite
/// (`RFB_ZBRT_SNAPSHOT_DIR`, single shard is fine) that `zeroboot_fork` needs.
pub fn require_real_hot() {
    super::require_real();
    if std::env::var("RFB_ZBRT_SNAPSHOT_DIR").is_err() {
        panic!("skip: fork requires RFB_ZBRT_SNAPSHOT_DIR (hot mode)");
    }
}
