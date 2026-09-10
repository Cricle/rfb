//! forkd controller reachability, snapshot readiness, and the read-only
//! preflight gate. Converges the legacy `preflight.sh` logic: platform / tool /
//! controller / snapshot checks with a `--require-vm` (exit 12) expectation.

use crate::cli::error::{no_vm, validation, CliError};
use crate::cli::host::detect;
use crate::cli::tool::{tool_available, HostKind};
use crate::controller::{ForkdClient, SnapshotInfo};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Build a forkd controller client from `FORKD_URL`/`FORKD_TOKEN` with a
/// strict loopback check. `require_vm` gates the "controller must respond"
/// expectation that acceptance/benchmark commands need.
pub fn client_from_env(timeout: Duration) -> Result<ForkdClient, CliError> {
    let url = std::env::var("FORKD_URL").unwrap_or_else(|_| "http://127.0.0.1:8889".into());
    client_from_url_env(&url, timeout)
}

/// Build a client for an explicit URL while retaining the shared token injection.
pub fn client_from_url_env(url: &str, timeout: Duration) -> Result<ForkdClient, CliError> {
    let base = crate::cli::localhost::require_localhost(url)?;
    let token = std::env::var("FORKD_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty());
    ForkdClient::new(base, token, timeout).map_err(|error| validation(error.to_string()))
}

pub(super) fn snapshot_ready(snapshots: &[SnapshotInfo], tag: &str) -> bool {
    snapshots
        .iter()
        .any(|s| s.tag == tag && s.status.eq_ignore_ascii_case("ready") && s.bootable)
}

/// True when `debugfs` can stat `path` inside the given ext4 image.
pub(super) fn debugfs_has(image: &Path, path: &str) -> bool {
    // `debugfs stat` exits 0 even for missing files ("File not found" goes to
    // stderr), so success alone is not a reliable signal. A present file is
    // only reported with an `Inode:` line on stdout; check for that.
    Command::new("debugfs")
        .args(["-R", &format!("stat {path}")])
        .arg(image)
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("Inode:")
        })
        .unwrap_or(false)
}

fn tap_ready() -> bool {
    if !matches!(detect().kind, HostKind::Linux | HostKind::Wsl) {
        return true;
    }
    let output = Command::new("ip")
        .args(["-o", "link", "show", "forkd-tap0"])
        .output();
    output
        .map(|value| {
            let text = String::from_utf8_lossy(&value.stdout);
            value.status.success()
                && text
                    .split('<')
                    .nth(1)
                    .and_then(|flags| flags.split('>').next())
                    .is_some_and(|flags| flags.split(',').any(|flag| flag == "UP"))
        })
        .unwrap_or(false)
}

/// One preflight check with a stable name and a pass/blocked status.
#[derive(Debug, Clone)]
pub struct PreflightCheck {
    /// Stable check name, e.g. `platform`, `kvm`, `tool:cargo`, `forkd_snapshot`.
    pub name: String,
    /// Human-readable explanation of the check outcome.
    pub reason: String,
    /// Whether this check blocks a real-VM preflight gate.
    pub blocked: bool,
}

/// Run the read-only preflight gate. Collects platform/tool/forkd checks the
/// same way the legacy `preflight.sh` did, but never mutates any state.
pub async fn preflight_checks(url: &str, tag: &str) -> Result<Vec<PreflightCheck>, CliError> {
    let mut checks: Vec<PreflightCheck> = Vec::new();
    let mut add = |name: &str, blocked: bool, reason: &str| {
        checks.push(PreflightCheck {
            name: name.to_owned(),
            reason: reason.to_owned(),
            blocked,
        });
    };

    let caps = detect();
    let linux = matches!(caps.kind, HostKind::Linux | HostKind::Wsl);
    add(
        "platform",
        !linux,
        if linux {
            "Linux detected"
        } else {
            "Linux is required"
        },
    );
    add(
        "architecture",
        caps.arch != "x86_64",
        if caps.arch == "x86_64" {
            "x86_64 detected"
        } else {
            "x86_64 is required"
        },
    );
    let kvm_text = if caps.kvm {
        "/dev/kvm is read-write"
    } else {
        "/dev/kvm is unavailable or not read-write"
    };
    add("kvm", !caps.kvm, kvm_text);

    for tool_name in [
        "cargo",
        "musl-gcc",
        "mke2fs",
        "readelf",
        "objdump",
        "firecracker",
        "curl",
    ] {
        let linux_only = matches!(
            tool_name,
            "musl-gcc" | "mke2fs" | "readelf" | "objdump" | "firecracker"
        );
        if !linux && linux_only {
            add(
                &format!("tool:{tool_name}"),
                false,
                "not applicable on Windows",
            );
            continue;
        }
        let available = tool_available(tool_name);
        add(
            &format!("tool:{tool_name}"),
            !available,
            if available { "available" } else { "missing" },
        );
    }

    // forkd controller binary (explicit FORKD_BIN or on PATH).
    let forkd_bin = std::env::var("FORKD_BIN").ok().filter(|v| !v.is_empty());
    let bin_ok = match &forkd_bin {
        Some(bin) => std::path::Path::new(bin).is_file(),
        None => tool_available("forkd-controller"),
    };
    add(
        "forkd_binary",
        !bin_ok,
        if bin_ok {
            "forkd controller binary found"
        } else {
            "forkd controller binary not found (set FORKD_BIN)"
        },
    );

    // forkd rootfs (optional; validated with debugfs when provided).
    let rootfs = std::env::var("FORKD_ROOTFS").ok().filter(|v| !v.is_empty());
    let rootfs_reason = match &rootfs {
        None => "FORKD_ROOTFS is not set",
        Some(path) if !std::path::Path::new(path).is_file() => "FORKD_ROOTFS is not readable",
        Some(path) => {
            // Verify the agent entrypoint when debugfs is present. The rfb-runtime
            // vsock image is explicitly rejected so forkd TCP and RFB1/vsock
            // mirrors can never be mixed.
            if !tool_available("debugfs") {
                "debugfs is required to verify the rootfs entrypoint"
            } else if debugfs_has(Path::new(path), "/sbin/rfb-runtime") {
                "FORKD_ROOTFS is the rfb-runtime vsock image, not forkd-agent"
            } else if debugfs_has(Path::new(path), "/sbin/forkd-agent") {
                "/sbin/forkd-agent entrypoint found"
            } else {
                "FORKD_ROOTFS lacks a verifiable forkd-agent entrypoint"
            }
        }
    };
    let rootfs_blocked = rootfs.is_none()
        || !std::path::Path::new(rootfs.as_deref().unwrap_or("")).is_file()
        || rootfs_reason.starts_with("debugfs")
        || rootfs_reason.starts_with("FORKD_ROOTFS");
    add("forkd_rootfs", rootfs_blocked, rootfs_reason);
    let tap_ok = tap_ready();
    add(
        "forkd_tap",
        !tap_ok,
        if tap_ok {
            "forkd-tap0 exists and is UP"
        } else {
            "forkd-tap0 is missing or DOWN; create/configure it before VM tests"
        },
    );

    // Controller reachability + snapshot readiness.
    let client = ForkdClient::new(url.to_owned(), None, Duration::from_secs(5))
        .map_err(|error| validation(error.to_string()))?;
    match client.list_snapshots().await {
        Ok(snapshots) => {
            add("forkd_controller", false, "controller is reachable");
            let ready = snapshot_ready(&snapshots, tag);
            add(
                "forkd_snapshot",
                !ready,
                if ready {
                    "requested snapshot is ready and bootable"
                } else {
                    "requested snapshot is missing or not ready/bootable"
                },
            );
        }
        Err(_) => {
            add("forkd_controller", true, "controller is unreachable");
            add(
                "forkd_snapshot",
                true,
                "controller snapshot status cannot be checked",
            );
        }
    }
    Ok(checks)
}

/// Read-only preflight against the controller: reachable + requested snapshot
/// is `ready` and `bootable`. Never mutates controller or guest state.
pub async fn preflight(url: &str, tag: &str, require_vm: bool) -> Result<Value, CliError> {
    let checks = preflight_checks(url, tag).await?;
    let blocked = checks.iter().any(|check| check.blocked);
    if blocked && require_vm {
        return Err(no_vm(format!(
            "RFB preflight blocked ({} prerequisite(s) missing)",
            checks.iter().filter(|c| c.blocked).count()
        )));
    }
    let check_json: Vec<Value> = checks
        .iter()
        .map(|check| {
            json!({
                "name": check.name,
                "status": if check.blocked { "blocked" } else { "pass" },
                "reason": check.reason,
            })
        })
        .collect();
    Ok(json!({
        "status": if blocked { "blocked" } else { "ready" },
        "snapshot_tag": tag,
        "checks": check_json,
    }))
}

/// Text rendering matching the legacy preflight script format.
pub fn preflight_text(value: &Value) -> String {
    let mut lines = Vec::new();
    let mut blocked = false;
    if let Some(checks) = value.get("checks").and_then(Value::as_array) {
        for check in checks {
            let status = check
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("blocked");
            if status == "blocked" {
                blocked = true;
            }
            lines.push(format!(
                "{}: {} ({})",
                status,
                check.get("name").and_then(Value::as_str).unwrap_or("?"),
                check.get("reason").and_then(Value::as_str).unwrap_or("")
            ));
        }
    }
    lines.push(if blocked {
        "RFB preflight: BLOCKED; no services or system resources were changed.".to_owned()
    } else {
        "RFB preflight: READY".to_owned()
    });
    lines.join("\n")
}
