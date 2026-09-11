//! Firecracker VM configuration and API request mapping.

use crate::boot_args::BootArgs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Full configuration for booting an RFB runtime guest under Firecracker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirecrackerConfig {
    /// Host Unix domain socket for the Firecracker API.
    pub api_socket: PathBuf,
    /// Host path to the guest kernel (vmlinux).
    pub kernel_image: PathBuf,
    /// Host path to the rootfs ext4 image.
    pub rootfs: PathBuf,
    /// Host path to the vsock UDS relay.
    pub vsock_path: PathBuf,
    /// Guest CID assigned to the runtime VM.
    pub vsock_cid: u32,
    /// Guest vsock port the runtime listens on.
    pub vsock_port: u32,
    /// Memory size in MiB.
    pub memory_mb: u32,
    /// Number of vCPUs.
    pub vcpu_count: u8,
}

impl FirecrackerConfig {
    /// Build a config from `RFB_FIRECRACKER_*` environment overrides on top of
    /// the defaults, then validate it.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn from_environment() -> anyhow::Result<Self> {
        let mut config = Self::default();
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_API_SOCKET") {
            config.api_socket = value.into();
        }
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_KERNEL") {
            config.kernel_image = value.into();
        }
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_ROOTFS") {
            config.rootfs = value.into();
        }
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_VSOCK") {
            config.vsock_path = value.into();
        }
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_CID") {
            config.vsock_cid = value.parse()?;
        }
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_VSOCK_PORT") {
            config.vsock_port = value.parse()?;
        }
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_MEMORY_MB") {
            config.memory_mb = value.parse()?;
        }
        if let Ok(value) = std::env::var("RFB_FIRECRACKER_VCPU") {
            config.vcpu_count = value.parse()?;
        }
        config
            .validate()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(config)
    }
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        Self {
            api_socket: PathBuf::from("/run/rfb/firecracker.sock"),
            kernel_image: PathBuf::from("/opt/rfb/vmlinux"),
            rootfs: PathBuf::from("/opt/rfb/rfb-runtime-rootfs.ext4"),
            vsock_path: PathBuf::from("/run/rfb/runtime.vsock"),
            vsock_cid: 52,
            vsock_port: 5000,
            memory_mb: 512,
            vcpu_count: 1,
        }
    }
}

/// A Firecracker API request with its HTTP method and path.
///
/// Each variant describes exactly one Firecracker control-plane API call.
/// The `method` and `path` fields map to the Firecracker REST API
/// documented at <https://github.com/firecracker-microvm/firecracker/blob/main/docs/api_requests/>.
/// (MMDS is a separate Firecracker facility and is not used by this runtime.)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FirecrackerApiRequest {
    /// PUT /boot-source
    BootSource {
        /// Host path to the guest kernel.
        kernel_image_path: String,
        /// Kernel command line.
        boot_args: String,
    },
    /// PUT /drives/rootfs
    Rootfs {
        /// Host path to the rootfs image.
        path_on_host: String,
        /// Whether this drive is the root device.
        is_root_device: bool,
        /// Optional partition UUID.
        partuuid: Option<String>,
    },
    /// PUT /machine-config
    MachineConfig {
        /// Number of vCPUs.
        vcpu_count: u8,
        /// Memory size in MiB.
        mem_size_mib: u32,
        /// Whether SMT is enabled.
        smt: bool,
    },
    /// PUT /vsock
    Vsock {
        /// Guest CID for the relay.
        guest_cid: u32,
        /// Host UDS path for the vsock relay.
        uds_path: String,
    },
    /// PUT /actions (InstanceStart)
    InstanceStart,
}

impl FirecrackerApiRequest {
    /// Returns the HTTP method for this Firecracker API request.
    pub fn method(&self) -> &'static str {
        match self {
            FirecrackerApiRequest::BootSource { .. }
            | FirecrackerApiRequest::Rootfs { .. }
            | FirecrackerApiRequest::MachineConfig { .. }
            | FirecrackerApiRequest::Vsock { .. }
            | FirecrackerApiRequest::InstanceStart => "PUT",
        }
    }

    /// Returns the URI path for this Firecracker API request.
    pub fn path(&self) -> &'static str {
        match self {
            FirecrackerApiRequest::BootSource { .. } => "/boot-source",
            FirecrackerApiRequest::Rootfs { .. } => "/drives/rootfs",
            FirecrackerApiRequest::MachineConfig { .. } => "/machine-config",
            FirecrackerApiRequest::Vsock { .. } => "/vsock",
            FirecrackerApiRequest::InstanceStart => "/actions",
        }
    }
}

/// Config validation errors.
#[derive(Debug, thiserror::Error)]
pub enum FirecrackerConfigError {
    /// Memory is outside the 32–65536 MiB range.
    #[error("memory must be between 32 and 65536 MiB")]
    Memory,
    /// vCPU count is outside 1–32.
    #[error("vcpu count must be between 1 and 32")]
    Vcpu,
    /// Guest CID must be > 2.
    #[error("guest CID must be greater than 2")]
    Cid,
    /// A required path is empty.
    #[error("required Firecracker path is empty")]
    Path,
}

impl FirecrackerConfig {
    /// Validate the config against Firecracker's accepted ranges.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate(&self) -> Result<(), FirecrackerConfigError> {
        if !(32..=65_536).contains(&self.memory_mb) {
            return Err(FirecrackerConfigError::Memory);
        }
        if !(1..=32).contains(&self.vcpu_count) {
            return Err(FirecrackerConfigError::Vcpu);
        }
        if self.vsock_cid <= 2 {
            return Err(FirecrackerConfigError::Cid);
        }
        if self.kernel_image.as_os_str().is_empty()
            || self.rootfs.as_os_str().is_empty()
            || self.vsock_path.as_os_str().is_empty()
        {
            return Err(FirecrackerConfigError::Path);
        }
        Ok(())
    }

    /// Produce the ordered Firecracker API requests for booting this VM
    /// (machine config, boot source, rootfs, vsock, start).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn api_requests(&self) -> Result<Vec<FirecrackerApiRequest>, FirecrackerConfigError> {
        self.validate()?;
        Ok(vec![
            FirecrackerApiRequest::BootSource {
                kernel_image_path: self.kernel_image.to_string_lossy().into_owned(),
                boot_args: BootArgs::new()
                    .random_trust_cpu()
                    .root_rw("/dev/vda")
                    .init("/sbin/rfb-runtime")
                    .vsock()
                    .finish(),
            },
            FirecrackerApiRequest::Rootfs {
                path_on_host: self.rootfs.to_string_lossy().into_owned(),
                is_root_device: true,
                partuuid: None,
            },
            FirecrackerApiRequest::MachineConfig {
                vcpu_count: self.vcpu_count,
                mem_size_mib: self.memory_mb,
                smt: false,
            },
            FirecrackerApiRequest::Vsock {
                guest_cid: self.vsock_cid,
                uds_path: self.vsock_path.to_string_lossy().into_owned(),
            },
            FirecrackerApiRequest::InstanceStart,
        ])
    }
}
