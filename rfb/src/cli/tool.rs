//! Cross-platform discovery of the external tools `rfb-cli` relies on.
//!
//! The resolver knows whether the process is running on native Linux, inside
//! WSL, or on Windows (Git Bash), and applies the right name resolution and
//! path handling for each. The same probe powers `doctor`, `env setup`, and
//! the various acceptance commands so a user gets one consistent capability
//! matrix instead of per-script heuristics.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

/// Host classification used to decide how tools and paths are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    /// Native Linux (including CI containers).
    Linux,
    /// WSL2 (Linux kernel under Windows).
    Wsl,
    /// Windows host (Git Bash / MSYS). RFB VM commands are not runnable here.
    Windows,
}

/// Detect the current host kind.
pub fn host_kind() -> HostKind {
    #[cfg(windows)]
    {
        HostKind::Windows
    }
    #[cfg(unix)]
    {
        let uts = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        if uts.to_ascii_lowercase().contains("microsoft")
            || std::env::var_os("WSL_INTEROP").is_some()
            || std::env::var_os("WSL_DISTRO_NAME").is_some()
        {
            HostKind::Wsl
        } else if std::env::consts::OS == "linux" {
            HostKind::Linux
        } else {
            HostKind::Windows
        }
    }
}

/// True when this process can run a real KVM/Firecracker VM.
pub fn kvm_available() -> bool {
    if !matches!(host_kind(), HostKind::Linux | HostKind::Wsl) {
        return false;
    }
    kvm_world_writable()
}

#[cfg(unix)]
fn kvm_world_writable() -> bool {
    // "/dev/kvm" is typically group-owned (crw-rw---- root:kvm) and the
    // operator belongs to the kvm group, so the world-write bit is not a
    // reliable signal. The real gate is whether this process can open the
    // device read-write; do that instead of guessing from mode bits.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

#[cfg(not(unix))]
fn kvm_world_writable() -> bool {
    false
}

/// Map a bare tool name to the command that should be invoked on this host.
/// On Windows we append `.exe` for well-known tools so `Command` finds them.
pub fn tool_command(name: &str) -> String {
    match host_kind() {
        HostKind::Windows if !name.ends_with(".exe") => format!("{name}.exe"),
        _ => name.to_owned(),
    }
}

/// Check whether an external tool is present and returns a non-empty version
/// line (a stronger signal than `which` alone on systems where stderr is the
/// version sink).
pub fn tool_available(name: &str) -> bool {
    let command = tool_command(name);
    Command::new(&command)
        .arg("-V")
        .output()
        .map(|output| output.status.success() || !output.stderr.is_empty())
        .unwrap_or(false)
}

/// Resolve an executable name to an absolute path if it is on `PATH`.
pub fn resolve_binary(name: &str) -> Option<PathBuf> {
    let command = tool_command(name);
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(&command);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The full set of tools the CLI may consult, as a JSON object of
/// `name -> available` for `doctor`/`env setup`.
pub fn tool_matrix() -> Value {
    let names = [
        "cargo",
        "rustc",
        "rustup",
        "mke2fs",
        "debugfs",
        "readelf",
        "file",
        "sha256sum",
        "stat",
        "truncate",
        "firecracker",
        "curl",
        "python3",
    ];
    let linux_only = [
        "mke2fs",
        "debugfs",
        "readelf",
        "sha256sum",
        "stat",
        "truncate",
        "firecracker",
    ];
    let native_vm = matches!(host_kind(), HostKind::Linux | HostKind::Wsl);
    let mut tools = serde_json::Map::new();
    for name in names {
        // These binaries are not meaningful prerequisites on native Windows;
        // report them as not-applicable rather than falsely missing.
        if !native_vm && linux_only.contains(&name) {
            tools.insert(name.to_owned(), json!("not-applicable"));
        } else {
            tools.insert(name.to_owned(), json!(tool_available(name)));
        }
    }
    Value::Object(tools)
}

/// Human-readable capability summary used by non-JSON `doctor`.
pub fn format_matrix() -> String {
    let mut lines = Vec::new();
    let tools = tool_matrix();
    if let Value::Object(map) = &tools {
        for (name, available) in map {
            let state = match available {
                Value::Bool(true) => "available",
                Value::Bool(false) => "missing",
                Value::String(value) if value == "not-applicable" => "not-applicable",
                _ => "unknown",
            };
            lines.push(format!("{name}: {state}"));
        }
    }
    lines.join("\n")
}

/// Normalize a Windows-style path (`C:\...`) to a WSL-style path
/// (`/mnt/c/...`) so a command started inside WSL can pass it to Linux tools.
pub fn normalize_for_wsl(path: &str) -> String {
    if !cfg!(unix) {
        return path.to_owned();
    }
    let lower = path.to_ascii_lowercase();
    if lower.len() >= 2
        && lower.as_bytes()[1] == b':'
        && (lower.as_bytes()[0] as char).is_ascii_alphabetic()
    {
        let drive = lower.as_bytes()[0] as char;
        let rest = path[2..].replace('\\', "/");
        return format!("/mnt/{}{}", drive.to_ascii_lowercase(), rest);
    }
    path.replace('\\', "/")
}
