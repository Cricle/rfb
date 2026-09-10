//! Firecracker VM lifecycle over the local API socket.

mod snapshot;
mod socket;

use crate::boot_args::BootArgs;
use crate::firecracker::FirecrackerConfig;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use socket::{
    endpoint_identity, remove_owned_socket, remove_snapshot_file, remove_stale_socket,
    EndpointIdentity, FC_SOCKET_TIMEOUT,
};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub use socket::{parse_api_response, read_http_response, readiness_error};

/// A running Firecracker VM with its API socket and vsock relay identity.
pub struct FirecrackerVm {
    // Keep the child optional so shutdown is idempotent and ownership is clear.
    process: Option<Child>,
    socket_path: String,
    socket_identity: EndpointIdentity,
    snapshot_dir: String,
    vsock_path: String,
    vsock_identity: Option<EndpointIdentity>,
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn take(&mut self) -> Result<Child> {
        self.0
            .take()
            .ok_or_else(|| anyhow::anyhow!("Firecracker child guard already consumed"))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    }
}

#[derive(Serialize)]
struct BootSource {
    kernel_image_path: String,
    boot_args: String,
}

#[derive(Serialize)]
struct Drive {
    drive_id: String,
    path_on_host: String,
    is_root_device: bool,
    is_read_only: bool,
}

#[derive(Serialize)]
struct Vsock {
    guest_cid: u32,
    uds_path: String,
}

#[derive(Serialize)]
struct MachineConfig {
    vcpu_count: u32,
    mem_size_mib: u32,
}

#[derive(Serialize)]
struct VmAction {
    action_type: String,
}

impl FirecrackerVm {
    /// Boot a VM from a [`FirecrackerConfig`] with the given init path.
    pub fn boot_config(config: &FirecrackerConfig, init_path: &str) -> Result<Self> {
        config
            .validate()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let work_dir = config
            .api_socket
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/run/rfb");
        let socket_path = config.api_socket.to_string_lossy().into_owned();
        Self::boot_with_socket(
            &config.kernel_image.to_string_lossy(),
            &config.rootfs.to_string_lossy(),
            &config.vsock_path.to_string_lossy(),
            config.vsock_cid,
            work_dir,
            &socket_path,
            config.memory_mb,
            config.vcpu_count,
            init_path,
        )
    }

    /// Boot a minimal VM (1 vCPU) with a given kernel/rootfs/init.
    pub fn boot(
        kernel_path: &str,
        rootfs_path: &str,
        work_dir: &str,
        mem_mib: u32,
        init_path: &str,
    ) -> Result<Self> {
        Self::boot_with_socket(
            kernel_path,
            rootfs_path,
            "",
            0,
            work_dir,
            &format!("{}/firecracker.sock", work_dir),
            mem_mib,
            1,
            init_path,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn boot_with_socket(
        kernel_path: &str,
        rootfs_path: &str,
        vsock_path: &str,
        vsock_cid: u32,
        work_dir: &str,
        socket_path: &str,
        mem_mib: u32,
        vcpu_count: u8,
        init_path: &str,
    ) -> Result<Self> {
        let socket_path = socket_path.to_string();
        let snapshot_dir = Path::new(work_dir).join("snapshot");
        let snapshot_dir = snapshot_dir.to_string_lossy().into_owned();

        // Ensure parent directories exist and remove stale endpoints/files.
        if let Some(parent) = Path::new(&socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&snapshot_dir)?;
        // Only remove stale Unix sockets, never an arbitrary file or symlink.
        remove_stale_socket(Path::new(&socket_path));
        if !vsock_path.is_empty() {
            remove_stale_socket(Path::new(vsock_path));
        }
        remove_snapshot_file(&Path::new(&snapshot_dir).join("vmstate"));
        remove_snapshot_file(&Path::new(&snapshot_dir).join("mem"));

        // Start Firecracker. Keep its stderr in the work dir so a failed boot
        // is diagnosable — KVM permission problems surface exactly here and
        // are invisible with a nulled stderr.
        eprintln!("Starting Firecracker...");
        let log_path = Path::new(work_dir).join("firecracker.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .with_context(|| format!("open Firecracker log file {}", log_path.display()))?;
        let process = Command::new("firecracker")
            .args(["--api-sock", &socket_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .context("Failed to start Firecracker")?;
        let mut process_guard = ChildGuard::new(process);

        // Wait until the API socket accepts a connection, not merely until its
        // filesystem entry appears.
        let start = Instant::now();
        loop {
            if let Ok(stream) = UnixStream::connect(&socket_path) {
                drop(stream);
                break;
            }
            let child_exited = process_guard
                .0
                .as_mut()
                .map(|child| child.try_wait())
                .transpose()?
                .flatten();
            if let Some(reason) = readiness_error(start.elapsed(), child_exited) {
                // ChildGuard owns process cleanup; do not unlink a path that may
                // have been replaced after the failed connection attempts.
                bail!("{reason}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let socket_identity = endpoint_identity(Path::new(&socket_path))
            .ok_or_else(|| anyhow::anyhow!("Firecracker API socket is not a Unix socket"))?;
        let mut vm = Self {
            process: Some(process_guard.take()?),
            socket_path,
            socket_identity,
            snapshot_dir,
            vsock_path: vsock_path.to_string(),
            vsock_identity: None,
        };

        // Configure machine
        vm.api_put(
            "/machine-config",
            &MachineConfig {
                vcpu_count: vcpu_count as u32,
                mem_size_mib: mem_mib,
            },
        )?;

        // Set boot source
        vm.api_put(
            "/boot-source",
            &BootSource {
                kernel_image_path: kernel_path.to_string(),
                boot_args: BootArgs::new()
                    .random_trust_cpu()
                    .root_rw("/dev/vda")
                    .init(init_path)
                    .finish(),
            },
        )?;

        // Add rootfs drive
        vm.api_put(
            "/drives/rootfs",
            &Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: rootfs_path.to_string(),
                is_root_device: true,
                is_read_only: false,
            },
        )?;
        if !vsock_path.is_empty() {
            vm.api_put(
                "/vsock",
                &Vsock {
                    guest_cid: vsock_cid,
                    uds_path: vsock_path.to_string(),
                },
            )?;
        }

        // Start the VM
        vm.api_put(
            "/actions",
            &VmAction {
                action_type: "InstanceStart".to_string(),
            },
        )?;
        if !vm.vsock_path.is_empty() {
            vm.vsock_identity = endpoint_identity(Path::new(&vm.vsock_path));
        }
        eprintln!("Firecracker VM started");
        Ok(vm)
    }

    fn api_put<T: Serialize>(&mut self, path: &str, body: &T) -> Result<String> {
        self.api_request("PUT", path, body)
    }

    fn api_patch<T: Serialize>(&mut self, path: &str, body: &T) -> Result<String> {
        self.api_request("PATCH", path, body)
    }

    fn ensure_process_alive(&mut self) -> Result<()> {
        let Some(process) = self.process.as_mut() else {
            bail!("Firecracker process is not running");
        };
        if let Some(status) = process.try_wait()? {
            bail!("Firecracker exited unexpectedly: {status}");
        }
        Ok(())
    }

    fn api_request<T: Serialize>(&mut self, method: &str, path: &str, body: &T) -> Result<String> {
        self.ensure_process_alive()?;
        if endpoint_identity(Path::new(&self.socket_path)) != Some(self.socket_identity) {
            bail!("Firecracker API socket identity changed");
        }
        let body_json = serde_json::to_string(body)?;
        let content_length = body_json.len();
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            method, path, content_length, body_json
        );

        let mut stream = UnixStream::connect(&self.socket_path)
            .with_context(|| format!("Connect to Firecracker socket at {}", self.socket_path))?;
        stream.set_read_timeout(Some(FC_SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(FC_SOCKET_TIMEOUT))?;

        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let response = read_http_response(&mut stream)?;
        parse_api_response(&response, method, path)
    }

    /// Connect to the vsock Unix domain socket endpoint.
    /// Returns `None` if no vsock was configured at boot time.
    pub fn vsock_connect(&self) -> Result<Option<UnixStream>> {
        if self.vsock_path.is_empty() {
            return Ok(None);
        }
        if endpoint_identity(Path::new(&self.vsock_path)).is_none() {
            anyhow::bail!("vsock socket not found at {}", self.vsock_path);
        }
        if let Some(identity) = self.vsock_identity {
            if endpoint_identity(Path::new(&self.vsock_path)) != Some(identity) {
                anyhow::bail!("vsock socket identity changed at {}", self.vsock_path);
            }
        }
        let stream = UnixStream::connect(&self.vsock_path)
            .with_context(|| format!("connect vsock at {}", self.vsock_path))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        Ok(Some(stream))
    }

    /// Stop and reap the Firecracker child (idempotent).
    pub fn kill(&mut self) {
        if let Some(mut process) = self.process.take() {
            match process.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    let _ = process.kill();
                    let _ = process.wait();
                }
            }
        }
    }
}

impl Drop for FirecrackerVm {
    fn drop(&mut self) {
        self.kill();
        remove_owned_socket(Path::new(&self.socket_path), self.socket_identity);
        if let Some(identity) = self.vsock_identity {
            remove_owned_socket(Path::new(&self.vsock_path), identity);
        }
    }
}

/// Boot a Firecracker VM, wait for it to be ready, then snapshot it.
/// Returns the paths to the snapshot files.
pub fn create_template_snapshot(
    kernel_path: &str,
    rootfs_path: &str,
    work_dir: &str,
    mem_mib: u32,
    wait_secs: u64,
    init_path: &str,
) -> Result<(String, String, u32)> {
    let mut vm = FirecrackerVm::boot(kernel_path, rootfs_path, work_dir, mem_mib, init_path)?;

    // Wait for the guest to boot and become ready
    eprintln!("Waiting {}s for guest to boot...", wait_secs);
    std::thread::sleep(Duration::from_secs(wait_secs));

    // Take snapshot
    let (state_path, mem_path) = vm.snapshot()?;

    Ok((state_path, mem_path, mem_mib))
}
