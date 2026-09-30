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
use crate::protocol::{Error as ProtocolError, Execute, Frame, HelloAck, Kind, Output};
use serde_json::{json, Value};
use std::{
    fs,
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

/// Read one response frame and validate magic/version (via the shared
/// single-source decoder) plus the echoed request_id, proving no cross-talk
/// between connections. The wire-declared payload length is attacker
/// controlled, so it is capped at the protocol's `MAX_PAYLOAD` by the decoder
/// before allocation.
fn read_response(
    stream: &mut UnixStream,
    request_id: [u8; 16],
    name: &str,
) -> Result<(Kind, Vec<u8>), CliError> {
    let frame = crate::protocol::read_frame_sync(stream).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            validation(format!("{name}: invalid ZBRT frame: {error}"))
        } else {
            external(format!("{name}: vsock read: {error}"))
        }
    })?;
    if frame.request_id != request_id {
        return Err(validation(format!(
            "{name}: request_id mismatch: got {:?} expected {request_id:?}",
            frame.request_id
        )));
    }
    Ok((frame.kind, frame.payload))
}

/// Mandatory ZBRT handshake on a fresh connection (PROTOCOL.md §3.4): the
/// guest refuses every non-Hello frame with `Error("protocol handshake
/// required")`, so each raw client sends Hello and requires the matching
/// HelloAck before any business frame.
fn handshake(stream: &mut UnixStream, name: &str) -> Result<(), CliError> {
    let request_id = request_id_from_name(&format!("{name}:hello"));
    let hello = crate::protocol::hello_frame("rfb-cli", request_id)
        .map_err(|error| external(format!("{name}: encode Hello: {error}")))?;
    hello
        .encode(&mut *stream)
        .map_err(|error| external(format!("{name}: write Hello: {error}")))?;
    let (kind, payload) = read_response(stream, request_id, name)?;
    if kind != Kind::HelloAck {
        return Err(validation(format!(
            "{name}: expected HelloAck, got {kind:?}"
        )));
    }
    HelloAck::decode(&payload)
        .map_err(|error| validation(format!("{name}: invalid HelloAck: {error}")))?;
    Ok(())
}

/// Derive a request_id by padding the check name (or `name`-derived label)
/// into the 16-byte field — the convention every raw verify client shares.
fn request_id_from_name(name: &str) -> [u8; 16] {
    let mut request_id = [0u8; 16];
    let bytes = name.as_bytes();
    request_id[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
    request_id
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
    let request_id = request_id_from_name(name);

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
        let (kind, payload) = read_response(&mut stream, request_id, name)?;
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

/// One boilerplate verify exchange: run the exchange, require a `Result`
/// frame (`{name}: expected Result kind`), and decode the legacy result.
/// Callers keep their own exit/stdout/stderr assertions and push their
/// `{name, ok}` entry into `checks` (extra fields like `round_trip_ms` and
/// custom transports like the concurrent threads stay handwritten).
fn run_check(
    uds: &Path,
    port: u16,
    name: &str,
    code: &[u8],
) -> Result<(i32, Vec<u8>, Vec<u8>), CliError> {
    let (kind, payload) = exchange(uds, port, name, code, 3000)?;
    if kind != Kind::Result {
        return Err(validation(format!("{name}: expected Result kind")));
    }
    parse_result(&payload, name)
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
    if !crate::cli::image_build::readable_file(kernel)
        || !crate::cli::image_build::readable_file(rootfs)
    {
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
    // deliberately read-only and rejects legacy RFB1/forkd artifacts. All
    // inspection goes through the shared run_debugfs wrapper (image_build).
    let debugfs = |request: &str| crate::cli::image_build::run_debugfs(rootfs, request, true);
    let init_stat = debugfs("stat /init")
        .map_err(|error| validation(format!("ZeroBoot rootfs /init: {}", error.message)))?;
    if !crate::cli::image_build::stat_is_executable_regular(&String::from_utf8_lossy(&init_stat)) {
        return Err(validation("ZeroBoot rootfs must contain executable regular /init; use image build-rootfs --mode zeroboot-zbrt"));
    }
    // Read the four contract markers via the shared debugfs reader; each
    // call site keeps its own missing-marker wording.
    let missing = |marker: &str| {
        validation(format!(
            "ZeroBoot rootfs is missing {marker}; use image build-rootfs --mode zeroboot-zbrt"
        ))
    };
    let [protocol, version, capabilities, guest_port] =
        crate::cli::image_build::read_zbrt_markers(rootfs);
    let markers = [
        protocol.map_err(|_| missing("/etc/zeroboot-protocol"))?,
        version.map_err(|_| missing("/etc/zeroboot-protocol-version"))?,
        capabilities.map_err(|_| missing("/etc/zeroboot-capabilities"))?,
        guest_port.map_err(|_| missing("/etc/zeroboot-guest-port"))?,
    ];
    if markers[0] != "zbrt"
        || markers[1] != "1"
        || markers[2] != crate::protocol::ZBRT_V1_CAPABILITIES.join(",")
        || markers[3] != "5000"
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
        let (exit, stdout, stderr) = run_check(&uds, DEFAULT_PORT, "echo", b"echo hello")?;
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
            let (exit, _, _) = run_check(&uds, DEFAULT_PORT, name, code)?;
            if exit != expected {
                return Err(validation(format!(
                    "{name}: exit={exit} expected {expected}"
                )));
            }
            checks.push(json!({"name": name, "ok": true}));
        }

        // unsupported command -> non-zero exit on the guest runtime
        let (exit, _, _) = run_check(&uds, DEFAULT_PORT, "unsupported", b"not-a-command")?;
        if exit != -1 {
            return Err(validation(format!("unsupported: exit={exit} expected -1")));
        }
        checks.push(json!({"name": "unsupported", "ok": true}));

        // bounded deadline: echo must round-trip within 5s wall clock
        let started = Instant::now();
        run_check(&uds, DEFAULT_PORT, "deadline-echo", b"echo bounded")?;
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
        let rid = request_id_from_name("malformed");
        let malformed = Frame {
            kind: Kind::Execute,
            flags: 0,
            request_id: rid,
            payload: b"X".to_vec(),
        };
        malformed
            .encode(&mut stream)
            .map_err(|error| external(format!("write malformed frame: {error}")))?;
        let (kind, payload) = read_response(&mut stream, rid, "malformed")?;
        if kind != Kind::Error {
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
            let ms = |q: f64| {
                crate::cli::report::quantile_ns(&samples, q)
                    .map(|ns| (ns / 1e6 * 1000.0).round() / 1000.0)
            };
            bench_stats = json!({
                "samples": samples.len(),
                "attempts": attempts,
                "timeouts": timeouts,
                "success_rate_pct": 100.0 * samples.len() as f64 / attempts as f64,
                "p50_ms": ms(0.50),
                "p95_ms": ms(0.95),
                "p99_ms": ms(0.99),
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
    // Argument-level validation first (KVM-free, mirrors forkd backend-up:
    // bad arguments must fail with their own message even on hosts without
    // KVM, before any environment gate fires).
    if args.n == 0 {
        return Err(validation("--n must be positive"));
    }
    let (rootfs, rootfs_source) = crate::cli::image_build::resolve_rootfs_source(
        args.rootfs.as_deref(),
        args.pid1_dir.as_deref(),
        |dir| {
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
            Ok(built)
        },
    )?;
    crate::cli::image_build::require_readable(&rootfs, "rootfs")?;
    crate::cli::image_build::require_readable(&args.kernel, "kernel")?;
    let caps = detect();
    let linux = matches!(caps.kind, HostKind::Linux | HostKind::Wsl);
    if !linux || !caps.kvm {
        return Err(crate::cli::error::no_vm(
            "Linux/KVM are required for zeroboot up",
        ));
    }

    /// One sandbox: its VM child, the relay UDS, and the cleaned-up work dir.
    struct Vm {
        child: Child,
        uds: std::path::PathBuf,
        work_dir: std::path::PathBuf,
    }

    let tcp = args.tcp.clone();
    // Strict parse: a malformed --tcp must be a usage error, not a silent
    // fallback to 5000 (a wrong bind target fails confusingly later); the
    // bind must be loopback — the relay exposes an UNAUTHENTICATED ZBRT
    // guest, binding it non-loopback would publish it to the network.
    let (host_ip, base_port) = match tcp.rsplit_once(':') {
        Some((ip, port)) => {
            let port: u16 = port
                .parse()
                .map_err(|_| validation(format!("--tcp port is not a valid port number: {tcp}")))?;
            (ip.to_owned(), port)
        }
        None => return Err(validation(format!("--tcp must be host:port, got {tcp}"))),
    };
    if host_ip != "127.0.0.1" && host_ip != "::1" && host_ip != "localhost" {
        return Err(validation(
            "--tcp must bind loopback (the ZBRT relay has no authentication)",
        ));
    }
    // Multi-sandbox increments must not wrap around (release builds would
    // silently bind low ports).
    let last_port =
        u16::try_from(base_port as u32 + (args.n.saturating_sub(1) as u32)).map_err(|_| {
            validation(format!(
                "--tcp port range exhausted: {} + {} sandboxes",
                base_port, args.n
            ))
        })?;
    let last_cid = args
        .cid
        .checked_add(args.n.saturating_sub(1) as u32)
        .ok_or_else(|| {
            validation(format!(
                "--cid range exhausted: {} + {} sandboxes",
                args.cid, args.n
            ))
        })?;
    let _ = (last_port, last_cid);

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
                        match listener.accept().await {
                            Ok((mut tcp_stream, _)) => {
                                let uds = uds.clone();
                                tokio::spawn(async move {
                                    let handshake = Duration::from_secs(10);
                                    let Ok(mut relay) = crate::vsock::connect_firecracker_uds(
                                        &uds, guest_port, handshake,
                                    )
                                    .await
                                    else {
                                        return;
                                    };
                                    let _ =
                                        tokio::io::copy_bidirectional(&mut tcp_stream, &mut relay)
                                            .await;
                                });
                            }
                            // A PERSISTENT accept error (fd exhaustion…) must
                            // not busy-spin the current-thread runtime: back
                            // off and retry.
                            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                        }
                    }
                }));
            }
            // Announce readiness only after every listener exists — on
            // STDERR: this is a status announcement, not command output, and
            // `--json` promises exactly ONE JSON document on stdout (the
            // final render comes from dispatch after shutdown).
            eprintln!(
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
            // Ctrl-C AND SIGTERM both tear down: SIGTERM is what systemd /
            // nohup-style supervisors send, and the doc promises it works.
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .map_err(|error| external(format!("signal listener: {error}")))?;
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            ctrl_c
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
