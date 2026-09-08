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
        let sandbox = rfb::cli::forkd::create_sandbox(url, &tag, 1, Some(32))
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
    let kernel = common::resx("kernel/vmlinux-5.10.225");
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
    format!("{:x}", Sha256::digest(&bytes))
}
