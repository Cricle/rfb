//! ZeroBoot CLI gates that mirror the forkd ones (benchmark / workload /
//! preflight): same report shapes and sanitized payloads, driven through the
//! ZeroBoot provider instead of a forkd controller. No daemon, no TAP: each
//! sandbox is one Firecracker VM over vsock, owned by this process.

use crate::cli::error::{external, validation, CliError};
use crate::cli::report::{checked, count_op, record, record_sample, Outcome};
use crate::cli::tool::HostKind;
use crate::core::{ExecSpec, Sandbox as _, SandboxSpec};
use crate::guest;
use crate::zeroboot::{Config, ZeroBootProvider};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Build a provider from CLI paths and fail fast when the host cannot run VMs.
fn provider(
    args: &crate::cli::zeroboot::ZerobootBackendArgs,
) -> Result<ZeroBootProvider, CliError> {
    let caps = crate::cli::host::detect();
    let linux = matches!(caps.kind, HostKind::Linux | HostKind::Wsl);
    if !linux || !caps.kvm {
        return Err(crate::cli::error::no_vm(
            "Linux/KVM are required for zeroboot gates",
        ));
    }
    if !crate::cli::tool::tool_available(&args.firecracker) {
        return Err(validation(format!(
            "firecracker binary not found: {}",
            args.firecracker
        )));
    }
    Ok(ZeroBootProvider::new(Config {
        kernel: Some(args.kernel.clone()),
        rootfs: Some(args.rootfs.clone()),
        firecracker: Some(args.firecracker.clone().into()),
        ..Config::default()
    }))
}

fn spec(argv: &[&str]) -> ExecSpec {
    ExecSpec {
        command: argv[0].to_owned(),
        args: argv[1..].iter().map(|s| s.to_string()).collect(),
        cwd: Some("/workspace".into()),
        stdin: None,
        timeout: Some(Duration::from_secs(30)),
    }
}

/// Drain a zeroboot guest stream to clean close, requiring an `Exit` frame.
/// Errors from event reads keep the external error wording.
async fn stream_to_exit(stream: &mut (dyn guest::GuestStream + '_)) -> Result<(), CliError> {
    let mut exited = false;
    while let Some(event) = stream
        .next_event()
        .await
        .map_err(|e| external(e.to_string()))?
    {
        if matches!(event, guest::StreamEvent::Exit { .. }) {
            exited = true;
        }
    }
    if exited {
        Ok(())
    } else {
        Err(validation("stream closed"))
    }
}

/// ZeroBoot microbenchmark: N iterations of create/health/exec/stream/cleanup
/// with sanitized p50/p95/p99 quantiles — the provider-level mirror of the
/// forkd benchmark. Guest payloads are never logged.
///
/// # Errors
///
/// Returns `Err` when the gate fails.
pub async fn benchmark(
    args: &crate::cli::zeroboot::ZerobootBackendArgs,
    n: usize,
) -> Result<Value, CliError> {
    if n == 0 {
        return Err(validation("iterations must be positive"));
    }
    let provider = provider(args)?;
    let mut samples: std::collections::HashMap<String, Vec<u64>> = Default::default();
    let mut outcome = Outcome::default();

    for _ in 0..n {
        let started = Instant::now();
        let sandbox = match provider.create_zero_boot(SandboxSpec::default()).await {
            Ok(sandbox) => sandbox,
            Err(_) => {
                outcome.failure += 1;
                continue;
            }
        };
        record_sample(&mut samples, "create", started);

        let t = Instant::now();
        record(
            &mut samples,
            &mut outcome,
            "health",
            t,
            sandbox.ping().await.is_ok(),
        );

        // The first exec also covers the guest's ready path.
        let ready_started = Instant::now();
        let exec = sandbox.exec(spec(&["/bin/true"])).await;
        let exec_ok = exec.as_ref().is_ok_and(|r| r.status == Some(0));
        record(
            &mut samples,
            &mut outcome,
            "ready+exec",
            ready_started,
            exec_ok,
        );

        let t = Instant::now();
        let streamed = {
            let opened = sandbox
                .stream(guest::StreamSpec {
                    command: "/bin/echo".into(),
                    args: vec!["zb-bench".into()],
                    cwd: Some("/workspace".into()),
                    ..Default::default()
                })
                .await
                .map_err(|e| external(e.to_string()));
            match opened {
                Ok(mut stream) => stream_to_exit(stream.as_mut()).await,
                Err(error) => Err(error),
            }
        };
        record(&mut samples, &mut outcome, "stream", t, streamed.is_ok());

        // Dropping the sandbox is the cleanup (VM teardown) — time it.
        let t = Instant::now();
        drop(sandbox);
        tokio::time::sleep(Duration::from_millis(50)).await;
        record(&mut samples, &mut outcome, "cleanup", t, true);
    }

    let stats = crate::cli::report::samples_stats_json(&samples, None);
    Ok(json!({
        "result": if outcome.failure == 0 { "PASS" } else { "FAIL" },
        "iterations": n,
        "outcome": crate::cli::report::outcome_json(&outcome),
        "stats": stats,
        "payload": "suppressed",
    }))
}

/// ZeroBoot workload gate: multiple sandboxes, rounds of exec/health/fs RPC,
/// session reuse via the pool, and leak-free teardown — the provider-level
/// mirror of the forkd workload. Returns sanitized aggregate counters only.
///
/// # Errors
///
/// Returns `Err` when the gate fails.
pub async fn workload(
    args: &crate::cli::zeroboot::ZerobootBackendArgs,
    sandboxes: usize,
    rounds: usize,
) -> Result<Value, CliError> {
    if sandboxes == 0 || rounds == 0 {
        return Err(validation("sandboxes and rounds must be positive"));
    }
    let provider = provider(args)?;
    let mut op_count: std::collections::BTreeMap<String, usize> = Default::default();

    for si in 0..sandboxes {
        let sandbox = provider
            .create_zero_boot(SandboxSpec::default())
            .await
            .map_err(|error| external(format!("sandbox {si} create failed: {error}")))?;
        count_op(&mut op_count, "sandbox_create");

        for ri in 0..rounds {
            checked(
                &mut op_count,
                sandbox.ping().await.is_ok(),
                "ping",
                "ping failed",
            )?;
            let exec = sandbox.exec(spec(&["/bin/true"])).await;
            checked(
                &mut op_count,
                exec.is_ok_and(|r| r.status == Some(0)),
                "exec",
                "exec failed",
            )?;

            let fname = format!("orders-2026-08-30-{si}-{ri}.csv");
            let row = format!("txn-{si}-{ri},pending,value-{}", si * 100 + ri);
            let content = format!("id,status,val\n{row}\n");
            let written = sandbox
                .write(guest::WriteRequest::new(&fname, content.as_bytes()))
                .await
                .map_err(|e| external(e.to_string()))?;
            checked(
                &mut op_count,
                written.bytes_written == content.len() as u64,
                "write",
                "write failed",
            )?;

            let read = sandbox
                .read(guest::ReadRequest {
                    path: fname.clone(),
                    offset: None,
                    max_bytes: Some(4096),
                })
                .await
                .map_err(|e| external(e.to_string()))?;
            let text = String::from_utf8_lossy(&read.data).into_owned();
            checked(
                &mut op_count,
                text.contains("txn-") && text.ends_with('\n'),
                "read",
                "read mismatch",
            )?;

            let found = sandbox
                .find(guest::FindRequest::new(".", &fname))
                .await
                .map_err(|e| external(e.to_string()))?;
            checked(
                &mut op_count,
                found.matches.iter().any(|m| m.contains(&fname)),
                "find",
                "find failed",
            )?;

            let greps = sandbox
                .grep(guest::GrepRequest::new(".", "pending"))
                .await
                .map_err(|e| external(e.to_string()))?;
            checked(
                &mut op_count,
                !greps.matches.is_empty(),
                "grep",
                "grep failed",
            )?;

            let mut stream = sandbox
                .stream(guest::StreamSpec {
                    command: "/bin/echo".into(),
                    args: vec![format!("round-{ri}")],
                    cwd: Some("/workspace".into()),
                    ..Default::default()
                })
                .await
                .map_err(|e| external(e.to_string()))?;
            checked(
                &mut op_count,
                stream_to_exit(stream.as_mut()).await.is_ok(),
                "stream",
                "stream closed",
            )?;
        }

        // Session-reuse pressure: repeated execs ride the pool.
        for _ in 0..5 {
            let exec = sandbox.exec(spec(&["/bin/true"])).await;
            checked(
                &mut op_count,
                exec.is_ok_and(|r| r.status == Some(0)),
                "reuse_exec",
                "reuse exec failed",
            )?;
        }

        // Handoff: write a summary, read it back, verify byte-exact.
        let summary = json!({"sandbox": si, "rounds": rounds, "ops": op_count});
        let summary_text = summary.to_string();
        sandbox
            .write(guest::WriteRequest::new(
                "summary.json",
                summary_text.as_bytes(),
            ))
            .await
            .map_err(|e| external(e.to_string()))?;
        count_op(&mut op_count, "summary_write");
        let read = sandbox
            .read(guest::ReadRequest::new("summary.json"))
            .await
            .map_err(|e| external(e.to_string()))?;
        // Byte-exact handoff: the read-back must equal what we wrote.
        checked(
            &mut op_count,
            read.data == summary_text.as_bytes(),
            "handoff_verify",
            "handoff digest mismatch",
        )?;
        // Dropping the sandbox tears the VM down (leak-free by construction).
        drop(sandbox);
        count_op(&mut op_count, "sandbox_destroy");
    }

    Ok(json!({
        "result": "PASS",
        "sandboxes": sandboxes,
        "rounds_per_sandbox": rounds,
        "total_ops": op_count.values().sum::<usize>(),
        "ops": op_count,
        "payload": "suppressed",
    }))
}

/// ZeroBoot preflight: host/KVM, Firecracker version, kernel, and the rootfs
/// contract (protocol markers, entrypoint) — everything `zeroboot up` needs.
///
/// # Errors
///
/// Returns `Err` when a required check fails.
pub async fn preflight(
    args: &crate::cli::zeroboot::ZerobootBackendArgs,
    require_vm: bool,
) -> Result<Value, CliError> {
    let caps = crate::cli::host::detect();
    let mut checks = Vec::new();
    let mut check = |name: &str, ok: bool, note: &str| {
        checks.push(json!({"name": name, "ok": ok, "note": note}));
        ok
    };

    let linux = matches!(caps.kind, HostKind::Linux | HostKind::Wsl);
    let mut all_ok = true;
    all_ok &= check(
        "host",
        linux && caps.kvm,
        if linux && caps.kvm {
            "Linux/KVM available"
        } else {
            "Linux/KVM required for real VMs"
        },
    );
    all_ok &= check(
        "firecracker",
        crate::cli::tool::tool_available(&args.firecracker)
            && crate::cli::rfb1::firecracker_version_ok(&args.firecracker),
        "v1.12.x / v1.16.x on PATH",
    );
    all_ok &= check(
        "kernel",
        args.kernel.is_file(),
        &args.kernel.to_string_lossy(),
    );
    let rootfs_ok =
        args.rootfs.is_file() && crate::cli::rfb1::validate_rootfs_contract(&args.rootfs).is_ok();
    all_ok &= check("rootfs_contract", rootfs_ok, &args.rootfs.to_string_lossy());
    all_ok &= check(
        "tools",
        crate::cli::tool::tool_available("debugfs"),
        "debugfs (e2fsprogs) validates the rootfs",
    );

    if require_vm && !all_ok {
        return Err(crate::cli::error::no_vm(
            "zeroboot preflight failed; see the check list",
        ));
    }
    Ok(json!({
        "ok": all_ok,
        "checks": checks,
        "protocol": ["zbrt"],
        "capabilities": ["execute", "stream", "health", "cancel", "filesystem"],
    }))
}
