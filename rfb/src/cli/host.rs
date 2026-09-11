//! Host capability detection (Linux/WSL/arch/KVM + tool matrix).
//!
//! This is the single implementation the CLI and all its subcommands use for
//! "can I run a real VM here?" decisions, replacing the per-script heuristics
//! that used to live in `preflight.sh` and `setup-runtime.sh`.

use crate::cli::error::{no_vm, usage, CliError};
use crate::cli::tool::{host_kind, kvm_available, HostKind};
use serde_json::{json, Value};

/// Host capability summary returned by `doctor`/`preflight`.
#[derive(Debug, Clone)]
pub struct HostCapabilities {
    /// Detected platform kind (Linux / WSL / Windows).
    pub kind: HostKind,
    /// Detected CPU architecture, e.g. `x86_64`.
    pub arch: String,
    /// Whether KVM (`/dev/kvm`) is available on this host.
    pub kvm: bool,
}

/// Detect the current host capabilities without running any commands.
pub fn detect() -> HostCapabilities {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64".to_owned()
    } else {
        std::env::consts::ARCH.to_owned()
    };
    HostCapabilities {
        kind: host_kind(),
        arch,
        kvm: kvm_available(),
    }
}

/// Render the host capability block as JSON (used by `doctor --json`).
pub fn capability_json(caps: &HostCapabilities, tools: &Value) -> Value {
    let kind = match caps.kind {
        HostKind::Linux => "linux",
        HostKind::Wsl => "wsl",
        HostKind::Windows => "windows",
    };
    json!({
        "platform": kind,
        "arch": caps.arch,
        "kvm": caps.kvm,
        "tools": tools,
    })
}

/// Human-readable capability block for non-JSON `doctor`.
pub fn capability_text(caps: &HostCapabilities, tools: &Value) -> String {
    let kind = match caps.kind {
        HostKind::Linux => "Linux",
        HostKind::Wsl => "WSL",
        HostKind::Windows => "Windows",
    };
    let tools_text = match tools {
        Value::Object(map) => {
            let mut lines: Vec<String> = map
                .iter()
                .map(|(name, available)| {
                    format!(
                        "{name}: {}",
                        if available.as_bool().unwrap_or(false) {
                            "available"
                        } else {
                            "missing"
                        }
                    )
                })
                .collect();
            lines.sort();
            lines.join("\n")
        }
        _ => "tools: unknown".to_owned(),
    };
    format!(
        "platform: {kind}\narch: {}\nkvm: {}\n{tools_text}",
        caps.arch,
        if caps.kvm { "available" } else { "unavailable" }
    )
}

/// Validate an architecture from CLI arguments against the current host.
pub fn require_arch(arch: &str) -> Result<(), CliError> {
    let caps = detect();
    if arch != caps.arch {
        return Err(usage(format!(
            "target architecture {arch} does not match host {}; use --target or --arch on a matching host",
            caps.arch
        )));
    }
    Ok(())
}

/// Require a real VM-capable Linux/WSL host (KVM + x86_64). Missing VM
/// prerequisites carry the documented exit-12 contract (`EXIT_NOVM`), not the
/// usage exit code — acceptance gates and CI assert on it.
pub fn require_vm_host() -> Result<HostCapabilities, CliError> {
    let caps = detect();
    if !matches!(caps.kind, HostKind::Linux | HostKind::Wsl) {
        return Err(no_vm("a Linux/WSL host is required for VM operations"));
    }
    if caps.arch != "x86_64" {
        return Err(no_vm(
            "x86_64 host architecture is required for VM operations",
        ));
    }
    if !caps.kvm {
        return Err(no_vm("/dev/kvm is unavailable or not read-write"));
    }
    Ok(caps)
}

/// Run the read-only preflight gate. `require_vm` makes blocked prerequisites
/// a hard error carrying the documented `EXIT_NOVM=12` contract; otherwise the
/// caller reports SKIP.
pub fn preflight(require_vm: bool) -> Result<HostCapabilities, CliError> {
    let caps = detect();
    if !matches!(caps.kind, HostKind::Linux | HostKind::Wsl) {
        if require_vm {
            return Err(no_vm("Linux/WSL required"));
        }
        return Ok(caps);
    }
    if caps.arch != "x86_64" && require_vm {
        return Err(no_vm("x86_64 required"));
    }
    if !caps.kvm && require_vm {
        return Err(no_vm("/dev/kvm is unavailable or not read-write"));
    }
    Ok(caps)
}
