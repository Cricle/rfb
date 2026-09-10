// Snapshot/restore scaffolding is not yet reachable from the CLI; only the
// rfb-cli `zeroboot verify` path (boot_with_runtime) is live. Keep the
// capability for upcoming snapshot workflows without dead-code noise.
use anyhow::{bail, Context, Result};
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
        self.0.take().context("child guard already consumed")
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
    let status_line = headers
        .lines()
        .next()
        .context("Firecracker returned an empty response")?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    anyhow::ensure!(
        version.starts_with("HTTP/1."),
        "Firecracker returned a non-HTTP/1.x status line: {status_line:?}"
    );
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .with_context(|| {
            format!("Firecracker returned an unparsable status line: {status_line:?}")
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
        let n = stream
            .read(&mut chunk)
            .context("read Firecracker API response")?;
        if n == 0 {
            bail!("Firecracker closed the connection before completing the response");
        }
        buf.extend_from_slice(&chunk[..n]);
        anyhow::ensure!(
            buf.len() <= MAX_RESPONSE_BYTES,
            "Firecracker response exceeded {MAX_RESPONSE_BYTES} bytes before header end"
        );
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let (status, content_length) = parse_response_head(&header_text)?;
    anyhow::ensure!(
        header_end.saturating_add(content_length) <= MAX_RESPONSE_BYTES,
        "Firecracker response body exceeds {MAX_RESPONSE_BYTES} bytes"
    );
    while buf.len() < header_end + content_length {
        let n = stream
            .read(&mut chunk)
            .context("read Firecracker API response body")?;
        if n == 0 {
            bail!("Firecracker closed the connection before sending the full response body");
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

pub struct FirecrackerVm {
    process: Child,
    socket_path: String,
    vsock_uds_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VsockConfig {
    pub guest_cid: u32,
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

#[derive(Serialize)]
struct VmAction {
    action_type: String,
}

impl FirecrackerVm {
    fn boot_internal(
        firecracker_path: &str,
        kernel_path: &str,
        rootfs_path: &str,
        work_dir: &str,
        mem_mib: u32,
        init_path: &str,
        vsock: Option<VsockConfig>,
    ) -> Result<Self> {
        let socket_path = format!("{}/firecracker.sock", work_dir);

        // Clean up
        let _ = std::fs::remove_file(&socket_path);

        // Start Firecracker
        eprintln!("Starting Firecracker...");
        let log_path = format!("{work_dir}/firecracker.log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create Firecracker log at {log_path}"))?;
        let process = Command::new(firecracker_path)
            .args(["--api-sock", &socket_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .context("Failed to start Firecracker")?;
        let mut process = ChildGuard::new(process);

        // Wait for socket
        let start = Instant::now();
        while !Path::new(&socket_path).exists() {
            if start.elapsed() > Duration::from_secs(5) {
                bail!("Firecracker socket didn't appear");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));

        let vsock_uds_path = vsock.as_ref().map(|v| v.uds_path.clone());
        let vm = Self {
            process: process.take()?,
            socket_path,
            vsock_uds_path,
        };

        // Configure machine
        vm.api_put(
            "/machine-config",
            &MachineConfig {
                vcpu_count: 1,
                mem_size_mib: mem_mib,
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
            vm.api_put(
                "/vsock",
                &VsockDevice {
                    guest_cid: vsock.guest_cid,
                    uds_path: vsock.uds_path,
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

        eprintln!("Firecracker VM started");
        Ok(vm)
    }

    fn api_put<T: Serialize>(&self, path: &str, body: &T) -> Result<String> {
        self.api_request("PUT", path, body)
    }

    fn api_request<T: Serialize>(&self, method: &str, path: &str, body: &T) -> Result<String> {
        let body_json = serde_json::to_string(body)?;
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            method, path, body_json.len(), body_json
        );

        let mut stream = UnixStream::connect(&self.socket_path)
            .with_context(|| format!("Connect to Firecracker socket at {}", self.socket_path))?;
        stream.set_read_timeout(Some(FC_SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(FC_SOCKET_TIMEOUT))?;

        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let (status, resp) = read_response(&mut stream)?;
        if !(200..300).contains(&status) {
            bail!(
                "Firecracker API error on {} {}: HTTP {} {}",
                method,
                path,
                status,
                resp
            );
        }

        Ok(resp)
    }

    pub fn kill(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl FirecrackerVm {
    pub fn vsock_uds_path(&self) -> Option<&str> {
        self.vsock_uds_path.as_deref()
    }

    /// OS process id of the backing Firecracker child. Used by tests to
    /// assert teardown deterministically without counting global processes.
    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn boot_with_runtime(
        firecracker_path: &str,
        kernel_path: &str,
        rootfs_path: &str,
        work_dir: &str,
        mem_mib: u32,
        init_path: &str,
        guest_cid: u32,
    ) -> Result<Self> {
        let vsock_path = format!("{work_dir}/vsock.sock");
        Self::boot_internal(
            firecracker_path,
            kernel_path,
            rootfs_path,
            work_dir,
            mem_mib,
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
            let _ = std::fs::remove_file(path);
        }
    }
}
