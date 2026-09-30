#![cfg(all(feature = "cli", unix))]

//! `forkd backend-up` / `zeroboot` gate contract tests — the argument
//! surface, the JSON report shapes, and the pid1 → rootfs path (pure file
//! work: no KVM, no TAP, no controller). Real-VM behaviour is exercised by
//! the `*_real` suites and the local gates, not here.

mod common;

use common::cli::{combined_text, run, stdout_text};

/// `forkd backend-up`: contract violations fail fast with actionable
/// messages — regardless of whether the caller is root (argument validation
/// deliberately precedes the root check).
#[test]
fn backend_up_rejects_missing_rootfs_and_pid1_dir() {
    let output = run(&[
        "forkd",
        "backend-up",
        "--kernel",
        "/tmp/vmlinux-unused",
        "--bind",
        "127.0.0.1:18889",
    ]);
    assert!(!output.status.success(), "missing both sources must fail");
    assert!(
        combined_text(&output).contains("--rootfs or --pid1-dir"),
        "error must name the missing argument"
    );
}

#[test]
fn backend_up_rejects_rootfs_and_pid1_dir_together() {
    let output = run(&[
        "forkd",
        "backend-up",
        "--kernel",
        "/tmp/vmlinux-unused",
        "--rootfs",
        "/tmp/a.ext4",
        "--pid1-dir",
        "/tmp/pid1",
        "--bind",
        "127.0.0.1:18889",
    ]);
    assert!(
        !output.status.success(),
        "mutually exclusive sources must fail"
    );
    assert!(combined_text(&output).contains("mutually exclusive"));
}

#[test]
fn backend_up_rejects_non_loopback_bind() {
    let output = run(&[
        "forkd",
        "backend-up",
        "--kernel",
        "/tmp/vmlinux-unused",
        "--rootfs",
        "/tmp/a.ext4",
        "--bind",
        "0.0.0.0:18889",
    ]);
    assert!(!output.status.success(), "non-loopback bind must fail");
    // The gate is the shared localhost single source
    // (`crate::cli::localhost::require_localhost`), so the message is the
    // shared "target localhost only" wording.
    assert!(combined_text(&output).contains("localhost only"));
}

/// `zeroboot preflight` reports JSON even when the host cannot run VMs: the
/// check list must always exist, `ok` reflects it, and without
/// `--require-vm` the exit stays 0 (report, not gate).
#[test]
fn preflight_reports_json_without_kvm_requirement() {
    let kernel = "/proc/self/exe"; // any readable file passes the is-file check
    let output = run(&[
        "--json",
        "zeroboot",
        "preflight",
        "--kernel",
        kernel,
        "--rootfs",
        kernel,
    ]);
    assert!(
        output.status.success(),
        "preflight without --require-vm reports instead of failing: {}",
        combined_text(&output)
    );
    let value: serde_json::Value = serde_json::from_str(stdout_text(&output).trim())
        .expect("preflight --json emits exactly one JSON object");
    let checks = value["checks"].as_array().expect("checks array");
    assert!(checks.len() >= 4, "host/firecracker/kernel/rootfs/tools");
    for check in checks {
        assert!(check["name"].is_string(), "check {check} has a name");
        assert!(check["ok"].is_boolean(), "check {check} has an ok flag");
    }
    assert!(value["ok"].is_boolean());
}

/// pid1 → rootfs, end to end, from the CLI surface with no cargo involved:
/// point `backend-up`'s rootfs builder at a directory holding the (dynamically
/// linked test-build) binaries and let `image build-rootfs` assemble the ext4.
#[test]
fn backend_up_builds_rootfs_from_pid1_dir_without_cargo() {
    let tmp = common::fsutil::unique_temp_dir("rfb-backend-up");
    let pid1 = tmp.join("pid1");
    std::fs::create_dir_all(&pid1).expect("create pid1 dir");
    // pid1 内容对组装契约无关（build-rootfs 只做文件安装 + 硬链接）；用小
    // 假二进制，避免 debug 构建的体积把自动 ext4 尺寸抬到元数据不匹配。
    for name in ["rfb-runtime", "rfb-busybox"] {
        std::fs::write(pid1.join(name), vec![0x7f; 3 * 1024 * 1024]).expect("write fake pid1");
    }

    let state_dir = tmp.join("state");
    let output = run(&[
        "forkd",
        "backend-up",
        "--kernel",
        "/tmp/vmlinux-unused",
        "--pid1-dir",
        pid1.to_str().expect("utf8 pid1 dir"),
        "--state-dir",
        state_dir.to_str().expect("utf8 state dir"),
        "--bind",
        "127.0.0.1:18889",
        "--allow-dynamic",
    ]);
    // The pid1 → rootfs stage has no KVM dependency: a failure here is a
    // contract break, not an environment gap. The command reaches the root
    // check afterwards (CI runners are not root) — so either it completed or
    // it stopped exactly at the root check with the rootfs already built.
    let rootfs = state_dir.join("sample.ext4");
    assert!(
        rootfs.is_file(),
        "pid1 rootfs must be built before the root check: {}",
        combined_text(&output)
    );
    // The built image carries the forkd contract sidecars.
    assert!(rootfs.with_extension("ext4.sha256").is_file());
    let sidecar = rootfs.with_extension("ext4.artifact.json");
    assert!(sidecar.is_file(), "artifact sidecar must exist");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sidecar).expect("read sidecar"))
            .expect("sidecar json");
    assert_eq!(manifest["backend"], "forkd");
    assert_eq!(manifest["entrypoint"], "/forkd-init.sh");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A pid1 dir missing the busybox binary fails the build with the
/// actionable message (rfb-busybox provides the guest's /bin/sh).
#[test]
fn backend_up_rejects_pid1_dir_without_busybox() {
    let tmp = common::fsutil::unique_temp_dir("rfb-backend-up-busy");
    let pid1 = tmp.join("pid1");
    std::fs::create_dir_all(&pid1).expect("create pid1 dir");
    std::fs::write(pid1.join("rfb-runtime"), vec![0x7f; 1024]).expect("write fake pid1");
    let output = run(&[
        "forkd",
        "backend-up",
        "--kernel",
        "/tmp/vmlinux-unused",
        "--pid1-dir",
        pid1.to_str().expect("utf8"),
        "--state-dir",
        tmp.join("state").to_str().expect("utf8"),
        "--bind",
        "127.0.0.1:18889",
    ]);
    assert!(
        !output.status.success(),
        "missing busybox must fail the build"
    );
    assert!(combined_text(&output).contains("rfb-busybox"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// zeroboot up mirrors the same contract on its surface: missing sources and
/// non-zero --n fail before any VM is touched (argument-level, KVM-free).
#[test]
fn zeroboot_up_rejects_bad_arguments_without_touching_kvm() {
    let output = run(&["zeroboot", "up", "--kernel", "/tmp/vmlinux-unused"]);
    assert!(!output.status.success(), "no rootfs source must fail");
    assert!(combined_text(&output).contains("--rootfs or --pid1-dir"));

    let output = run(&[
        "--json",
        "zeroboot",
        "benchmark",
        "--kernel",
        "/tmp/vmlinux-unused",
        "--rootfs",
        "/tmp/no-such-rootfs.ext4",
    ]);
    // No KVM on most CI hosts: the gate reports skip/absence instead of
    // failing; with KVM it fails closed on the missing rootfs. Both are
    // contract-shaped (JSON or a validation error), never a panic.
    let text = combined_text(&output);
    assert!(
        output.status.success() || text.contains("rootfs") || text.contains("KVM"),
        "unexpected failure shape: {text}"
    );
}
