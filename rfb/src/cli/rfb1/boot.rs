//! Firecracker VM boot for the RFB1 runtime: version gate, rootfs contract,
//! API configuration, and vsock UDS relay placement.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::tool::{tool_available, tool_command};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Defaults carried from `acceptance-rfb-runtime-vsock.sh`.
pub const DEFAULT_VSOCK_UDS: &str = "/tmp/rfb-cli-rfb1.vsock";
/// Default guest CID for RFB1 acceptance.
pub const DEFAULT_CID: u32 = 52;
/// Default RFB1 guest port.
pub const DEFAULT_PORT: u16 = 5000;
/// Default Linux kernel path.
pub const DEFAULT_KERNEL: &str = "/boot/vmlinux";
/// Default RFB1 rootfs path.
pub const DEFAULT_ROOTFS: &str = "/tmp/rfb-runtime-rootfs.ext4";

/// Validate that a Firecracker binary reports a v1.12.x version (the first
/// line of `firecracker --version`). Older releases fail at API stage.
pub fn firecracker_version_ok(firecracker: &str) -> bool {
    let Ok(output) = Command::new(firecracker).arg("--version").output() else {
        return false;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    text.lines()
        .next()
        .map(|line| line.contains("v1.12."))
        .unwrap_or(false)
}

/// Validate the guest rootfs contract (protocol marker + entrypoint) with
/// debugfs, mirroring the script's `cat`/`stat` gates.
pub fn validate_rootfs_contract(image: &Path) -> Result<(), CliError> {
    if !image.is_file() {
        return Err(validation(format!(
            "rootfs is not a readable file: {}",
            image.display()
        )));
    }
    let marker = Command::new("debugfs")
        .args(["-R", "cat /etc/rfb-runtime/protocol-version"])
        .arg(image)
        .output()
        .map_err(|error| external(format!("debugfs failed: {error}")))?;
    if !marker.status.success() {
        return Err(validation(format!(
            "rootfs protocol marker is unreadable: {}",
            image.display()
        )));
    }
    let marker_text = String::from_utf8_lossy(&marker.stdout);
    if marker_text.trim() != "1" {
        return Err(validation(format!(
            "rootfs protocol marker must be exactly 1 (got {:?})",
            marker_text.trim()
        )));
    }
    let stat = Command::new("debugfs")
        .args(["-R", "stat /sbin/rfb-runtime"])
        .arg(image)
        .output()
        .map_err(|error| external(format!("debugfs stat failed: {error}")))?;
    if !stat.status.success() {
        return Err(validation("rootfs lacks /sbin/rfb-runtime entrypoint"));
    }
    let stat_text = String::from_utf8_lossy(&stat.stdout);
    let regular = stat_text
        .lines()
        .any(|line| line.contains("Type:") && (line.contains("regular") || line.contains("file")));
    let executable = stat_text.contains("0755") || stat_text.contains("-rwxr-xr-x");
    if !regular || !executable {
        return Err(validation(format!(
            "rootfs entrypoint is not an executable regular file (regular={regular} executable={executable})"
        )));
    }
    Ok(())
}

/// Parameters for booting a Firecracker VM behind the vsock UDS relay.
///
/// `init_path` and `vsock_flag` let the same boot code serve both the RFB1
/// runtime (`/sbin/rfb-runtime vsock`) and the ZeroBoot vortex guest (`/init`,
/// no `vsock` keyword). Returns the child process.
pub struct BootOptions<'a> {
    /// Firecracker executable path.
    pub firecracker: &'a str,
    /// Guest kernel image path.
    pub kernel: &'a Path,
    /// Guest root filesystem path.
    pub rootfs: &'a Path,
    /// Working directory for sockets and logs.
    pub work_dir: &'a Path,
    /// Guest vsock CID.
    pub cid: u32,
    /// Host-side vsock relay socket path.
    pub uds: &'a Path,
    /// Firecracker log path.
    pub log: &'a Path,
    /// Guest init executable path.
    pub init_path: &'a str,
    /// Whether to append the RFB1 `vsock` boot argument.
    pub vsock_flag: bool,
}

/// Start a Firecracker VM with the given kernel/rootfs and vsock relay, and wait
/// for the UDS to appear.
pub fn boot_firecracker_with(options: BootOptions<'_>) -> Result<Child, CliError> {
    let BootOptions {
        firecracker,
        kernel,
        rootfs,
        work_dir,
        cid,
        uds,
        log,
        init_path,
        vsock_flag,
    } = options;
    if !tool_available(firecracker) {
        return Err(validation(format!(
            "firecracker binary not found: {firecracker}"
        )));
    }
    if !firecracker_version_ok(firecracker) {
        return Err(validation(
            "Firecracker v1.12.x is required (older releases fail at API stage)",
        ));
    }
    fs::create_dir_all(work_dir).map_err(|error| io(error.to_string()))?;
    let _ = fs::remove_file(uds);
    let socket = work_dir.join("firecracker.sock");
    let _ = fs::remove_file(&socket);
    let log_file = fs::File::create(log).map_err(|error| io(error.to_string()))?;

    let child = Command::new(tool_command(firecracker))
        .args(["--api-sock", socket.to_str().unwrap_or("firecracker.sock")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|error| external(format!("failed to start firecracker: {error}")))?;

    // Wait for the API socket (matching the script's 50 * 100ms polling).
    let deadline = Instant::now() + Duration::from_secs(10);
    let api_sock = socket.clone();
    while !api_sock.exists() {
        if Instant::now() > deadline {
            return Err(external("Firecracker API socket did not become ready"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let vsock_flag_present = vsock_flag;
    // Configure the VM via the API: boot source, rootfs drive (rw), machine
    // config, vsock, then InstanceStart.
    let mut boot_args = rfb_runtime::boot_args::BootArgs::new()
        .random_trust_cpu()
        .root_rw("/dev/vda")
        .init(init_path);
    if vsock_flag_present {
        boot_args = boot_args.vsock();
    }
    api_put(
        &api_sock,
        "/boot-source",
        &json!({
            "kernel_image_path": kernel.to_string_lossy(),
            "boot_args": boot_args.finish(),
        }),
    )?;
    api_put(
        &api_sock,
        "/drives/rootfs",
        &json!({
            "drive_id": "rootfs",
            "path_on_host": rootfs.to_string_lossy(),
            "is_root_device": true,
            "is_read_only": false,
        }),
    )?;
    api_put(
        &api_sock,
        "/machine-config",
        &json!({
            "vcpu_count": 1,
            "mem_size_mib": 512,
            "smt": false,
        }),
    )?;
    api_put(
        &api_sock,
        "/vsock",
        &json!({
            "guest_cid": cid,
            "uds_path": uds.to_string_lossy(),
        }),
    )?;
    api_put(
        &api_sock,
        "/actions",
        &json!({"action_type": "InstanceStart"}),
    )?;

    // Wait for the vsock UDS (20 * 100ms).
    let deadline = Instant::now() + Duration::from_secs(5);
    while !uds.exists() {
        if Instant::now() > deadline {
            return Err(external("vsock UDS was not created after InstanceStart"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(child)
}

/// Boot a Firecracker VM for the RFB1 runtime (init=/sbin/rfb-runtime vsock).
pub fn boot_firecracker(
    firecracker: &str,
    kernel: &Path,
    rootfs: &Path,
    work_dir: &Path,
    cid: u32,
    uds: &Path,
    log: &Path,
) -> Result<Child, CliError> {
    boot_firecracker_with(BootOptions {
        firecracker,
        kernel,
        rootfs,
        work_dir,
        cid,
        uds,
        log,
        init_path: "/sbin/rfb-runtime",
        vsock_flag: true,
    })
}

fn api_put(socket: &Path, path: &str, body: &Value) -> Result<(), CliError> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| external(format!("connect Firecracker API: {error}")))?;
    let payload = serde_json::to_string(body).map_err(|error| io(error.to_string()))?;
    let request = format!(
        "PUT {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| external(format!("write Firecracker API: {error}")))?;
    // On Linux, share the loop-based HTTP reader with the zeroboot driver:
    // a single `read` never guarantees a complete HTTP response, and success
    // must be parsed from the status line, never substring-matched.
    #[cfg(target_os = "linux")]
    {
        let (status, text) = crate::firecracker::read_response(&mut stream)
            .map_err(|error| external(format!("read Firecracker API: {error}")))?;
        if !(200..300).contains(&status) {
            return Err(external(format!(
                "Firecracker API error on PUT {path}: HTTP {status} {text}"
            )));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::io::Read as _;
        let mut response = [0u8; 512];
        let n = stream
            .read(&mut response)
            .map_err(|error| external(format!("read Firecracker API: {error}")))?;
        let text = String::from_utf8_lossy(&response[..n]);
        if !(text.contains("200") || text.contains("204")) {
            return Err(external(format!("Firecracker API rejected {path}: {text}")));
        }
    }
    Ok(())
}
