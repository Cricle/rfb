//! ZeroBoot ZBRT real-VM acceptance and benchmark.
//!
//! Converges the former `tests/native_vsock_verify.sh`: boots a real
//! Firecracker VM with a HANDOFF guest rootfs, then exchanges ZBRT frames over
//! the host-side vsock UDS relay — echo/true/false, unsupported-command error,
//! concurrent connections with isolated request_ids, malformed Execute, and an
//! optional 100-sample round-trip benchmark. Only sanitized aggregates are
//! reported; payloads are never logged.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::host::detect;
use crate::cli::image_build::sha256;
use crate::cli::rfb1::{boot_firecracker_with, connect_vsock_uds, BootOptions};
use crate::cli::tool::HostKind;
use crate::protocol::{
    Error as ProtocolError, Execute, Frame, Hello, HelloAck, Kind, Output, ZBRT_V1_CAPABILITIES,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::Child,
    time::{Duration, Instant},
};

#[cfg(unix)]
use clap::Subcommand;

/// ZeroBoot ZBRT real-VM verification subcommands.
#[cfg(unix)]
#[derive(Subcommand, Debug)]
pub enum ZerobootCommand {
    /// Run ZeroBoot ZBRT VERSION=1 full-protocol verification (echo/true/false/concurrent/malformed).
    Verify(ZerobootVerifyArgs),
    /// Keep ZeroBoot sandbox VMs running and bridge each guest's ZBRT port to
    /// a local TCP listener (one port per sandbox), so plain TCP SDK clients
    /// drive the guests directly — no controller, no TAP, no forkd.
    Up(ZerobootUpArgs),
    /// Provider-level microbenchmark: create/health/exec/cleanup quantiles.
    Benchmark(ZerobootBackendArgs),
    /// Multi-sandbox business workload: exec/health/fs rounds, session reuse,
    /// handoff digest, leak-free teardown.
    Workload(ZerobootBackendArgs),
    /// Host/KVM/Firecracker/kernel/rootfs-contract checks.
    Preflight(ZerobootBackendArgs),
}
/// Shared paths for the zeroboot provider gates (benchmark/workload/preflight).
#[cfg(unix)]
#[derive(clap::Args, Debug)]
pub struct ZerobootBackendArgs {
    /// Guest kernel image (vmlinux).
    #[arg(long, value_name = "VMLINUX")]
    pub kernel: PathBuf,
    /// ZeroBoot rootfs ext4 (zeroboot-zbrt mode).
    #[arg(long, value_name = "EXT4")]
    pub rootfs: PathBuf,
    /// Firecracker executable.
    #[arg(long, default_value = "firecracker")]
    pub firecracker: String,
    /// Benchmark/workload iterations (create cycles / reuse execs).
    #[arg(long, default_value_t = 5)]
    pub iterations: usize,
    /// Workload: number of sandboxes.
    #[arg(long, default_value_t = 2)]
    pub sandboxes: usize,
    /// Workload: rounds per sandbox.
    #[arg(long, default_value_t = 2)]
    pub rounds: usize,
    /// Preflight: fail closed when any VM check fails.
    #[arg(long)]
    pub require_vm: bool,
}
/// Arguments for `rfb-cli zeroboot up`.
#[cfg(unix)]
#[derive(clap::Args, Debug)]
pub struct ZerobootUpArgs {
    /// Guest kernel image (vmlinux).
    #[arg(long, value_name = "VMLINUX")]
    pub kernel: PathBuf,
    /// Ready ZeroBoot rootfs ext4 (mutually exclusive with --pid1-dir).
    #[arg(long, value_name = "EXT4")]
    pub rootfs: Option<PathBuf>,
    /// Directory holding the pid1 binaries (rfb-runtime + rfb-mini-tools +
    /// rfb-busybox beside each other); the ZeroBoot rootfs is built from
    /// them into this directory — no cargo needed.
    #[arg(long, value_name = "DIR")]
    pub pid1_dir: Option<PathBuf>,
    /// Install /bin/python3 (requires the pid1 built with the rustpython
    /// feature).
    #[arg(long)]
    pub with_python: bool,
    /// Install /bin/lua (requires the pid1 built with the mlua feature).
    #[arg(long)]
    pub with_lua: bool,
    /// Firecracker executable.
    #[arg(long, default_value = "firecracker")]
    pub firecracker: String,
    /// TCP listen address the ZBRT guest is bridged to.
    #[arg(long, default_value = "127.0.0.1:5000")]
    pub tcp: String,
    /// Number of sandbox VMs to run, each bridged to its own TCP port
    /// (base port, base+1, ...). Each VM is one independent sandbox.
    #[arg(long, default_value_t = 1)]
    pub n: usize,
    /// Guest vsock port the ZBRT listener binds (zeroboot-guest-port).
    #[arg(long, default_value_t = 5000)]
    pub guest_port: u32,
    /// Guest CID for the vsock device.
    #[arg(long, default_value_t = 3)]
    pub cid: u32,
}
/// Arguments for the ZeroBoot ZBRT real-VM verification.
#[cfg(unix)]
#[derive(clap::Args, Debug)]
pub struct ZerobootVerifyArgs {
    /// Kernel image path (default `/boot/vmlinux`).
    #[arg(long, default_value = "/boot/vmlinux")]
    pub kernel: PathBuf,
    /// Rootfs ext4 image path (default `/tmp/rootfs.ext4`).
    #[arg(long, default_value = "/tmp/rootfs.ext4")]
    pub rootfs: PathBuf,
    /// Firecracker binary to use (default `firecracker`).
    #[arg(long, default_value = "firecracker")]
    pub firecracker: String,
    /// Exit non-zero if prerequisites are missing.
    #[arg(long, help = "Exit non-zero if prerequisites are missing")]
    pub require_vm: bool,
    /// Run the 100-sample round-trip benchmark.
    #[arg(long, help = "Run the 100-sample round-trip benchmark")]
    pub bench: bool,
}

fn readable_file(path: &Path) -> bool {
    path.is_file() && fs::File::open(path).is_ok()
}

/// Top-level dispatch for the zeroboot subcommands. Lives here (not in
/// `cli/dispatch.rs`) so the whole zeroboot command surface — types, match,
/// and implementations — is one deletable unit for the forkd-only build.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
#[cfg(unix)]
pub fn dispatch(json_out: bool, command: ZerobootCommand) -> Result<(), CliError> {
    use crate::cli::dispatch::block_on;
    use crate::cli::error::render_output;
    use crate::cli::zeroboot_backend;

    match command {
        ZerobootCommand::Verify(args) => {
            let value = verify(
                &args.kernel,
                &args.rootfs,
                &args.firecracker,
                args.require_vm,
                args.bench,
            )?;
            render_output(json_out, value, "zeroboot verify".to_owned());
            Ok(())
        }
        ZerobootCommand::Up(args) => {
            let value = up(&args)?;
            render_output(json_out, value, "zeroboot stopped".to_owned());
            Ok(())
        }
        ZerobootCommand::Benchmark(args) => {
            let value = block_on(zeroboot_backend::benchmark(&args, args.iterations))?;
            render_output(json_out, value, "zeroboot benchmark".to_owned());
            Ok(())
        }
        ZerobootCommand::Workload(args) => {
            let value = block_on(zeroboot_backend::workload(
                &args,
                args.sandboxes,
                args.rounds,
            ))?;
            render_output(json_out, value, "zeroboot workload".to_owned());
            Ok(())
        }
        ZerobootCommand::Preflight(args) => {
            let value = block_on(zeroboot_backend::preflight(&args, args.require_vm))?;
            render_output(json_out, value, "zeroboot preflight".to_owned());
            Ok(())
        }
    }
}

/// Defaults carried from `tests/native_vsock_verify.sh`.
pub const DEFAULT_PORT: u16 = 5000;
/// Default guest CID for ZeroBoot verification.
pub const DEFAULT_CID: u32 = 3;

/// Complete the `CONNECT <port>\n` vsock UDS handshake and return the stream.
///
/// Firecracker creates the relay socket before the guest has finished binding
/// its vsock listener. Retry transient EOF/refusal responses during that boot
/// window instead of treating the expected startup race as a protocol failure.
fn connect(uds: &Path, port: u16) -> Result<UnixStream, CliError> {
    connect_vsock_uds(uds, port, Duration::from_secs(5))
}

fn read_exact(stream: &mut UnixStream, n: usize) -> Result<Vec<u8>, CliError> {
    let mut data = vec![0u8; n];
    stream
        .read_exact(&mut data)
        .map_err(|error| external(format!("vsock EOF after {n} bytes: {error}")))?;
    Ok(data)
}

/// Read one response payload after validating its wire-declared length. The
/// length field is attacker-controlled, so it is capped at the protocol's
/// `MAX_PAYLOAD` before allocation; anything larger fails closed instead of
/// materializing up to a 4 GiB buffer.
fn read_payload(stream: &mut UnixStream, len: usize) -> Result<Vec<u8>, CliError> {
    if len > crate::protocol::MAX_PAYLOAD {
        return Err(validation(format!(
            "ZBRT payload length {len} exceeds MAX_PAYLOAD {}",
            crate::protocol::MAX_PAYLOAD
        )));
    }
    read_exact(stream, len)
}

/// Decode a ZBRT response frame header + payload, validating magic/version and
/// echoing the request_id back (proving no cross-talk between connections).
fn decode_response(
    header: &[u8],
    payload: Vec<u8>,
    request_id: [u8; 16],
) -> Result<(Kind, Vec<u8>), CliError> {
    if header.len() < crate::protocol::HEADER_LEN
        || header[0..4] != crate::protocol::MAGIC
        || header[4] != crate::protocol::VERSION
    {
        return Err(validation(format!(
            "bad ZBRT magic/version: {:?}",
            &header[..header.len().min(8)]
        )));
    }
    let got_rid: [u8; 16] = header[8..24]
        .try_into()
        .map_err(|_| validation("invalid request_id length"))?;
    if got_rid != request_id {
        return Err(validation(format!(
            "request_id mismatch: got {got_rid:?} expected {request_id:?}"
        )));
    }
    let kind_byte = header[5];
    // Single-sourced kind table (rfb-runtime zeroboot_protocol); unknown
    // bytes fail closed here.
    let kind = crate::protocol::Kind::parse(kind_byte)
        .map_err(|error| validation(format!("unknown ZBRT kind: {kind_byte}: {error}")))?;
    Ok((kind, payload))
}

/// Mandatory ZBRT handshake on a fresh connection (PROTOCOL.md §3.4): the
/// guest refuses every non-Hello frame with `Error("protocol handshake
/// required")`, so each raw client sends Hello and requires the matching
/// HelloAck before any business frame.
fn handshake(stream: &mut UnixStream, name: &str) -> Result<(), CliError> {
    let mut request_id = [0u8; 16];
    let label = format!("{name}:hello").into_bytes();
    request_id[..label.len().min(16)].copy_from_slice(&label[..label.len().min(16)]);
    let hello = Frame {
        kind: Kind::Hello,
        flags: 0,
        request_id,
        payload: Hello {
            client: "rfb-cli".to_owned(),
            capabilities: ZBRT_V1_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
        }
        .encode()
        .map_err(|error| external(format!("{name}: encode Hello: {error}")))?,
    };
    hello
        .encode(&mut *stream)
        .map_err(|error| external(format!("{name}: write Hello: {error}")))?;
    let header = read_exact(stream, crate::protocol::HEADER_LEN)?;
    let len = u32::from_be_bytes(header[24..28].try_into().unwrap()) as usize;
    let payload = read_payload(stream, len)?;
    let (kind, payload) = decode_response(&header, payload, request_id)?;
    if kind != Kind::HelloAck {
        return Err(validation(format!(
            "{name}: expected HelloAck, got {kind:?}"
        )));
    }
    HelloAck::decode(&payload)
        .map_err(|error| validation(format!("{name}: invalid HelloAck: {error}")))?;
    Ok(())
}

/// One ZBRT exchange on a fresh vsock connection. `code` is the guest command;
/// the Execute frame carries a 4-byte big-endian deadline (ms) followed by the
/// command. Returns (kind, payload) without logging payload contents.
fn exchange(
    uds: &Path,
    port: u16,
    name: &str,
    code: &[u8],
    deadline_ms: u32,
) -> Result<(Kind, Vec<u8>), CliError> {
    let mut request_id = [0u8; 16];
    let name_bytes = name.as_bytes();
    request_id[..name_bytes.len().min(16)].copy_from_slice(&name_bytes[..name_bytes.len().min(16)]);

    let command = std::str::from_utf8(code)
        .map_err(|_| validation(format!("{name}: command is not UTF-8")))?;
    let request = Execute {
        argv: command.split_whitespace().map(str::to_owned).collect(),
        cwd: None,
        stdin: Vec::new(),
        timeout_ms: deadline_ms,
    };
    if request.argv.is_empty() {
        return Err(validation(format!("{name}: command is empty")));
    }

    let mut stream = connect(uds, port)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|error| io(error.to_string()))?;
    handshake(&mut stream, name)?;
    let frame = Frame {
        kind: Kind::Execute,
        flags: 0,
        request_id,
        payload: request
            .encode()
            .map_err(|error| external(format!("encode Execute frame: {error}")))?,
    };
    frame
        .encode(&mut stream)
        .map_err(|error| external(format!("write Execute frame: {error}")))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let header = read_exact(&mut stream, crate::protocol::HEADER_LEN)?;
        let len = u32::from_be_bytes(header[24..28].try_into().unwrap()) as usize;
        let payload = read_payload(&mut stream, len)?;
        let (kind, payload) = decode_response(&header, payload, request_id)?;
        match kind {
            Kind::Output => {
                let output = Output::decode(&payload)
                    .map_err(|error| validation(format!("{name}: invalid Output: {error}")))?;
                // Same aggregate cap as the provider/SDK clients: a broken or
                // hostile guest must not OOM the CLI through unbounded output
                // accumulation.
                if stdout
                    .len()
                    .saturating_add(stderr.len())
                    .saturating_add(output.data.len())
                    > crate::protocol::MAX_PAYLOAD
                {
                    return Err(validation(format!(
                        "{name}: guest output exceeded the {} MiB limit",
                        crate::protocol::MAX_PAYLOAD / (1024 * 1024)
                    )));
                }
                if output.stream == 0 {
                    stdout.extend(output.data);
                } else {
                    stderr.extend(output.data);
                }
            }
            Kind::Exit => {
                let exit = crate::protocol::Exit::decode(&payload)
                    .map_err(|error| validation(format!("{name}: invalid Exit: {error}")))?;
                let mut result = Vec::new();
                result.extend_from_slice(&exit.code.to_be_bytes());
                result.extend_from_slice(&(stdout.len() as u32).to_be_bytes());
                result.extend_from_slice(&(stderr.len() as u32).to_be_bytes());
                result.extend_from_slice(&stdout);
                result.extend_from_slice(&stderr);
                return Ok((Kind::Result, result));
            }
            Kind::Result => return Ok((Kind::Result, payload)),
            Kind::Error => {
                let error = ProtocolError::decode(&payload)
                    .map_err(|error| validation(format!("{name}: invalid Error: {error}")))?;
                return Ok((
                    Kind::Error,
                    error.encode().map_err(|e| external(e.to_string()))?,
                ));
            }
            other => return Err(validation(format!("{name}: unexpected response {other:?}"))),
        }
    }
}

/// Validate the result payload shape for the simple command contract:
/// `exit_code:i32, stdout_len:u32, stderr_len:u32` then stdout/stderr bytes.
/// Delegates to the canonical decoder in the provider module.
fn parse_result(payload: &[u8], name: &str) -> Result<(i32, Vec<u8>, Vec<u8>), CliError> {
    crate::zeroboot::parse_legacy_result(payload)
        .map_err(|error| validation(format!("{name}: {error}")))
}

/// Full ZBRT acceptance: echo/true/false, unsupported error, bounded deadline,
/// 8 concurrent connections, malformed Execute, and optional 100-sample bench.
/// Boots a real Firecracker VM (KVM) and never touches external state.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn verify(
    kernel: &Path,
    rootfs: &Path,
    firecracker: &str,
    require_vm: bool,
    bench: bool,
) -> Result<Value, CliError> {
    let caps = detect();
    let linux = matches!(caps.kind, HostKind::Linux | HostKind::Wsl);
    if !linux || !caps.kvm {
        if require_vm {
            return Err(crate::cli::error::no_vm(
                "Linux/KVM are required for ZBRT verify",
            ));
        }
        return Ok(json!({"status": "skipped", "reason": "Linux/KVM unavailable"}));
    }
    if !readable_file(kernel) || !readable_file(rootfs) {
        return Err(validation(
            "kernel and rootfs must be readable files; use image build-rootfs --mode zeroboot-zbrt",
        ));
    }
    if !crate::cli::tool::tool_available("debugfs") {
        return Err(external(
            "debugfs is required to validate the ZeroBoot rootfs",
        ));
    }

    // Validate the ZeroBoot rootfs contract before starting a VM. This is
    // deliberately read-only and rejects legacy RFB1/forkd artifacts.
    let init = std::process::Command::new("debugfs")
        .args(["-R", "stat /init"])
        .arg(rootfs)
        .output()
        .map_err(|error| external(format!("debugfs preflight failed: {error}")))?;
    let init_text = String::from_utf8_lossy(&init.stdout);
    if !init.status.success()
        || !init_text.contains("Type:")
        || !init_text.contains("regular")
        || !(init_text.contains("0755") || init_text.contains("-rwxr-xr-x"))
    {
        return Err(validation("ZeroBoot rootfs must contain executable regular /init; use image build-rootfs --mode zeroboot-zbrt"));
    }
    for marker in [
        "/etc/zeroboot-protocol",
        "/etc/zeroboot-protocol-version",
        "/etc/zeroboot-capabilities",
        "/etc/zeroboot-guest-port",
    ] {
        let check = std::process::Command::new("debugfs")
            .args(["-R", &format!("stat {marker}")])
            .arg(rootfs)
            .output()
            .map_err(|error| external(format!("debugfs preflight failed: {error}")))?;
        if !check.status.success() {
            return Err(validation(format!(
                "ZeroBoot rootfs is missing {marker}; use image build-rootfs --mode zeroboot-zbrt"
            )));
        }
    }
    let protocol_marker = std::process::Command::new("debugfs")
        .args(["-R", "cat /etc/zeroboot-protocol"])
        .arg(rootfs)
        .output()
        .map_err(|error| external(error.to_string()))?;
    let version_marker = std::process::Command::new("debugfs")
        .args(["-R", "cat /etc/zeroboot-protocol-version"])
        .arg(rootfs)
        .output()
        .map_err(|error| external(error.to_string()))?;
    let capabilities_marker = std::process::Command::new("debugfs")
        .args(["-R", "cat /etc/zeroboot-capabilities"])
        .arg(rootfs)
        .output()
        .map_err(|error| external(error.to_string()))?;
    let guest_port = std::process::Command::new("debugfs")
        .args(["-R", "cat /etc/zeroboot-guest-port"])
        .arg(rootfs)
        .output()
        .map_err(|error| external(error.to_string()))?;
    if !protocol_marker.status.success()
        || String::from_utf8_lossy(&protocol_marker.stdout).trim() != "zbrt"
        || !version_marker.status.success()
        || String::from_utf8_lossy(&version_marker.stdout).trim() != "1"
        || !capabilities_marker.status.success()
        || String::from_utf8_lossy(&capabilities_marker.stdout).trim()
            != crate::protocol::ZBRT_V1_CAPABILITIES.join(",")
        || !guest_port.status.success()
        || String::from_utf8_lossy(&guest_port.stdout).trim() != "5000"
    {
        return Err(validation("ZeroBoot rootfs marker/guest port is invalid; use image build-rootfs --mode zeroboot-zbrt"));
    }

    // ZBRT verification must not silently boot the legacy RFB1 rootfs image.
    // Inspect only metadata with debugfs; do not modify or infer a runtime.
    if let Ok(output) = std::process::Command::new("debugfs")
        .args(["-R", "cat /etc/rfb-runtime/protocol-version"])
        .arg(rootfs)
        .output()
    {
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "1" {
            return Err(validation(
                "zeroboot verify refuses an RFB1 rfb-runtime rootfs; use a ZeroBoot rootfs",
            ));
        }
    }
    let rootfs_digest = sha256(rootfs).map_err(|error| io(error.to_string()))?;
    let kernel_digest = sha256(kernel).map_err(|error| io(error.to_string()))?;

    let work_dir = std::env::temp_dir().join(format!("rfb-cli-zbrt-{}", std::process::id()));
    let uds = work_dir.join("vsock.sock");
    let log = work_dir.join("firecracker.log");
    let _ = fs::remove_dir_all(&work_dir);
    fs::create_dir_all(&work_dir).map_err(|error| io(error.to_string()))?;

    let mut child: Child = boot_firecracker_with(BootOptions {
        firecracker,
        kernel,
        rootfs,
        work_dir: &work_dir,
        cid: DEFAULT_CID,
        uds: &uds,
        log: &log,
        init_path: "/init",
        vsock_flag: false,
    })?;

    let result = (|| -> Result<Value, CliError> {
        let mut checks = Vec::new();

        // echo hello -> exit 0, stdout "hello\n"
        let (kind, payload) = exchange(&uds, DEFAULT_PORT, "echo", b"echo hello", 3000)?;
        if kind != Kind::Result {
            return Err(validation("echo: expected Result kind"));
        }
        let (exit, stdout, stderr) = parse_result(&payload, "echo")?;
        if exit != 0 || stdout != b"hello\n" || !stderr.is_empty() {
            return Err(validation(format!(
                "echo: exit={exit} stdout={stdout:?} stderr={stderr:?}"
            )));
        }
        checks.push(json!({"name": "echo", "ok": true}));

        // true -> exit 0; false -> exit 1
        for (name, code, expected) in [
            ("true", b"true".as_slice(), 0i32),
            ("false", b"false".as_slice(), 1i32),
        ] {
            let (kind, payload) = exchange(&uds, DEFAULT_PORT, name, code, 3000)?;
            if kind != Kind::Result {
                return Err(validation(format!("{name}: expected Result kind")));
            }
            let (exit, _, _) = parse_result(&payload, name)?;
            if exit != expected {
                return Err(validation(format!(
                    "{name}: exit={exit} expected {expected}"
                )));
            }
            checks.push(json!({"name": name, "ok": true}));
        }

        // unsupported command -> non-zero exit on the guest runtime
        let (kind, payload) = exchange(&uds, DEFAULT_PORT, "unsupported", b"not-a-command", 3000)?;
        if kind != Kind::Result {
            return Err(validation("unsupported: expected Result kind"));
        }
        let (exit, _, _) = parse_result(&payload, "unsupported")?;
        if exit != -1 {
            return Err(validation(format!("unsupported: exit={exit} expected -1")));
        }
        checks.push(json!({"name": "unsupported", "ok": true}));

        // bounded deadline: echo must round-trip within 5s wall clock
        let started = Instant::now();
        let (kind, _payload) =
            exchange(&uds, DEFAULT_PORT, "deadline-echo", b"echo bounded", 3000)?;
        if kind != Kind::Result {
            return Err(validation("deadline-echo: expected Result kind"));
        }
        let elapsed = started.elapsed();
        if elapsed > Duration::from_secs(5) {
            return Err(validation(format!(
                "deadline-echo exceeded 5s: {elapsed:?}"
            )));
        }
        checks.push(json!({
            "name": "deadline-echo",
            "ok": true,
            "round_trip_ms": (elapsed.as_millis() as f64) / 1000.0 * 1000.0,
        }));

        // 8 concurrent connections, each with its own request_id
        let handles: Vec<std::thread::JoinHandle<Result<usize, CliError>>> = (0..8)
            .map(|i| {
                let uds = uds.clone();
                std::thread::spawn(move || {
                    let name = format!("concurrent{i:06}");
                    let (kind, payload) =
                        exchange(&uds, DEFAULT_PORT, &name, b"echo concurrency", 3000)?;
                    if kind != Kind::Result {
                        return Err(validation("concurrent: expected Result kind"));
                    }
                    let (exit, stdout, _) = parse_result(&payload, "concurrent")?;
                    if exit != 0 || stdout != b"concurrency\n" {
                        return Err(validation(format!(
                            "concurrent{i}: exit={exit} stdout={stdout:?}"
                        )));
                    }
                    Ok(i)
                })
            })
            .collect();
        let mut concurrent_ok = true;
        for (idx, handle) in handles.into_iter().enumerate() {
            match handle.join() {
                Ok(Ok(_)) => {}
                _ => concurrent_ok = false,
            }
            let _ = idx;
        }
        if !concurrent_ok {
            return Err(validation(
                "8 concurrent connections: request_id isolation failed",
            ));
        }
        checks.push(json!({"name": "concurrent_8", "ok": true}));

        // malformed Execute -> Error frame (the handshake passes first so the
        // guest's strict decoder, not the handshake guard, rejects it)
        let mut stream = connect(&uds, DEFAULT_PORT)?;
        handshake(&mut stream, "malformed")?;
        let mut rid = [0u8; 16];
        rid[..9].copy_from_slice(b"malformed");
        let malformed = Frame {
            kind: Kind::Execute,
            flags: 0,
            request_id: rid,
            payload: b"X".to_vec(),
        };
        malformed
            .encode(&mut stream)
            .map_err(|error| external(format!("write malformed frame: {error}")))?;
        let header = read_exact(&mut stream, crate::protocol::HEADER_LEN)?;
        let len = u32::from_be_bytes(header[24..28].try_into().unwrap()) as usize;
        let payload = read_payload(&mut stream, len)?;
        if header[5] != Kind::Error as u8 {
            return Err(validation("malformed: expected Error frame"));
        }
        checks.push(json!({"name": "malformed_execute", "ok": true, "error_payload": payload}));

        // Optional 100-sample bench with 10-sample warmup, 0.15s pacing.
        let mut bench_stats = Value::Null;
        if bench {
            for _ in 0..10 {
                let _ = exchange(&uds, DEFAULT_PORT, "warmup", b"echo zb-perf", 3000);
                std::thread::sleep(Duration::from_millis(150));
            }
            let mut samples = Vec::new();
            let mut attempts = 0;
            let mut timeouts = 0;
            while samples.len() < 100 && attempts < 120 {
                let started = Instant::now();
                match exchange(&uds, DEFAULT_PORT, "zbbench", b"echo zb-perf", 3000) {
                    Ok((Kind::Result, payload)) => {
                        let (exit, _, _) = parse_result(&payload, "zbbench")?;
                        if exit == 0 {
                            samples.push(started.elapsed().as_nanos() as u64);
                        } else {
                            timeouts += 1;
                        }
                    }
                    _ => timeouts += 1,
                }
                attempts += 1;
                std::thread::sleep(Duration::from_millis(150));
            }
            if samples.len() < 100 {
                return Err(external(format!(
                    "ZB_BENCH insufficient samples: {} successes in {} attempts",
                    samples.len(),
                    attempts
                )));
            }
            samples.sort_unstable();
            let p = |q: f64| {
                let idx = (samples.len() as f64 - 1.0) * q;
                let lo = idx.floor() as usize;
                let hi = (idx.ceil() as usize).min(samples.len() - 1);
                samples[lo] as f64 + (samples[hi] as f64 - samples[lo] as f64) * (idx - idx.floor())
            };
            bench_stats = json!({
                "samples": samples.len(),
                "attempts": attempts,
                "timeouts": timeouts,
                "success_rate_pct": 100.0 * samples.len() as f64 / attempts as f64,
                "p50_ms": (p(0.50) / 1e6 * 1000.0).round() / 1000.0,
                "p95_ms": (p(0.95) / 1e6 * 1000.0).round() / 1000.0,
                "p99_ms": (p(0.99) / 1e6 * 1000.0).round() / 1000.0,
                "max_ms": (samples[samples.len() - 1] as f64 / 1e6 * 1000.0).round() / 1000.0,
            });
        }

        Ok(json!({
            "status": "passed",
            "rootfs_sha256": format!("sha256:{rootfs_digest}"),
            "kernel_sha256": format!("sha256:{kernel_digest}"),
            "checks": checks,
            "benchmark": bench_stats,
            "payload": "suppressed",
        }))
    })();

    // Always tear down the VM and private work directory.
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&work_dir);
    result
}

/// Keep one ZeroBoot sandbox VM running and bridge its ZBRT guest port to a
/// Keep ZeroBoot sandbox VMs running and bridge each guest's ZBRT port to its
/// own local TCP listener (`--n N`: N VMs, cid and TCP port increment per
/// instance). Every TCP connection is proxied to a fresh Firecracker
/// vsock-relay connection, so plain TCP SDK clients (rfb-sdk and all four
/// language SDKs) drive the guests directly — no controller, no TAP, no
/// forkd. Runs in the foreground; Ctrl-C (or SIGTERM) tears all VMs down.
///
/// The rootfs comes either ready (`--rootfs`) or built on the spot from pid1
/// binaries (`--pid1-dir`: rfb-runtime + rfb-mini-tools + rfb-busybox) — the
/// no-cargo path, `image build-rootfs` only assembles the ext4.
///
/// # Errors
///
/// Returns `Err` when any stage fails.
pub fn up(args: &crate::cli::zeroboot::ZerobootUpArgs) -> Result<Value, CliError> {
    let caps = detect();
    let linux = matches!(caps.kind, HostKind::Linux | HostKind::Wsl);
    if !linux || !caps.kvm {
        return Err(crate::cli::error::no_vm(
            "Linux/KVM are required for zeroboot up",
        ));
    }
    if args.n == 0 {
        return Err(validation("--n must be positive"));
    }
    let (rootfs, rootfs_source) = match (&args.rootfs, &args.pid1_dir) {
        (Some(rootfs), None) => (rootfs.clone(), "provided".to_owned()),
        (None, Some(dir)) => {
            let built = dir.join("zeroboot-sandbox.ext4");
            crate::cli::image_build::build_rootfs(
                &dir.join("rfb-runtime"),
                &built,
                None,
                "zeroboot-zbrt",
                false,
                true,
                &crate::cli::image_build::RootfsOptions {
                    with_python: args.with_python,
                    with_lua: args.with_lua,
                    py_site_dir: None,
                    lua_lib_dir: None,
                    extra_files: Vec::new(),
                },
            )?;
            (built, "built from pid1".to_owned())
        }
        (Some(_), Some(_)) => {
            return Err(validation("--rootfs and --pid1-dir are mutually exclusive"))
        }
        (None, None) => return Err(validation("one of --rootfs or --pid1-dir is required")),
    };
    if !readable_file(&rootfs) {
        return Err(validation(format!("rootfs missing: {}", rootfs.display())));
    }
    if !readable_file(&args.kernel) {
        return Err(validation(format!(
            "kernel missing: {}",
            args.kernel.display()
        )));
    }

    /// One sandbox: its VM child, the relay UDS, and the cleaned-up work dir.
    struct Vm {
        child: Child,
        uds: std::path::PathBuf,
        work_dir: std::path::PathBuf,
    }

    let tcp = args.tcp.clone();
    let host_ip = tcp
        .rsplit_once(':')
        .map(|(ip, _)| ip)
        .unwrap_or("127.0.0.1")
        .to_owned();
    let base_port: u16 = tcp
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5000);

    let mut vms: Vec<Vm> = Vec::new();
    let result = (|| -> Result<Value, CliError> {
        for i in 0..args.n {
            let work_dir =
                std::env::temp_dir().join(format!("rfb-zb-up-{}-{i}", std::process::id()));
            let _ = fs::remove_dir_all(&work_dir);
            let uds = work_dir.join("vsock.sock");
            let child = boot_firecracker_with(BootOptions {
                firecracker: &args.firecracker,
                kernel: &args.kernel,
                rootfs: &rootfs,
                work_dir: &work_dir,
                cid: args.cid + i as u32,
                uds: &uds,
                log: &work_dir.join("firecracker.log"),
                init_path: "/init",
                vsock_flag: false,
            })?;
            vms.push(Vm {
                child,
                uds,
                work_dir,
            });
        }

        // The bridges are the process's only reason to live: one tokio
        // current-thread runtime, one listener per sandbox, one relay task
        // per client connection.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| external(format!("build async runtime: {error}")))?;
        let rootfs_ref = rootfs.clone();
        let guest_port = args.guest_port;
        let n = args.n;
        let relay_uds: Vec<std::path::PathBuf> = vms.iter().map(|vm| vm.uds.clone()).collect();
        runtime.block_on(async move {
            let mut addresses = Vec::with_capacity(n);
            let mut accept_loops = Vec::with_capacity(n);
            for (i, uds) in relay_uds.iter().enumerate() {
                let addr = format!("{host_ip}:{}", base_port + i as u16);
                let listener = tokio::net::TcpListener::bind(&addr)
                    .await
                    .map_err(|error| external(format!("bind {addr}: {error}")))?;
                addresses.push(addr);
                let uds = uds.clone();
                accept_loops.push(tokio::spawn(async move {
                    loop {
                        let Ok((mut tcp_stream, _)) = listener.accept().await else {
                            continue;
                        };
                        let uds = uds.clone();
                        tokio::spawn(async move {
                            let handshake = Duration::from_secs(10);
                            let Ok(mut relay) =
                                crate::vsock::connect_firecracker_uds(&uds, guest_port, handshake)
                                    .await
                            else {
                                return;
                            };
                            let _ =
                                tokio::io::copy_bidirectional(&mut tcp_stream, &mut relay).await;
                        });
                    }
                }));
            }
            // Announce readiness only after every listener exists.
            println!(
                "{}",
                json!({
                    "ok": true,
                    "sandboxes": addresses
                        .iter()
                        .enumerate()
                        .map(|(i, addr)| {
                            json!({
                                "index": i,
                                "tcp": addr,
                                "cid": args.cid + i as u32,
                                "guest_port": args.guest_port,
                            })
                        })
                        .collect::<Vec<_>>(),
                    "rootfs": rootfs_ref.to_string_lossy(),
                    "rootfs_source": rootfs_source,
                })
            );
            eprintln!("zeroboot up: {n} sandbox VM(s) bridged at {addresses:?} (Ctrl-C to stop)");
            tokio::signal::ctrl_c()
                .await
                .map_err(|error| external(format!("ctrl-c listener: {error}")))?;
            Ok(json!({
                "ok": true,
                "stopped": true,
                "sandboxes": addresses,
                "rootfs": rootfs_ref.to_string_lossy(),
            }))
        })
    })();

    // Always tear the VMs and their work directories down.
    for vm in &mut vms {
        let _ = vm.child.kill();
        let _ = vm.child.wait();
    }
    for vm in vms {
        let _ = fs::remove_dir_all(&vm.work_dir);
    }

    result
}
