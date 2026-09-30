// Firecracker VM lifecycle over the local API socket. This file opens with
// plain `//` comments (no `//!` inner docs) because it is also pulled into
// `tests/firecracker_controller.rs` via `include!`, which cannot carry inner
// doc comments (same seam contract as `rfb/src/cli/error.rs`).
//
// This is the single Firecracker boot driver for the workspace: the ZeroBoot
// backend (`rfb::firecracker`, re-exported from the `rfb` crate) and the
// runtime controller entry points (`boot_config`/`boot`/`create_template_snapshot`)
// share one engine. Historical behavior differences between the two drivers
// are engine parameters; every pre-existing entry point passes its original
// value (see `boot_internal`).

mod snapshot;
mod socket;

use socket::{
    endpoint_identity, remove_owned_socket, remove_snapshot_file, remove_stale_socket,
    snapshot_file_ready, EndpointIdentity, FC_SOCKET_TIMEOUT,
};
// common is embedded whole per test binary; the paste seam (firecracker
// tests) uses only part of the socket surface — allow the per-binary unused
// re-exports.
#[allow(unused_imports)]
pub use socket::{
    parse_api_response, parse_response_head, read_http_response, read_response, readiness_error,
};

use crate::boot_args::BootArgs;
use crate::firecracker::FirecrackerConfig;
use serde::Serialize;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Firecracker VM helper failures. Typed (not `anyhow`) because this module is
/// re-exported as public API of the published `rfb-sdk` crate
/// (`rfb::firecracker`); `source()` chains survive.
#[derive(Debug, thiserror::Error)]
pub enum FirecrackerError {
    /// Underlying I/O failure (socket, log file, process spawn).
    #[error("firecracker io failure: {0}")]
    Io(#[from] std::io::Error),
    /// The Firecracker API or setup protocol failed.
    #[error("{0}")]
    Protocol(String),
}

type Result<T> = std::result::Result<T, FirecrackerError>;

/// `PR_SET_PDEATHSIG(SIGKILL)` arming for the Firecracker child.
///
/// Without it, an orchestrator that dies without an orderly `kill()` leaves
/// Firecracker running (and holding KVM/vsock resources) until the host is
/// rebooted. The hook also re-checks the parent pid: the parent may already
/// have died between `fork` and `exec`, in which case the child must not
/// `exec` at all.
///
/// The `libc` crate is only pulled in by the `guest`/`forkd` features, so the
/// three libc symbols needed here are declared directly instead of expanding
/// the feature graph for one syscall.
#[cfg(target_os = "linux")]
mod pdeathsig {
    use std::io;

    extern "C" {
        fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
        fn getppid() -> i32;
        fn _exit(status: i32) -> !;
    }

    /// `prctl(2)` option: deliver a signal to this process when its parent dies.
    const PR_SET_PDEATHSIG: i32 = 1;
    /// The parent-death signal: the orphaned VM must not linger.
    const SIGKILL: u64 = 9;

    /// Run inside `CommandExt::pre_exec` (post-fork, pre-exec) so only
    /// async-signal-safe libc calls are made. `parent_pid` is the pid recorded
    /// by the parent immediately before `spawn`.
    pub(super) fn arm(parent_pid: i32) -> io::Result<()> {
        // SAFETY: PR_SET_PDEATHSIG takes an integer signal number as arg2.
        if unsafe { prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: plain getppid query, no preconditions.
        if unsafe { getppid() } != parent_pid {
            // The parent died in the spawn race window: fail the child instead
            // of exec'ing a Firecracker with no supervisor.
            // SAFETY: _exit never returns and only affects this child.
            unsafe { _exit(1) };
        }
        Ok(())
    }
}

/// Make a Firecracker child die with its parent: prctl(PDEATHSIG, SIGKILL) is
/// set in the child before exec (pre_exec runs between fork and exec), and the
/// parent-pid check closes the fork/exec race where the parent already died
/// before prctl ran. EVERY Firecracker spawn must go through this — a single
/// unhardened spawn leaks an orphan VM (guest memory, snapshot dir, sockets)
/// when the host process is SIGKILLed. The single shared helper concentrates
/// the unsafe.
///
/// # Errors
///
/// Returns `Err` when the prctl setup fails (the child must not be spawned
/// unhardened as a fallback).
pub fn attach_pdeathsig(command: &mut Command) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let parent_pid = std::process::id() as i32;
    // SAFETY: pre_exec runs in the forked child before exec; only
    // async-signal-safe calls (prctl, getppid, _exit) are made.
    unsafe {
        command.pre_exec(move || pdeathsig::arm(parent_pid));
    }
    Ok(())
}

/// Owns a Firecracker child until VM construction succeeds. This prevents
/// failed socket/configuration setup from orphaning the process.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn take(&mut self) -> Result<Child> {
        self.0.take().ok_or_else(|| {
            FirecrackerError::Protocol("Firecracker child guard already consumed".into())
        })
    }

    /// Exit status when the child has already died (try_wait also reaps it).
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.0
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
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

/// Firecracker vsock device configuration: the guest-visible CID and the
/// host-side Unix socket the relay listens on. Doubles as the `PUT /vsock`
/// request body — the field names match the wire format exactly.
#[derive(Clone, Debug, Serialize)]
pub struct VsockConfig {
    /// Virtio-vsock context id the guest sees.
    pub guest_cid: u32,
    /// Host-side UDS path Firecracker proxies guest connections to.
    pub uds_path: String,
}

#[derive(Serialize)]
struct MachineConfig {
    vcpu_count: u32,
    mem_size_mib: u32,
    /// SMT toggle; `None` omits the field from the wire body entirely (the
    /// historical body of every existing entry point).
    #[serde(skip_serializing_if = "Option::is_none")]
    smt: Option<bool>,
}

#[derive(Serialize)]
struct VmAction {
    action_type: String,
}

/// Body of `PATCH /vm`: the v1.x pause/resume control surface.
#[derive(Serialize)]
struct VmStatePatch {
    state: &'static str,
}

/// Body of `PUT /snapshot/create` (this Firecracker build serves snapshot
/// lifecycle endpoints on PUT, mirroring the other config endpoints).
/// `snapshot_type` is omitted from the wire body when `None` (the zeroboot
/// checkpoint path keeps its historical body without the field).
#[derive(Serialize)]
struct SnapshotCreate {
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_type: Option<&'static str>,
    snapshot_path: String,
    mem_file_path: String,
}

/// Body of `PUT /snapshot/load`.
#[derive(Serialize)]
struct SnapshotLoad {
    #[serde(rename = "enable_diff_snapshots")]
    enable_diff_snapshots: bool,
    snapshot_path: String,
    mem_backend: MemBackend,
    #[serde(rename = "resume_vm")]
    resume_vm: bool,
}

/// Memory backend for `PUT /snapshot/load`: one shared backing file per
/// parent snapshot, mapped `MAP_PRIVATE` by every restored child so each
/// sandbox gets copy-on-write memory without a per-create 512 MiB copy.
#[derive(Serialize)]
struct MemBackend {
    backend_path: String,
    backend_type: &'static str,
}

/// One snapshot-load body, shared by both restore modes.
fn snapshot_load_body(vmstate_path: &str, mem_file_path: &str) -> SnapshotLoad {
    SnapshotLoad {
        enable_diff_snapshots: true,
        snapshot_path: vmstate_path.to_string(),
        mem_backend: MemBackend {
            backend_path: mem_file_path.to_string(),
            backend_type: "File",
        },
        resume_vm: true,
    }
}

/// Guest VM shape: what Firecracker allocates at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmResources {
    /// Guest memory in MiB.
    pub mem_mib: u32,
    /// Guest vCPU count. Every guest session runs its command on a worker
    /// thread, so this is what lets concurrent commands use host cores.
    pub vcpu_count: u32,
}

impl VmResources {
    /// Build a shape from its two axes.
    pub const fn new(mem_mib: u32, vcpu_count: u32) -> Self {
        Self {
            mem_mib,
            vcpu_count,
        }
    }
}

/// One booted Firecracker microVM: the child process handle plus the sockets
/// it was started with. Dropping it KILLS the VM (kill + reap on Drop) — the
/// owner drives the lifecycle, and a leaked handle must not leak a VM.
pub struct FirecrackerVm {
    // Keep the child optional so shutdown is idempotent and ownership is clear.
    process: Option<Child>,
    socket_path: String,
    socket_identity: EndpointIdentity,
    /// Snapshot directory (`<work_dir>/snapshot`) of the controller-facing
    /// entry points; `None` for the zeroboot driver, whose snapshots carry
    /// explicit paths per call.
    snapshot_dir: Option<String>,
    /// Host-side vsock relay UDS this VM was booted with, when any.
    vsock_uds_path: Option<String>,
    vsock_identity: Option<EndpointIdentity>,
}

impl FirecrackerVm {
    /// Boot a VM from a [`FirecrackerConfig`] with the given init path.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn boot_config(config: &FirecrackerConfig, init_path: &str) -> Result<Self> {
        config
            .validate()
            .map_err(|e| FirecrackerError::Protocol(e.to_string()))?;
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
            &format!("{work_dir}/firecracker.sock"),
            mem_mib,
            1,
            init_path,
        )
    }

    /// Thin shell around the shared engine for the controller entry points:
    /// resolves the runtime-managed Firecracker binary and passes the
    /// historical engine defaults.
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
        let snapshot_dir = Path::new(work_dir).join("snapshot");
        let snapshot_dir = snapshot_dir.to_string_lossy().into_owned();
        let vsock = if vsock_path.is_empty() {
            None
        } else {
            Some(VsockConfig {
                guest_cid: vsock_cid,
                uds_path: vsock_path.to_owned(),
            })
        };
        Self::boot_internal(
            &crate::config::firecracker_bin(),
            kernel_path,
            rootfs_path,
            work_dir,
            socket_path,
            Some(&snapshot_dir),
            vsock,
            VmResources::new(mem_mib, u32::from(vcpu_count)),
            init_path,
            false,
            Some("/dev/vda"),
            false,
            None,
        )
    }

    /// Boot a VM with a vsock device so the guest runtime can be driven over
    /// the Firecracker UDS relay.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn boot_with_runtime(
        firecracker_path: &str,
        kernel_path: &str,
        rootfs_path: &str,
        work_dir: &str,
        resources: VmResources,
        init_path: &str,
        guest_cid: u32,
    ) -> Result<Self> {
        let vsock_path = format!("{work_dir}/vsock.sock");
        Self::boot_internal(
            firecracker_path,
            kernel_path,
            rootfs_path,
            work_dir,
            &format!("{work_dir}/firecracker.sock"),
            None,
            Some(VsockConfig {
                guest_cid,
                uds_path: vsock_path,
            }),
            resources,
            init_path,
            false,
            Some("/dev/vda"),
            false,
            None,
        )
    }

    /// The shared boot engine. Every entry point funnels through here; the
    /// parameters carry the historical behavior differences:
    /// - `firecracker_bin_path`: explicit binary (the controller shell resolves
    ///   `crate::config::firecracker_bin()`, the zeroboot driver passes its own).
    /// - `snapshot_dir`: `Some` only for the controller entry points, which
    ///   stage `<work_dir>/snapshot` and clean stale snapshot files.
    /// - `is_read_only` / `root_rw` / `vsock_token` / `smt`: every existing
    ///   entry point passes `false` / `Some("/dev/vda")` / `false` / `None`.
    #[allow(clippy::too_many_arguments)]
    fn boot_internal(
        firecracker_bin_path: &str,
        kernel_path: &str,
        rootfs_path: &str,
        work_dir: &str,
        socket_path: &str,
        snapshot_dir: Option<&str>,
        vsock: Option<VsockConfig>,
        resources: VmResources,
        init_path: &str,
        is_read_only: bool,
        root_rw: Option<&str>,
        vsock_token: bool,
        smt: Option<bool>,
    ) -> Result<Self> {
        // Ensure parent directories exist and remove stale endpoints/files.
        if let Some(parent) = Path::new(socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(dir) = snapshot_dir {
            std::fs::create_dir_all(dir)?;
            remove_snapshot_file(&Path::new(dir).join("vmstate"));
            remove_snapshot_file(&Path::new(dir).join("mem"));
        }
        // Only remove stale Unix sockets, never an arbitrary file or symlink.
        remove_stale_socket(Path::new(socket_path));
        if let Some(vsock) = &vsock {
            remove_stale_socket(Path::new(&vsock.uds_path));
        }

        let process = Self::spawn_api(firecracker_bin_path, work_dir, socket_path)?;
        let mut vm = Self::attach_api_child(socket_path, process)?;
        vm.snapshot_dir = snapshot_dir.map(str::to_owned);

        // Configure machine
        vm.api_put(
            "/machine-config",
            &MachineConfig {
                vcpu_count: resources.vcpu_count,
                mem_size_mib: resources.mem_mib,
                smt,
            },
        )?;

        // Set boot source
        let mut boot_args = BootArgs::new().random_trust_cpu();
        if let Some(device) = root_rw {
            boot_args = boot_args.root_rw(device);
        }
        boot_args = boot_args.init(init_path);
        if vsock_token {
            boot_args = boot_args.vsock();
        }
        vm.api_put(
            "/boot-source",
            &BootSource {
                kernel_image_path: kernel_path.to_string(),
                boot_args: boot_args.finish(),
            },
        )?;

        // Add rootfs drive
        vm.api_put(
            "/drives/rootfs",
            &Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: rootfs_path.to_string(),
                is_root_device: true,
                is_read_only,
            },
        )?;

        if let Some(vsock) = &vsock {
            vm.api_put("/vsock", vsock)?;
        }

        // Start the VM
        vm.api_start()?;
        vm.vsock_uds_path = vsock.map(|v| v.uds_path);
        if let Some(path) = &vm.vsock_uds_path {
            vm.vsock_identity = endpoint_identity(Path::new(path));
        }
        eprintln!("Firecracker VM started");
        Ok(vm)
    }

    /// Spawn a Firecracker process and wait until its API socket accepts a
    /// connection. Returns the raw child on success; a dropped [`ChildGuard`]
    /// on any failure path tears the process down.
    fn spawn_api(firecracker_bin_path: &str, work_dir: &str, socket_path: &str) -> Result<Child> {
        eprintln!("Starting Firecracker...");
        let log_path = format!("{work_dir}/firecracker.log");
        // Both stdio hooks append to the same file: Firecracker logs to stdout
        // and the guest serial console arrives on stderr, so a guest panic
        // (PID-1 crash) is visible in the same file as the VMM log. Append
        // mode keeps the two handles from clobbering each other's offsets.
        let open_log = || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(|e| {
                    FirecrackerError::Protocol(format!("create Firecracker log at {log_path}: {e}"))
                })
        };
        let log = open_log()?;
        let log_out = open_log()?;
        let mut command = Command::new(firecracker_bin_path);
        command
            .args(["--api-sock", socket_path])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log));
        // The VM must not outlive this process: without PDEATHSIG a SIGKILLed
        // host leaves an orphan Firecracker holding the snapshot dir and its
        // 100s of MiB of guest memory. The single shared helper concentrates
        // the unsafe; every Firecracker spawn goes through it.
        attach_pdeathsig(&mut command)?;
        let process = command
            .spawn()
            .map_err(|e| FirecrackerError::Protocol(format!("Failed to start Firecracker: {e}")))?;
        let mut process_guard = ChildGuard::new(process);

        // Wait until the API socket accepts a connection, not merely until its
        // filesystem entry appears. A dead child fails in milliseconds (the
        // exit status is checked on every iteration) instead of burning the
        // whole socket deadline with a misleading timeout message.
        //
        // Only a plain blocking connect is used here: probing via
        // `Handle::try_current()` + `block_on` PANICS when called from an
        // async-runtime worker thread (a legal context for this public SDK
        // API), so the async branch is deliberately absent. This function is
        // synchronous spawn glue — blocking the caller for the socket deadline
        // is its documented behavior.
        let start = Instant::now();
        loop {
            let connected = UnixStream::connect(socket_path).is_ok();
            if connected {
                break;
            }
            if let Some(reason) = readiness_error(start.elapsed(), process_guard.exited()) {
                // ChildGuard owns process cleanup; do not unlink a path that may
                // have been replaced after the failed connection attempts.
                return Err(FirecrackerError::Protocol(reason.to_string()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        process_guard.take()
    }

    /// Wrap a freshly spawned (and connect-ready) Firecracker child into a VM
    /// handle by taking the socket's endpoint identity.
    fn attach_api_child(socket_path: &str, process: Child) -> Result<Self> {
        let socket_identity = endpoint_identity(Path::new(socket_path)).ok_or_else(|| {
            FirecrackerError::Protocol("Firecracker API socket is not a Unix socket".into())
        })?;
        Ok(Self {
            process: Some(process),
            socket_path: socket_path.to_owned(),
            socket_identity,
            snapshot_dir: None,
            vsock_uds_path: None,
            vsock_identity: None,
        })
    }

    fn api_put<T: Serialize>(&self, path: &str, body: &T) -> Result<String> {
        self.api_request("PUT", path, body)
    }

    fn api_patch<T: Serialize>(&self, path: &str, body: &T) -> Result<String> {
        self.api_request("PATCH", path, body)
    }

    fn api_request<T: Serialize>(&self, method: &str, path: &str, body: &T) -> Result<String> {
        // The API socket must still be the one this VM captured at boot; a
        // replaced path belongs to another VM.
        if endpoint_identity(Path::new(&self.socket_path)) != Some(self.socket_identity) {
            return Err(FirecrackerError::Protocol(
                "Firecracker API socket identity changed".into(),
            ));
        }
        let body_json = serde_json::to_string(body)
            .map_err(|e| FirecrackerError::Protocol(format!("encode request body: {e}")))?;
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            method, path, body_json.len(), body_json
        );

        let mut stream = UnixStream::connect(&self.socket_path).map_err(|e| {
            FirecrackerError::Protocol(format!(
                "Connect to Firecracker socket at {}: {e}",
                self.socket_path
            ))
        })?;
        stream.set_read_timeout(Some(FC_SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(FC_SOCKET_TIMEOUT))?;

        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let (status, resp) = read_response(&mut stream)?;
        if !(200..300).contains(&status) {
            return Err(FirecrackerError::Protocol(format!(
                "Firecracker API error on {} {}: HTTP {} {}",
                method, path, status, resp
            )));
        }

        Ok(resp)
    }

    /// InstanceStart across both Firecracker builds: the patched DevPreview
    /// build takes PUT /actions (and rejects POST outright), upstream takes
    /// POST /actions (PUT is rejected as an invalid method). Try PUT, fall
    /// back to POST on that specific rejection.
    fn api_start(&self) -> Result<()> {
        match self.api_put(
            "/actions",
            &VmAction {
                action_type: "InstanceStart".to_string(),
            },
        ) {
            Ok(_) => Ok(()),
            Err(FirecrackerError::Protocol(message))
                if message.contains("Invalid HTTP Method")
                    || message.contains("Unsupported HTTP method") =>
            {
                self.api_request(
                    "POST",
                    "/actions",
                    &VmAction {
                        action_type: "InstanceStart".to_string(),
                    },
                )?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Shared `PUT /snapshot/create` core. `wait_stable = true` (the
    /// controller `snapshot()` path) removes stale outputs and polls until
    /// both files are non-empty and size-stable; `wait_stable = false` (the
    /// zeroboot checkpoint path) keeps the original fire-and-forget timing.
    fn snapshot_create(
        &self,
        snapshot_path: &str,
        mem_file_path: &str,
        snapshot_type: Option<&'static str>,
        wait_stable: bool,
    ) -> Result<()> {
        if wait_stable {
            eprintln!("Pausing VM...");
        }
        // v1.x control surface: pause via PATCH /vm, snapshot via PUT
        // /snapshot/create (this build serves snapshot endpoints on PUT).
        self.api_patch("/vm", &VmStatePatch { state: "Paused" })?;
        if wait_stable {
            // Remove outputs from an earlier attempt. Otherwise a failed API call could
            // leave old, non-empty files that look like a newly completed snapshot.
            remove_snapshot_file(Path::new(snapshot_path));
            remove_snapshot_file(Path::new(mem_file_path));
            eprintln!("Creating snapshot...");
        }
        self.api_put(
            "/snapshot/create",
            &SnapshotCreate {
                snapshot_type,
                snapshot_path: snapshot_path.to_string(),
                mem_file_path: mem_file_path.to_string(),
            },
        )?;
        if !wait_stable {
            return Ok(());
        }

        // Firecracker writes both files asynchronously. Poll instead of using a
        // fixed sleep, and require non-empty files so partial snapshots fail.
        let deadline = Instant::now() + FC_SOCKET_TIMEOUT;
        let mut previous_sizes = None;
        let mut stable_polls = 0;
        while Instant::now() < deadline {
            let sizes = match (
                std::fs::symlink_metadata(snapshot_path),
                std::fs::symlink_metadata(mem_file_path),
            ) {
                (Ok(state), Ok(mem))
                    if state.file_type().is_file()
                        && mem.file_type().is_file()
                        && state.len() > 0
                        && mem.len() > 0 =>
                {
                    Some((state.len(), mem.len()))
                }
                _ => None,
            };
            if sizes.is_some() && sizes == previous_sizes {
                stable_polls += 1;
                if stable_polls >= 2 {
                    break;
                }
            } else {
                stable_polls = 0;
            }
            previous_sizes = sizes;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if stable_polls < 2 {
            remove_snapshot_file(Path::new(snapshot_path));
            remove_snapshot_file(Path::new(mem_file_path));
            return Err(FirecrackerError::Protocol(
                "Snapshot files did not become stable before timeout".into(),
            ));
        }
        if !snapshot_file_ready(Path::new(snapshot_path)) {
            remove_snapshot_file(Path::new(snapshot_path));
            remove_snapshot_file(Path::new(mem_file_path));
            return Err(FirecrackerError::Protocol(
                "Snapshot state file not created".into(),
            ));
        }
        if !snapshot_file_ready(Path::new(mem_file_path)) {
            remove_snapshot_file(Path::new(snapshot_path));
            remove_snapshot_file(Path::new(mem_file_path));
            return Err(FirecrackerError::Protocol(
                "Snapshot memory file not created".into(),
            ));
        }
        Ok(())
    }

    /// Pause the VM and write a full snapshot (vmstate + guest memory file) to
    /// explicit paths. The zeroboot fork path calls this from inside a
    /// `&FirecrackerVm` reference — hence `&self`, no stability polling, and
    /// no `snapshot_type` field (historical wire body).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn create_snapshot(&self, vmstate_path: &str, mem_file_path: &str) -> Result<()> {
        self.snapshot_create(vmstate_path, mem_file_path, None, false)
    }

    /// Kill the Firecracker child and reap it. Idempotent and best-effort:
    /// an already-exited child reports through `let _ =`.
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

    /// OS process id of the backing Firecracker child (`0` after teardown).
    /// Used by tests to assert teardown deterministically without counting
    /// global processes.
    pub fn id(&self) -> u32 {
        self.process.as_ref().map_or(0, Child::id)
    }

    /// Host-side vsock relay UDS this VM was booted with, when any.
    pub fn vsock_uds_path(&self) -> Option<&str> {
        self.vsock_uds_path.as_deref()
    }

    /// Connect to the vsock Unix domain socket endpoint.
    /// Returns `None` if no vsock was configured at boot time.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn vsock_connect(&self) -> Result<Option<UnixStream>> {
        let Some(vsock_path) = &self.vsock_uds_path else {
            return Ok(None);
        };
        if endpoint_identity(Path::new(vsock_path)).is_none() {
            return Err(FirecrackerError::Protocol(format!(
                "vsock socket not found at {vsock_path}"
            )));
        }
        if let Some(identity) = self.vsock_identity {
            if endpoint_identity(Path::new(vsock_path)) != Some(identity) {
                return Err(FirecrackerError::Protocol(format!(
                    "vsock socket identity changed at {vsock_path}"
                )));
            }
        }
        let stream = UnixStream::connect(vsock_path).map_err(|e| {
            FirecrackerError::Protocol(format!("connect vsock at {vsock_path}: {e}"))
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        Ok(Some(stream))
    }

    /// Restore a child VM from a parent snapshot taken by
    /// [`FirecrackerVm::create_snapshot`].
    ///
    /// Two Firecracker builds must be served with one code path:
    /// - Upstream builds require the pre-load device re-declaration (machine
    ///   shape matching the snapshot, rootfs drive, vsock relay with a fresh
    ///   per-child UDS path) and reject nothing.
    /// - The patched v1.12.x DevPreview build in `resx/` restores the
    ///   snapshot's devices verbatim and rejects any load after configuring
    ///   devices ("not allowed after configuring boot-specific resources").
    ///
    /// The upstream flow is attempted first; on that specific rejection the
    /// child falls back to a bare load, which reuses the parent's baked rootfs
    /// and vsock relay paths (the parent must therefore keep them alive — the
    /// zeroboot provider stores both under the snapshot directory). The
    /// memory backing file is shared `MAP_PRIVATE`, so a create pays no
    /// memory copy.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    /// Returns the VM and whether the bare (patched-build) path was taken —
    /// the caller records it so later creates skip work only the upstream
    /// path needs (e.g. the per-sandbox rootfs staging copy).
    pub fn restore_from_snapshot(
        firecracker_path: &str,
        work_dir: &str,
        resources: VmResources,
        rootfs_path: &str,
        guest_cid: u32,
        vmstate_path: &str,
        mem_file_path: &str,
    ) -> Result<(Self, bool)> {
        match Self::restore_with_config(
            firecracker_path,
            work_dir,
            resources,
            rootfs_path,
            guest_cid,
            vmstate_path,
            mem_file_path,
        ) {
            Ok(vm) => Ok((vm, false)),
            Err(err) => {
                if err.to_string().contains("boot-specific resources") {
                    Ok((
                        Self::restore_bare(
                            firecracker_path,
                            work_dir,
                            vmstate_path,
                            mem_file_path,
                        )?,
                        true,
                    ))
                } else {
                    Err(err)
                }
            }
        }
    }

    /// Upstream-style restore: re-declare the machine shape and non-boot
    /// devices, then load.
    fn restore_with_config(
        firecracker_path: &str,
        work_dir: &str,
        resources: VmResources,
        rootfs_path: &str,
        guest_cid: u32,
        vmstate_path: &str,
        mem_file_path: &str,
    ) -> Result<Self> {
        let socket_path = format!("{work_dir}/firecracker.sock");
        let process = Self::spawn_api(firecracker_path, work_dir, &socket_path)?;
        let vm = Self::attach_api_child(&socket_path, process)?;
        let vsock_path = format!("{work_dir}/vsock.sock");
        // Boot-specific resources (boot-source) are baked into the snapshot
        // and must NOT be configured before a load. The machine shape must
        // match the snapshot exactly.
        vm.api_put(
            "/machine-config",
            &MachineConfig {
                vcpu_count: resources.vcpu_count,
                mem_size_mib: resources.mem_mib,
                smt: None,
            },
        )?;
        vm.api_put(
            "/drives/rootfs",
            &Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: rootfs_path.to_string(),
                is_root_device: true,
                is_read_only: false,
            },
        )?;
        vm.api_put(
            "/vsock",
            &VsockConfig {
                guest_cid,
                uds_path: vsock_path,
            },
        )?;
        vm.api_put(
            "/snapshot/load",
            &snapshot_load_body(vmstate_path, mem_file_path),
        )?;
        Ok(vm)
    }

    /// Patched-build fallback: load only; every device comes back exactly as
    /// the parent snapshot baked it (including its rootfs and vsock relay
    /// paths, which is why the parent's files must outlive this child).
    fn restore_bare(
        firecracker_path: &str,
        work_dir: &str,
        vmstate_path: &str,
        mem_file_path: &str,
    ) -> Result<Self> {
        let baked = baked_vsock_uds_path(vmstate_path)?;
        // The baked name may still be bound by a live sibling's listener.
        // Unlinking the NAME is safe: established connections stay attached
        // to their own (now unnamed) inode, and this child binds a fresh
        // socket at the same path. The caller holds the snapshot-dir lock
        // across load + pool-open, so two concurrent restores cannot race on
        // the name.
        let _ = std::fs::remove_file(&baked);
        let socket_path = format!("{work_dir}/firecracker.sock");
        let process = Self::spawn_api(firecracker_path, work_dir, &socket_path)?;
        let mut vm = Self::attach_api_child(&socket_path, process)?;
        vm.api_put(
            "/snapshot/load",
            &snapshot_load_body(vmstate_path, mem_file_path),
        )?;
        vm.vsock_uds_path = Some(baked);
        vm.vsock_identity = vm
            .vsock_uds_path
            .as_deref()
            .and_then(|path| endpoint_identity(Path::new(path)));
        Ok(vm)
    }
}

/// Extract the vsock relay UDS path baked into a vmstate blob: the printable
/// ASCII string ending in `vsock.sock`. The patched Firecracker restores the
/// device verbatim, so the child must connect to this exact path.
fn baked_vsock_uds_path(vmstate_path: &str) -> Result<String> {
    let blob = std::fs::read(vmstate_path)
        .map_err(|e| FirecrackerError::Protocol(format!("read vmstate for vsock path: {e}")))?;
    let needle = b"vsock.sock";
    let pos = blob
        .windows(needle.len())
        .rposition(|w| w == needle)
        .ok_or_else(|| FirecrackerError::Protocol("vmstate carries no vsock UDS path".into()))?;
    let start = blob[..pos]
        .iter()
        .rposition(|&b| {
            !(b.is_ascii_alphanumeric() || b == b'/' || b == b'.' || b == b'_' || b == b'-')
        })
        .map(|i| i + 1)
        .unwrap_or(0);
    let path = String::from_utf8_lossy(&blob[start..pos + needle.len()]).into_owned();
    if path.starts_with('/') {
        Ok(path)
    } else {
        Err(FirecrackerError::Protocol(format!(
            "vmstate vsock path is not absolute: {path}"
        )))
    }
}

/// Boot a Firecracker VM, wait for it to be ready, then snapshot it.
/// Returns the paths to the snapshot files.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn create_template_snapshot(
    kernel_path: &str,
    rootfs_path: &str,
    work_dir: &str,
    mem_mib: u32,
    wait_secs: u64,
    init_path: &str,
) -> Result<(String, String, u32)> {
    let vm = FirecrackerVm::boot(kernel_path, rootfs_path, work_dir, mem_mib, init_path)?;

    // Wait for the guest to boot and become ready
    eprintln!("Waiting {}s for guest to boot...", wait_secs);
    std::thread::sleep(Duration::from_secs(wait_secs));

    // Take snapshot
    let (state_path, mem_path) = vm.snapshot()?;

    Ok((state_path, mem_path, mem_mib))
}

impl Drop for FirecrackerVm {
    fn drop(&mut self) {
        self.kill();
        remove_owned_socket(Path::new(&self.socket_path), self.socket_identity);
        if let Some(identity) = self.vsock_identity {
            if let Some(path) = &self.vsock_uds_path {
                remove_owned_socket(Path::new(path), identity);
            }
        }
    }
}
