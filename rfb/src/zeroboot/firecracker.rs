// Snapshot/restore scaffolding is not yet reachable from the CLI; only the
// rfb-cli `zeroboot verify` path (boot_with_runtime) is live. Keep the
// capability for upcoming snapshot workflows without dead-code noise.

/// Firecracker VM helper failures. Typed (not `anyhow`) because this
/// module is public API of a published crate; `source()` chains survive.
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
use serde::Serialize;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Owns a Firecracker child until VM construction succeeds. This prevents
/// failed socket/configuration setup from orphaning the process.
struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }
    fn take(&mut self) -> Result<Child> {
        self.0
            .take()
            .ok_or_else(|| FirecrackerError::Protocol("child guard already consumed".into()))
    }
    /// Exit status when the child has already died (try_wait also reaps it).
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.0
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
    }
}
use std::time::{Duration, Instant};

const FC_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on one Firecracker API response (headers plus body). The API
/// only ever returns small JSON payloads; the bound stops a misbehaving peer
/// from exhausting memory.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Parse an HTTP/1.1 response head into `(status, content_length)`.
///
/// The status code is read from the status line only — never matched by
/// substring — so a `Content-Length: 2048` header cannot be mistaken for a
/// `204 OK` status.
pub(crate) fn parse_response_head(headers: &str) -> Result<(u16, usize)> {
    let status_line = headers.lines().next().ok_or_else(|| {
        FirecrackerError::Protocol("Firecracker returned an empty response".into())
    })?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(FirecrackerError::Protocol(format!(
            "Firecracker returned a non-HTTP/1.x status line: {status_line:?}"
        )));
    }
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| {
            FirecrackerError::Protocol(format!(
                "Firecracker returned an unparsable status line: {status_line:?}"
            ))
        })?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    Ok((status, content_length))
}

/// Read one complete HTTP/1.1 response from `stream` and return
/// `(status_code, full_response_text)`.
///
/// Reads until the header terminator (`\r\n\r\n`) arrives, parses the status
/// line and `Content-Length`, then keeps reading until the body is complete.
/// A single `read` never returns the whole response reliably, so this loop
/// replaces the previous one-shot 4096-byte read.
pub(crate) fn read_response(stream: &mut UnixStream) -> Result<(u16, String)> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).map_err(|e| {
            FirecrackerError::Protocol(format!("read Firecracker API response: {e}"))
        })?;
        if n == 0 {
            return Err(FirecrackerError::Protocol(
                "Firecracker closed the connection before completing the response".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(FirecrackerError::Protocol(format!(
                "Firecracker response exceeded {MAX_RESPONSE_BYTES} bytes before header end"
            )));
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let (status, content_length) = parse_response_head(&header_text)?;
    if header_end.saturating_add(content_length) > MAX_RESPONSE_BYTES {
        return Err(FirecrackerError::Protocol(format!(
            "Firecracker response body exceeds {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).map_err(|e| {
            FirecrackerError::Protocol(format!("read Firecracker API response body: {e}"))
        })?;
        if n == 0 {
            return Err(FirecrackerError::Protocol(
                "Firecracker closed the connection before sending the full response body".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let resp = String::from_utf8_lossy(&buf).into_owned();
    Ok((status, resp))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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
pub(crate) fn attach_pdeathsig(command: &mut Command) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent_pid = std::process::id() as libc::pid_t;
        // SAFETY: pre_exec runs in the forked child before exec; only
        // async-signal-safe calls (prctl, getppid, _exit) are made.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    // The intended parent is already gone: PDEATHSIG would
                    // never fire, so exit instead of orphaning the VM.
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
    }
    Ok(())
}

/// One booted Firecracker microVM: the child process handle plus the sockets
/// it was started with. Dropping it KILLS the VM (kill + reap on Drop) — the
/// owner drives the lifecycle, and a leaked handle must not leak a VM.
pub struct FirecrackerVm {
    process: Child,
    socket_path: String,
    vsock_uds_path: Option<String>,
    /// 裸恢复共享 baked 的 vsock UDS 名：这个名字属于"最后绑定者"（每次
    /// 裸恢复 unlink→bind 换名）。任何一个裸恢复 VM Drop 时都删这个名字，
    /// 会把活着的新 VM 的名字删掉（已建立的连接靠 inode 存活，新会话
    /// ENOENT）——共享名的清理归快照目录删除/scavenger。
    vsock_uds_shared: bool,
}

/// Firecracker vsock device configuration: the guest-visible CID and the
/// host-side Unix socket the relay listens on.
#[derive(Clone, Debug)]
pub struct VsockConfig {
    /// Virtio-vsock context id the guest sees.
    pub guest_cid: u32,
    /// Host-side UDS path Firecracker proxies guest connections to.
    pub uds_path: String,
}

#[derive(Serialize)]
struct VsockDevice {
    guest_cid: u32,
    uds_path: String,
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
struct MachineConfig {
    vcpu_count: u32,
    mem_size_mib: u32,
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
#[derive(Serialize)]
struct SnapshotCreate {
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

impl FirecrackerVm {
    fn boot_internal(
        firecracker_path: &str,
        kernel_path: &str,
        rootfs_path: &str,
        work_dir: &str,
        resources: VmResources,
        init_path: &str,
        vsock: Option<VsockConfig>,
    ) -> Result<Self> {
        let mut vm = Self::spawn_api(firecracker_path, work_dir)?;

        // Configure machine
        vm.api_put(
            "/machine-config",
            &MachineConfig {
                vcpu_count: resources.vcpu_count,
                mem_size_mib: resources.mem_mib,
            },
        )?;

        // Set boot source
        vm.api_put(
            "/boot-source",
            &BootSource {
                kernel_image_path: kernel_path.to_string(),
                boot_args: rfb_runtime::boot_args::BootArgs::new()
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

        if let Some(vsock) = vsock {
            let path = vsock.uds_path.clone();
            vm.api_put(
                "/vsock",
                &VsockDevice {
                    guest_cid: vsock.guest_cid,
                    uds_path: vsock.uds_path,
                },
            )?;
            vm.set_vsock_uds_path(path);
        }

        // Start the VM
        vm.api_start()?;

        eprintln!("Firecracker VM started");
        Ok(vm)
    }

    fn api_put<T: Serialize>(&self, path: &str, body: &T) -> Result<String> {
        self.api_request("PUT", path, body)
    }

    fn api_patch<T: Serialize>(&self, path: &str, body: &T) -> Result<String> {
        self.api_request("PATCH", path, body)
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

    fn api_request<T: Serialize>(&self, method: &str, path: &str, body: &T) -> Result<String> {
        let body_json = serde_json::to_string(body)
            .map_err(|e| FirecrackerError::Protocol(format!("encode request body: {e}")))?;
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
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

    /// Kill the Firecracker child and reap it. Idempotent and best-effort:
    /// an already-exited child reports through `let _ =`.
    pub fn kill(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }

    /// Pause the VM and write a full snapshot (vmstate + guest memory file).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn create_snapshot(&self, vmstate_path: &str, mem_file_path: &str) -> Result<()> {
        // v1.x control surface: pause via PATCH /vm, snapshot via PUT
        // /snapshot/create (this build serves snapshot endpoints on PUT).
        self.api_patch("/vm", &VmStatePatch { state: "Paused" })?;
        self.api_put(
            "/snapshot/create",
            &SnapshotCreate {
                snapshot_path: vmstate_path.to_string(),
                mem_file_path: mem_file_path.to_string(),
            },
        )?;
        Ok(())
    }
}

impl FirecrackerVm {
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
        let vm = Self::spawn_api(firecracker_path, work_dir)?;
        let vsock_path = format!("{work_dir}/vsock.sock");
        // Boot-specific resources (boot-source) are baked into the snapshot
        // and must NOT be configured before a load. The machine shape must
        // match the snapshot exactly.
        vm.api_put(
            "/machine-config",
            &MachineConfig {
                vcpu_count: resources.vcpu_count,
                mem_size_mib: resources.mem_mib,
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
            &VsockDevice {
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
        let mut vm = Self::spawn_api(firecracker_path, work_dir)?;
        vm.api_put(
            "/snapshot/load",
            &snapshot_load_body(vmstate_path, mem_file_path),
        )?;
        vm.set_vsock_uds_path(baked);
        vm.vsock_uds_shared = true;
        Ok(vm)
    }

    fn set_vsock_uds_path(&mut self, path: String) {
        self.vsock_uds_path = Some(path);
    }

    /// Spawn a Firecracker process and wait for its API socket.
    fn spawn_api(firecracker_path: &str, work_dir: &str) -> Result<Self> {
        let socket_path = format!("{}/firecracker.sock", work_dir);
        let _ = std::fs::remove_file(&socket_path);
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
        let mut command = Command::new(firecracker_path);
        command
            .args(["--api-sock", &socket_path])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log));
        // The VM must not outlive this process: without PDEATHSIG a SIGKILLed
        // host leaves an orphan Firecracker holding the snapshot dir and its
        // 100s of MiB of guest memory. The single shared helper concentrates
        // the unsafe; every Firecracker spawn goes through it.
        crate::firecracker::attach_pdeathsig(&mut command)?;
        let process = command
            .spawn()
            .map_err(|e| FirecrackerError::Protocol(format!("Failed to start Firecracker: {e}")))?;
        let mut process = ChildGuard::new(process);

        let start = Instant::now();
        loop {
            if Path::new(&socket_path).exists() {
                break;
            }
            // Fail in milliseconds when the binary died at spawn (bad flags,
            // exec format, seccomp) instead of burning the whole socket
            // deadline with a misleading message.
            if let Some(status) = process.exited() {
                return Err(FirecrackerError::Protocol(format!(
                    "Firecracker exited before opening its API socket: {status} (log: {log_path})"
                )));
            }
            if start.elapsed() > Duration::from_secs(5) {
                return Err(FirecrackerError::Protocol(
                    "Firecracker socket didn't appear".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // The socket path appearing does not mean the API is accepting
        // connections yet. Poll a real connect in 1 ms steps for up to 50 ms
        // instead of sleeping a fixed 50 ms: a ready API starts configuration
        // immediately, and a stalled one still falls through to the original
        // behavior (the following api_put reports the failure).
        let ready_deadline = Instant::now() + Duration::from_millis(50);
        loop {
            let connected = match tokio::runtime::Handle::try_current() {
                Ok(handle) => handle
                    .block_on(tokio::net::UnixStream::connect(&socket_path))
                    .is_ok(),
                // No runtime in this thread (pure synchronous caller): a
                // plain blocking connect still proves the listener is up.
                Err(_) => std::os::unix::net::UnixStream::connect(&socket_path).is_ok(),
            };
            if connected || Instant::now() >= ready_deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        Ok(Self {
            process: process.take()?,
            socket_path,
            vsock_uds_path: None,
            vsock_uds_shared: false,
        })
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

impl FirecrackerVm {
    /// Host-side vsock relay UDS this VM was booted with, when any.
    pub fn vsock_uds_path(&self) -> Option<&str> {
        self.vsock_uds_path.as_deref()
    }

    /// OS process id of the backing Firecracker child. Used by tests to
    /// assert teardown deterministically without counting global processes.
    pub fn id(&self) -> u32 {
        self.process.id()
    }

    /// Boot a VM with a vsock device so the guest runtime can be driven over
    /// the Firecracker UDS relay.
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
            resources,
            init_path,
            Some(VsockConfig {
                guest_cid,
                uds_path: vsock_path,
            }),
        )
    }
}

impl Drop for FirecrackerVm {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(path) = &self.vsock_uds_path {
            // 裸恢复共享的名字不删——见 vsock_uds_shared。
            if !self.vsock_uds_shared {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
