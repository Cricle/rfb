#![cfg(all(unix, feature = "cli"))]

// Real-VM forkd E2E: drives the actual rfb-cli against a live forkd controller
// and a bootable snapshot. Triple-gated so a default `cargo test` never boots a
// VM: unix+cli cfg, `#[ignore]`, and `RFB_REAL_E2E=1` (checked in-test, never
// silently skipped). Expected check-name sets are pinned from
// `src/cli/forkd/*`, so contract drift fails here first. Driven by
// `tests/run-real.sh` on WSL or native Linux.

mod common;

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PREFLIGHT_CHECKS: &[&str] = &[
    "platform",
    "architecture",
    "kvm",
    "tool:cargo",
    "tool:musl-gcc",
    "tool:mke2fs",
    "tool:readelf",
    "tool:objdump",
    "tool:firecracker",
    "tool:curl",
    "forkd_binary",
    "forkd_rootfs",
    "forkd_tap",
    "forkd_controller",
    "forkd_snapshot",
];
const ACCEPTANCE_CHECKS: &[&str] = &[
    "controller_ping",
    "guest_ping",
    "guest_stream",
    "guest_exec",
    "guest_ls",
    "guest_write",
    "guest_read",
    "guest_find",
    "guest_grep",
];
const ACCEPTANCE_NEGATIVES: &[&str] = &["ls_escape", "ls_zero_limit", "grep_too_many_bytes"];
const BENCH_STAGES: &[&str] = &[
    "create",
    "ready",
    "controller_ping",
    "health",
    "stream",
    "exec",
    "cleanup",
];

fn name_set(entries: &[Value], field: &str, label: &str) -> BTreeSet<String> {
    entries
        .iter()
        .map(|entry| {
            entry[field]
                .as_str()
                .unwrap_or_else(|| panic!("{label}: missing {field} in {entry}"))
                .to_owned()
        })
        .collect()
}

fn assert_names(actual: BTreeSet<String>, expected: &[&str], label: &str) {
    let expected: BTreeSet<String> = expected.iter().map(|name| name.to_string()).collect();
    let missing: Vec<_> = expected.difference(&actual).collect();
    let unexpected: Vec<_> = actual.difference(&expected).collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "{label}: check-name drift, missing={missing:?} unexpected={unexpected:?}"
    );
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn preflight_ready_with_full_check_set() {
    common::require_real();
    let tag = common::snapshot_tag();
    let out = common::run(
        common::cli().args([
            "forkd",
            "preflight",
            "--require-vm",
            "--tag",
            &tag,
            "--json",
        ]),
        Duration::from_secs(300),
        "preflight",
    );
    assert_eq!(
        out.code, 0,
        "preflight failed: {} / {}",
        out.stdout, out.stderr
    );
    let value = common::parse_json(&out, "preflight");
    assert_eq!(value["status"], "ready", "preflight not ready");
    assert_eq!(value["snapshot_tag"], tag);
    let checks = value["checks"].as_array().expect("checks array");
    assert_names(
        name_set(checks, "name", "preflight"),
        PREFLIGHT_CHECKS,
        "preflight",
    );
    for check in checks {
        assert_eq!(
            check["status"], "pass",
            "preflight check {} blocked: {}",
            check["name"], check["reason"]
        );
    }
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn acceptance_gate_all_ops_and_negatives() {
    common::require_real();
    let tag = common::snapshot_tag();
    let out = common::run(
        common::cli().args([
            "forkd",
            "acceptance",
            "--require-vm",
            "--tag",
            &tag,
            "--json",
        ]),
        Duration::from_secs(300),
        "acceptance",
    );
    assert_eq!(
        out.code, 0,
        "acceptance failed: {} / {}",
        out.stdout, out.stderr
    );
    let value = common::parse_json(&out, "acceptance");
    assert_eq!(value["status"], "passed", "acceptance not passed");
    assert!(
        !value["sandbox_id"].as_str().unwrap_or_default().is_empty(),
        "sandbox_id missing"
    );
    let checks = value["checks"].as_array().expect("checks array");
    assert_names(
        name_set(checks, "name", "acceptance"),
        ACCEPTANCE_CHECKS,
        "acceptance",
    );
    for check in checks {
        assert_eq!(
            check["ok"], true,
            "acceptance check {} failed",
            check["name"]
        );
    }
    let negatives = value["negative_cases"].as_array().expect("negative_cases");
    assert_names(
        name_set(negatives, "name", "negatives"),
        ACCEPTANCE_NEGATIVES,
        "negatives",
    );
    for case in negatives {
        assert_eq!(
            case["rejected"], true,
            "negative case {} was not rejected",
            case["name"]
        );
    }
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn workload_passes_without_orphans() {
    common::require_real();
    let tag = common::snapshot_tag();
    let out = common::run(
        common::cli().args(["forkd", "workload", "--tag", &tag, "--json"]),
        Duration::from_secs(600),
        "workload",
    );
    assert_eq!(
        out.code, 0,
        "workload failed: {} / {}",
        out.stdout, out.stderr
    );
    let value = common::parse_json(&out, "workload");
    assert_eq!(value["result"], "PASS", "workload not PASS");
    assert!(
        value["orphans"].as_array().expect("orphans").is_empty(),
        "leaked sandboxes: {:?}",
        value["orphans"]
    );
    let created = value["sandboxes_created"].as_u64().expect("created");
    let destroyed = value["sandboxes_destroyed"].as_u64().expect("destroyed");
    assert!(created > 0, "no sandboxes created");
    assert_eq!(created, destroyed, "sandbox leak: {created} vs {destroyed}");
    assert!(value["total_ops"].as_u64().expect("total_ops") > 0);
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn benchmark_100_iterations_all_stages() {
    common::require_real();
    let tag = common::snapshot_tag();
    let out = common::run(
        common::cli().args(["forkd", "benchmark", "--tag", &tag, "--n", "100", "--json"]),
        Duration::from_secs(900),
        "benchmark",
    );
    assert_eq!(
        out.code, 0,
        "benchmark failed: {} / {}",
        out.stdout, out.stderr
    );
    let value = common::parse_json(&out, "benchmark");
    assert_eq!(value["result"], "PASS", "benchmark not PASS");
    assert_eq!(value["iterations"], 100);
    assert_eq!(value["failures"], 0);
    let stats = value["stats"].as_object().expect("stats object");
    assert_names(
        stats.keys().cloned().collect(),
        BENCH_STAGES,
        "benchmark stages",
    );
    for (stage, entry) in stats {
        assert_eq!(entry["samples"], 100, "stage {stage} under-sampled");
        assert_eq!(entry["success_rate_pct"], 100.0, "stage {stage} lossy");
        let p50 = entry["p50_ns"].as_f64().expect("p50_ns");
        let p95 = entry["p95_ns"].as_f64().expect("p95_ns");
        let p99 = entry["p99_ns"].as_f64().expect("p99_ns");
        assert!(p50 > 0.0, "stage {stage} p50 not positive");
        assert!(p95 >= p50 && p99 >= p95, "stage {stage} quantiles inverted");
    }
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn gate_exit_code_contract_on_dead_controller() {
    common::require_real();
    let out = common::run(
        common::cli().env("FORKD_URL", "http://127.0.0.1:9").args([
            "forkd",
            "preflight",
            "--require-vm",
            "--json",
        ]),
        Duration::from_secs(120),
        "dead-controller gate",
    );
    assert_eq!(
        out.code,
        common::EXIT_NOVM,
        "expected exit 12, got {}: {} / {}",
        out.code,
        out.stdout,
        out.stderr
    );
    let value = common::parse_json(&out, "dead-controller gate");
    assert_eq!(value["error"]["code"], common::EXIT_NOVM);
    assert!(
        !value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "error message missing"
    );
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn sandbox_has_no_internet_egress() {
    common::require_real();
    let tag = common::snapshot_tag();
    common::block_on(async {
        let url = "http://127.0.0.1:8889";
        let sandbox = rfb::cli::forkd::create_sandbox(url, &tag, 1, Some(32), false)
            .await
            .expect("sandbox create")
            .into_iter()
            .next()
            .expect("sandbox");
        let sid = sandbox.id.clone();
        let result = async {
            rfb::cli::forkd::wait_for_guest_ready(&sandbox.guest_addr, Duration::from_secs(30))
                .await?;
            // The forkd agent implements `netprobe` in-process (no /bin
            // tooling needed): exit 0 only when internet egress works.
            rfb::cli::forkd::guest_call(
                &sandbox.guest_addr,
                serde_json::json!({
                    "action": "exec",
                    "cwd": "/workspace",
                    "args": ["netprobe"]
                }),
                true,
            )
            .await
        }
        .await;
        let _ = rfb::cli::forkd::destroy_sandbox(url, &sid).await;
        let response = result.expect("netprobe exec failed");
        assert_eq!(
            response["exit_code"], 1,
            "SANDBOX NO-EGRESS CONTRACT VIOLATED — guest reached the internet: {response}"
        );
    });
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn provenance_binding_roundtrip_verified() {
    common::require_real();
    let tag = format!("rfb-e2e-prov-{}", std::process::id());
    let _guard = SnapshotGuard { tag: tag.clone() };
    let out = common::run(
        common::cli().args([
            "forkd",
            "snapshot-create",
            "--tag",
            &tag,
            "--tap",
            "forkd-tap0",
            "--json",
        ]),
        Duration::from_secs(420),
        "snapshot-create (provenance)",
    );
    assert_eq!(
        out.code, 0,
        "snapshot-create failed: {} / {}",
        out.stdout, out.stderr
    );

    // Build the artifact manifest from measured digests: kernel and
    // firecracker from the assets the snapshot actually booted, rootfs from
    // the built image, snapshot artifacts from the on-disk snapshot dir.
    let rootfs = common::rootfs("forkd-agent.ext4");
    let kernel = common::resx("kernel/vmlinux-arcbox-0.0.24");
    let home = std::env::var("HOME").unwrap_or_default();
    let snap_dir = PathBuf::from(home)
        .join(".local/share/forkd/snapshots")
        .join(&tag);
    let manifest = serde_json::json!({
        "schema": "rfb-artifact/v1",
        "backend": "forkd",
        "profile": "forkd-agent",
        "arch": "x86_64",
        "transport": "tcp",
        "protocol": "forkd",
        "entrypoint": "/forkd-init.sh",
        "guest_port": 8888,
        "kernel": {"path": kernel, "sha256": sha256_file(&kernel)},
        "rootfs": {"path": rootfs, "sha256": sha256_file(&rootfs)},
        "firecracker": {"path": "/usr/local/bin/firecracker", "version": "1.12.1",
                         "sha256": sha256_file(Path::new("/usr/local/bin/firecracker"))},
        "vm": {"cpus": 1, "memory_bytes": 536870912u64},
        "snapshot": {
            "tag": tag,
            "sha256": sha256_file(&snap_dir.join("snapshot.json")),
            "memory_sha256": sha256_file(&snap_dir.join("memory.bin")),
            "vmstate_sha256": sha256_file(&snap_dir.join("vmstate")),
            "network": true,
            "batch_id": "e2e-provenance",
            "version": "0.1.0",
            "vm_identity": "e2e-1vcpu-512m",
            "network_identity": "forkd-tap0/10.42.0.0-24",
        }
    });
    let manifest_path =
        std::env::temp_dir().join(format!("rfb-e2e-manifest-{}.json", std::process::id()));
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).expect("manifest json"),
    )
    .expect("write manifest");
    let binding_path =
        std::env::temp_dir().join(format!("rfb-e2e-binding-{}.json", std::process::id()));

    // Host-observed digests must reach `verified` under --require-provenance.
    let out = common::run(
        common::cli().args([
            "forkd",
            "snapshot-bind",
            "--tag",
            &tag,
            "--artifact",
            manifest_path.to_str().expect("manifest path"),
            "--output",
            binding_path.to_str().expect("binding path"),
            "--require-provenance",
            "--json",
        ]),
        Duration::from_secs(120),
        "snapshot-bind",
    );
    assert_eq!(
        out.code, 0,
        "snapshot-bind failed: {} / {}",
        out.stdout, out.stderr
    );
    let binding = common::parse_json(&out, "snapshot-bind");
    assert_eq!(
        binding["verification"], "verified",
        "binding not verified: {binding}"
    );
    assert_eq!(
        binding["controller"]["provenance"]["source"], "host-observed-disk",
        "provenance source must be labelled"
    );

    // Preflight accepts the verified binding paired with its manifest.
    let out = common::run(
        common::cli().args([
            "forkd",
            "preflight",
            "--require-vm",
            "--tag",
            &tag,
            "--artifact-manifest",
            manifest_path.to_str().expect("manifest path"),
            "--snapshot-binding",
            binding_path.to_str().expect("binding path"),
            "--json",
        ]),
        Duration::from_secs(120),
        "preflight (provenance)",
    );
    assert_eq!(
        out.code, 0,
        "preflight with verified binding failed: {} / {}",
        out.stdout, out.stderr
    );
}

/// Best-effort delete of the E2E snapshot on scope exit, even on panic.
struct SnapshotGuard {
    tag: String,
}

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        let _ = common::run(
            common::cli().args(["forkd", "snapshot-delete", "--tag", &self.tag, "--json"]),
            Duration::from_secs(120),
            "snapshot-delete (guard)",
        );
    }
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn snapshot_lifecycle_private_rootfs_and_delete() {
    common::require_real();
    let tag = format!(
        "rfb-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
    );
    let _guard = SnapshotGuard { tag: tag.clone() };
    let rootfs = common::rootfs("forkd-agent.ext4");
    let before = sha256_file(&rootfs);

    let out = common::run(
        common::cli().args([
            "forkd",
            "snapshot-create",
            "--tag",
            &tag,
            "--tap",
            "forkd-tap0",
            "--json",
        ]),
        Duration::from_secs(420),
        "snapshot-create",
    );
    assert_eq!(
        out.code, 0,
        "snapshot-create failed: {} / {}",
        out.stdout, out.stderr
    );
    let value = common::parse_json(&out, "snapshot-create");
    assert_eq!(value["ok"], true, "create not ok");
    assert_eq!(value["created"], true);
    assert_eq!(value["tag"], tag);
    assert_eq!(value["tap"], "forkd-tap0");
    let copy_path = PathBuf::from(value["rootfs_copy"].as_str().expect("rootfs_copy"));
    assert!(
        copy_path.is_file(),
        "private rootfs copy missing: {copy_path:?}"
    );
    assert_ne!(
        copy_path, rootfs,
        "private copy must differ from the artifact rootfs"
    );
    assert_eq!(
        sha256_file(&rootfs),
        before,
        "artifact rootfs was mutated by snapshot-create"
    );

    // Controller-side readiness: the snapshot must be ready AND bootable
    // (preflight's forkd_snapshot check reads the controller registry).
    let out = common::run(
        common::cli().args([
            "forkd",
            "preflight",
            "--require-vm",
            "--tag",
            &tag,
            "--json",
        ]),
        Duration::from_secs(120),
        "preflight after snapshot-create",
    );
    assert_eq!(
        out.code, 0,
        "new snapshot not ready/bootable: {} / {}",
        out.stdout, out.stderr
    );
    let ready = common::parse_json(&out, "preflight after snapshot-create");
    assert_eq!(ready["status"], "ready");

    // Delegated chain info must resolve for the fresh tag.
    let out = common::run(
        common::cli().args(["forkd", "snapshot-info", "--tag", &tag, "--json"]),
        Duration::from_secs(60),
        "snapshot-info",
    );
    assert_eq!(
        out.code, 0,
        "snapshot-info failed: {} / {}",
        out.stdout, out.stderr
    );
    let info = common::parse_json(&out, "snapshot-info");
    assert_eq!(info["tag"], tag);

    let out = common::run(
        common::cli().args(["forkd", "snapshot-delete", "--tag", &tag, "--json"]),
        Duration::from_secs(120),
        "snapshot-delete",
    );
    assert_eq!(
        out.code, 0,
        "snapshot-delete failed: {} / {}",
        out.stdout, out.stderr
    );
    let deleted = common::parse_json(&out, "snapshot-delete");
    assert_eq!(deleted["deleted"], true);

    // After deletion the tag must fail the VM gate (exit 12) again.
    let out = common::run(
        common::cli().args([
            "forkd",
            "preflight",
            "--require-vm",
            "--tag",
            &tag,
            "--json",
        ]),
        Duration::from_secs(120),
        "preflight after snapshot-delete",
    );
    assert_eq!(
        out.code,
        common::EXIT_NOVM,
        "snapshot still usable after delete: {} / {}",
        out.stdout,
        out.stderr
    );
}

fn sha256_file(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).expect("read rootfs for digest");
    // Independent lowercase-hex encoder; digest 0.11 outputs no longer
    // implement LowerHex.
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Guest-level success/failure matrix: per-op semantics against live sandboxes,
// reusing the CI snapshot. Each test owns and reaps its sandboxes.
// ---------------------------------------------------------------------------

const CONTROLLER_URL: &str = "http://127.0.0.1:8889";

/// Best-effort sandbox teardown on scope exit, even on panic. Lives at the
/// sync level so its Drop can safely `block_on` outside any tokio runtime.
struct SandboxGuard {
    id: String,
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        let _ = common::block_on(rfb::cli::forkd::destroy_sandbox(CONTROLLER_URL, &self.id));
    }
}

/// Create one sandbox and wait for its guest agent. The leak-guard is
/// installed BEFORE the readiness wait so a boot failure still destroys the
/// sandbox instead of leaving it holding the shared tap (a leaked sandbox
/// makes every later create fail with 503).
fn spawn_guarded_sandbox(tag: &str) -> (rfb::forkd::SandboxInfo, SandboxGuard) {
    let sandbox = common::block_on(async {
        rfb::cli::forkd::create_sandbox(CONTROLLER_URL, tag, 1, None, false)
            .await
            .expect("sandbox create")
            .into_iter()
            .next()
            .expect("sandbox")
    });
    let guard = SandboxGuard {
        id: sandbox.id.clone(),
    };
    common::block_on(async {
        rfb::cli::forkd::wait_for_guest_ready(&sandbox.guest_addr, Duration::from_secs(30))
            .await
            .expect("guest ready");
    });
    (sandbox, guard)
}

fn guest_exec(address: &str, args: &[&str], timeout_secs: u64) -> Value {
    let args: Vec<&str> = args.to_vec();
    common::block_on(rfb::cli::forkd::guest_call(
        address,
        serde_json::json!({
            "action": "exec",
            "cwd": "/workspace",
            "timeout": timeout_secs,
            "args": args,
        }),
        true,
    ))
    .expect("guest exec")
}

fn guest_eval(address: &str, code: &str) -> Value {
    common::block_on(rfb::cli::forkd::guest_call(
        address,
        serde_json::json!({
            "action": "eval",
            "code": code,
            "cwd": "/workspace",
            "timeout": 10,
        }),
        true,
    ))
    .expect("guest eval")
}

fn json_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_u64().expect("byte in array") as u8)
            .collect(),
        Value::String(text) => text.as_bytes().to_vec(),
        other => panic!("expected byte array or string, got {other}"),
    }
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn guest_exec_streams_exit_codes_and_rejects_missing_binaries() {
    common::require_real();
    let tag = common::snapshot_tag();
    let (sandbox, _guard) = spawn_guarded_sandbox(&tag);
    let address = sandbox.guest_addr.clone();

    // stdout/stderr separation through a real shell.
    let result = guest_exec(
        &address,
        &["/bin/sh", "-c", "echo out-line; echo err-line >&2"],
        10,
    );
    assert_eq!(result["exit_code"], 0, "sh streams: {result}");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("out-line"),
        "stdout must carry the stdout line: {result}"
    );
    assert!(
        result["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("err-line"),
        "stderr must carry the stderr line: {result}"
    );
    assert_eq!(result["timed_out"], false);

    // Exit-code propagation.
    let result = guest_exec(&address, &["/bin/sh", "-c", "exit 7"], 10);
    assert_eq!(result["exit_code"], 7, "exit 7: {result}");
    let result = guest_exec(&address, &["/bin/false"], 10);
    assert_eq!(result["exit_code"], 1, "/bin/false: {result}");

    // Missing binary is a surfaced error, not a hang or a fake success.
    let error = common::block_on(rfb::cli::forkd::guest_call(
        &address,
        serde_json::json!({
            "action": "exec", "cwd": "/workspace", "timeout": 10,
            "args": ["/no/such/binary-e2e"],
        }),
        true,
    ));
    assert!(error.is_err(), "missing binary must surface an error");

    // The agent stays healthy after every failure above.
    let result = guest_exec(&address, &["/bin/true"], 10);
    assert_eq!(
        result["exit_code"], 0,
        "agent must survive failures: {result}"
    );
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn guest_exec_timeout_kills_and_agent_recovers() {
    common::require_real();
    let tag = common::snapshot_tag();
    let (sandbox, _guard) = spawn_guarded_sandbox(&tag);
    let address = sandbox.guest_addr.clone();

    let started = std::time::Instant::now();
    let result = guest_exec(&address, &["/bin/sleep", "30"], 2);
    assert_eq!(result["timed_out"], true, "sleep must time out: {result}");
    assert!(
        result["exit_code"].is_null(),
        "timeout has null exit_code: {result}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "timeout must not wait for the child: {started:?}"
    );

    // The agent must stay responsive (timeout killed only the child).
    let result = guest_exec(&address, &["/bin/true"], 10);
    assert_eq!(
        result["exit_code"], 0,
        "agent unhealthy after timeout: {result}"
    );
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn guest_eval_runs_code_and_maps_failures() {
    common::require_real();
    let tag = common::snapshot_tag();
    let (sandbox, _guard) = spawn_guarded_sandbox(&tag);
    let address = sandbox.guest_addr.clone();

    let result = guest_eval(&address, "echo hello-eval");
    assert_eq!(result["status"], 0, "eval success status: {result}");
    let output = String::from_utf8_lossy(&json_bytes(&result["output"])).into_owned();
    assert!(output.contains("hello-eval"), "eval output: {output:?}");

    assert_eq!(guest_eval(&address, "exit 3")["status"], 3, "eval exit 3");
    assert_eq!(
        guest_eval(&address, "definitely-not-a-command-e2e")["status"],
        127,
        "eval unknown command maps to 127"
    );
    assert_eq!(
        guest_eval(&address, "echo 'unterminated")["status"],
        2,
        "eval syntax error maps to 2"
    );
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn guest_fs_binary_roundtrip_append_and_read_miss() {
    common::require_real();
    let tag = common::snapshot_tag();
    let (sandbox, _guard) = spawn_guarded_sandbox(&tag);
    let address = sandbox.guest_addr.clone();
    let path = "/workspace/.rfb-e2e-binary.bin";
    let payload: Vec<u8> = (0..=255u8).collect();

    let written = common::block_on(rfb::cli::forkd::guest_call(
        &address,
        serde_json::json!({"action": "write", "path": path, "data": payload, "append": false}),
        false,
    ))
    .expect("write payload");
    assert_eq!(written["bytes_written"], 256, "write: {written}");

    let read = common::block_on(rfb::cli::forkd::guest_call(
        &address,
        serde_json::json!({"action": "read", "path": path, "max_bytes": 4096}),
        false,
    ))
    .expect("read payload");
    assert_eq!(json_bytes(&read["data"]), payload, "binary roundtrip");
    assert_eq!(read["truncated"], false);
    assert_eq!(read["total_bytes"], 256);

    // Offset + bounded read.
    let sliced = common::block_on(rfb::cli::forkd::guest_call(
        &address,
        serde_json::json!({"action": "read", "path": path, "offset": 10, "max_bytes": 5}),
        false,
    ))
    .expect("read slice");
    assert_eq!(
        json_bytes(&sliced["data"]),
        payload[10..15].to_vec(),
        "offset slice"
    );
    assert_eq!(sliced["truncated"], true, "slice must report truncation");

    // Append grows the file.
    let appended = common::block_on(rfb::cli::forkd::guest_call(
        &address,
        serde_json::json!({"action": "write", "path": path, "data": [1, 2], "append": true}),
        false,
    ))
    .expect("append");
    assert_eq!(appended["bytes_written"], 2);
    let read = common::block_on(rfb::cli::forkd::guest_call(
        &address,
        serde_json::json!({"action": "read", "path": path, "max_bytes": 4096}),
        false,
    ))
    .expect("read after append");
    assert_eq!(
        read["total_bytes"], 258,
        "append must grow the file: {read}"
    );

    // Missing file is a surfaced error.
    let missing = common::block_on(rfb::cli::forkd::guest_call(
        &address,
        serde_json::json!({"action": "read", "path": "/workspace/.rfb-e2e-missing.bin"}),
        false,
    ));
    assert!(missing.is_err(), "reading a missing file must fail");
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn guest_stream_event_lifecycle_stop_and_pty_rejection() {
    common::require_real();
    let tag = common::snapshot_tag();
    let (sandbox, _guard) = spawn_guarded_sandbox(&tag);
    let address = sandbox.guest_addr.clone();

    // Happy path: started → output → exit(0).
    let (stdout, stderr, exit_code) = common::block_on(async {
        use rfb::forkd_guest::ForkdGuestClient;
        let mut stream = ForkdGuestClient::new(address.clone())
            .stream(
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo stream-out; echo stream-err >&2".into(),
                ],
                Some("/workspace"),
                None,
                None,
            )
            .await
            .expect("open stream");
        let mut stdout = String::new();
        let mut stderr = String::new();
        let exit_code = loop {
            match stream.next_event().await.expect("stream event") {
                Some(value) => {
                    if let Some(code) = value.get("exit_code") {
                        break code.as_i64();
                    }
                    if let Some(out) = value.get("out").and_then(Value::as_str) {
                        stdout.push_str(out);
                    }
                    if let Some(err) = value.get("err").and_then(Value::as_str) {
                        stderr.push_str(err);
                    }
                }
                None => panic!("stream closed before exit"),
            }
        };
        (stdout, stderr, exit_code)
    });
    assert_eq!(exit_code, Some(0), "stream exit code");
    assert!(stdout.contains("stream-out"), "stream stdout: {stdout:?}");
    assert!(stderr.contains("stream-err"), "stream stderr: {stderr:?}");

    // stop: kills a long-running child and yields a terminal frame; idempotent.
    common::block_on(async {
        use rfb::forkd_guest::ForkdGuestClient;
        let mut stream = ForkdGuestClient::new(address.clone())
            .stream(
                vec!["/bin/sleep".into(), "30".into()],
                Some("/workspace"),
                None,
                None,
            )
            .await
            .expect("open sleep stream");
        let started = std::time::Instant::now();
        stream.stop().await.expect("stop");
        stream.stop().await.expect("second stop is idempotent");
        let mut terminal = false;
        while let Some(value) = stream.next_event().await.expect("event after stop") {
            if value.get("exit_code").is_some() {
                terminal = true;
                break;
            }
        }
        assert!(terminal, "stop must produce a terminal frame");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "stop must not wait for the child"
        );
    });

    // pty is explicitly rejected (never silently degraded).
    let pty_error = common::block_on(async {
        use rfb::forkd_guest::ForkdGuestClient;
        let mut stream = ForkdGuestClient::new(address.clone())
            .stream(vec!["/bin/true".into()], None, Some(true), None)
            .await
            .expect("open pty stream");
        loop {
            match stream.next_event().await {
                Ok(Some(value)) => {
                    if value.get("exit_code").is_some() {
                        return None;
                    }
                }
                Ok(None) => return None,
                Err(error) => return Some(error.to_string()),
            }
        }
    });
    let pty_error = pty_error.expect("pty request must be rejected");
    assert!(pty_error.contains("pty"), "pty error message: {pty_error}");
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn sandbox_create_rejects_invalid_and_unknown_tags() {
    common::require_real();
    common::block_on(async {
        let invalid =
            rfb::cli::forkd::create_sandbox(CONTROLLER_URL, "bad/tag!", 1, None, false).await;
        assert!(invalid.is_err(), "path-like tag must be rejected");
        let unknown = rfb::cli::forkd::create_sandbox(
            CONTROLLER_URL,
            "rfb-e2e-no-such-tag-xyz",
            1,
            None,
            false,
        )
        .await;
        assert!(unknown.is_err(), "unknown snapshot tag must be rejected");
    });
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn sandbox_delete_is_idempotent_and_guest_dies() {
    common::require_real();
    let tag = common::snapshot_tag();
    let (sandbox, _guard) = spawn_guarded_sandbox(&tag);
    let address = sandbox.guest_addr.clone();
    let id = sandbox.id.clone();

    common::block_on(async {
        assert!(
            rfb::cli::forkd::ping_sandbox(CONTROLLER_URL, &id)
                .await
                .is_ok(),
            "controller ping before delete"
        );
        rfb::cli::forkd::destroy_sandbox(CONTROLLER_URL, &id)
            .await
            .expect("destroy");
        rfb::cli::forkd::destroy_sandbox(CONTROLLER_URL, &id)
            .await
            .expect("double delete is idempotent");

        // The guest is truly gone, not just unregistered.
        let ping =
            rfb::cli::forkd::guest_call(&address, serde_json::json!({"action":"ping"}), false)
                .await;
        assert!(ping.is_err(), "guest must be unreachable after delete");
    });
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn wait_for_guest_ready_rejects_bad_address_and_times_out() {
    common::require_real();
    common::block_on(async {
        let empty = rfb::cli::forkd::wait_for_guest_ready("", Duration::from_secs(2)).await;
        assert!(empty.is_err(), "empty guest address must be rejected");

        let started = std::time::Instant::now();
        let dead =
            rfb::cli::forkd::wait_for_guest_ready("127.0.0.1:1", Duration::from_secs(2)).await;
        let error = dead.expect_err("closed port must never become ready");
        assert!(
            error.message.contains("not ready"),
            "dead-port error must name the readiness failure: {error:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(1500),
            "must respect the deadline"
        );
    });
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn shared_tap_rejects_multi_spawn_and_sequential_sandboxes_are_isolated() {
    common::require_real();
    let tag = common::snapshot_tag();

    // Design contract for the shared host tap: n>1 is rejected with an
    // actionable error (per_child_netns=true is the documented alternative).
    let error = common::block_on(rfb::cli::forkd::create_sandbox(
        CONTROLLER_URL,
        &tag,
        2,
        None,
        false,
    ))
    .expect_err("n>1 on the shared tap must be rejected");
    assert!(
        error.message.contains("n>1") || error.message.contains("per_child_netns"),
        "multi-spawn error must be actionable: {error:?}"
    );

    // Sequential single spawns work, and /workspace state is per-sandbox: a
    // file written in the first sandbox is invisible in the second.
    let marker = "isolated-payload";
    let (first, first_guard) = spawn_guarded_sandbox(&tag);
    common::block_on(async {
        rfb::cli::forkd::guest_call(
            &first.guest_addr,
            serde_json::json!({"action":"write","path":"/workspace/iso.txt","data":marker}),
            false,
        )
        .await
        .expect("write in first sandbox");
        let read = rfb::cli::forkd::guest_call(
            &first.guest_addr,
            serde_json::json!({"action":"read","path":"/workspace/iso.txt"}),
            false,
        )
        .await
        .expect("read back in first sandbox");
        let data: Vec<u8> = serde_json::from_value(read["data"].clone()).expect("byte array data");
        assert_eq!(data, marker.as_bytes(), "first sandbox keeps its own file");
    });
    let first_id = first.id.clone();
    drop(first_guard);

    // Wait until the controller reaped the first sandbox (its teardown frees
    // the shared tap for the next spawn).
    let reaped = common::block_on(async {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = rfb::cli::forkd::list_sandboxes(CONTROLLER_URL)
                .await
                .expect("list");
            if !remaining.iter().any(|sandbox| sandbox.id == first_id) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    assert!(
        reaped,
        "first sandbox must be reaped before the second spawn"
    );

    let (second, _second_guard) = spawn_guarded_sandbox(&tag);
    common::block_on(async {
        let read = rfb::cli::forkd::guest_call(
            &second.guest_addr,
            serde_json::json!({"action":"read","path":"/workspace/iso.txt"}),
            false,
        )
        .await;
        assert!(
            read.is_err(),
            "second sandbox must not see the first's file"
        );
    });
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn per_child_netns_supports_concurrently_live_sandboxes() {
    common::require_real();
    let tag = common::snapshot_tag();

    // per_child_netns=true is the documented path for parallel sandboxes; the
    // create must return all N at once and every guest must be independently
    // reachable while the others are alive.
    let sandboxes = common::block_on(rfb::cli::forkd::create_sandbox(
        CONTROLLER_URL,
        &tag,
        3,
        Some(32),
        true,
    ))
    .expect("parallel create with per_child_netns");
    assert_eq!(sandboxes.len(), 3, "all three sandboxes must be created");
    struct Guards(Vec<String>);
    impl Drop for Guards {
        fn drop(&mut self) {
            for id in &self.0 {
                let _ = common::block_on(rfb::cli::forkd::destroy_sandbox(CONTROLLER_URL, id));
            }
        }
    }
    let _guards = Guards(sandboxes.iter().map(|s| s.id.clone()).collect());

    let addresses: BTreeSet<String> = sandboxes.iter().map(|s| s.guest_addr.clone()).collect();
    assert_eq!(
        addresses.len(),
        3,
        "each sandbox must get its own guest address: {addresses:?}"
    );

    common::block_on(async {
        for (index, sandbox) in sandboxes.iter().enumerate() {
            rfb::cli::forkd::wait_for_guest_ready(&sandbox.guest_addr, Duration::from_secs(30))
                .await
                .expect("guest ready");
            let path = format!("/workspace/parallel-{index}.txt");
            rfb::cli::forkd::guest_call(
                &sandbox.guest_addr,
                serde_json::json!({
                    "action": "write",
                    "path": path,
                    "data": format!("owner-{index}"),
                }),
                false,
            )
            .await
            .expect("write in parallel sandbox");
        }
        // No sandbox may observe another's file: per-child netns also gives
        // each guest its own rootfs workspace namespace.
        for (index, sandbox) in sandboxes.iter().enumerate() {
            for other in 0..sandboxes.len() {
                let read = rfb::cli::forkd::guest_call(
                    &sandbox.guest_addr,
                    serde_json::json!({
                        "action": "read",
                        "path": format!("/workspace/parallel-{other}.txt"),
                    }),
                    false,
                )
                .await;
                let visible = read.is_ok();
                assert_eq!(
                    visible,
                    other == index,
                    "sandbox {index} read parallel-{other}.txt visible={visible}"
                );
            }
        }
    });
}

#[test]
#[ignore = "requires a live forkd stack; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn cli_snapshot_info_rejects_invalid_tag_with_validation_exit() {
    common::require_real();
    let out = common::run(
        common::cli().args(["forkd", "snapshot-info", "--tag", "bad/tag!", "--json"]),
        Duration::from_secs(60),
        "snapshot-info invalid tag",
    );
    assert_eq!(
        out.code,
        common::EXIT_VALIDATION,
        "invalid tag must exit 3: {} / {}",
        out.stdout,
        out.stderr
    );
    let value = common::parse_json(&out, "snapshot-info invalid tag");
    assert_eq!(value["error"]["code"], common::EXIT_VALIDATION);
}
