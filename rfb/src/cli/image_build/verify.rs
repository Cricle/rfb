//! Rootfs/kernel/static-runtime verification: read-only contract gates and the
//! debugfs/readelf helpers shared by image building and `rfb-cli` checks.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::image_build::manifest::sha256;
use crate::cli::tool::tool_available;
use serde_json::{json, Value};
use std::{path::Path, process::Command};

/// Read the rootfs protocol marker and entrypoint stat (contract gate).
pub fn inspect_rootfs(image: &Path) -> Result<Value, CliError> {
    let marker = match run_debugfs(image, "cat /etc/rfb-runtime/protocol-version", true) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_owned(),
        Err(_) => "".to_owned(),
    };
    let entrypoint = match run_debugfs(image, "stat /sbin/rfb-runtime", true) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => "".to_owned(),
    };
    Ok(json!({
        "protocol_version": if marker == "1" { "1" } else { "unknown" },
        "entrypoint_regular": entrypoint.contains("Type:") && entrypoint.contains("regular"),
        "entrypoint_executable": entrypoint.contains("0755") || entrypoint.contains("-rwxr-xr-x"),
    }))
}

/// Validate a kernel ELF (ELF64, little-endian, x86_64, with a loadable
/// segment) via `readelf` and compute its SHA-256 digest. Converges the former
/// `check-kernel.sh` gate into the CLI.
pub fn check_kernel(path: &Path) -> Result<Value, CliError> {
    if !path.is_file() {
        return Err(validation(format!(
            "kernel is not a readable regular file: {}",
            path.display()
        )));
    }
    if !tool_available("readelf") {
        return Err(external("readelf is required to validate the kernel"));
    }
    let header = Command::new("readelf")
        .args(["-h", path.to_str().unwrap_or_default()])
        .output()
        .map_err(|error| external(format!("readelf -h failed: {error}")))?;
    if !header.status.success() {
        return Err(validation(format!(
            "readelf -h failed for {}",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&header.stdout);
    let checks = json!({
        "elf64": text.contains("ELF64"),
        "little_endian": text.contains("little endian"),
        "x86_64": text.contains("Advanced Micro Devices X86-64") || text.contains("x86-64"),
    });
    if !checks["elf64"].as_bool().unwrap_or(false) {
        return Err(validation("kernel must be ELF64"));
    }
    if !checks["little_endian"].as_bool().unwrap_or(false) {
        return Err(validation("kernel must be little-endian"));
    }
    if !checks["x86_64"].as_bool().unwrap_or(false) {
        return Err(validation("kernel must target x86_64"));
    }
    let segments = Command::new("readelf")
        .args(["-l", path.to_str().unwrap_or_default()])
        .output()
        .map_err(|error| external(format!("readelf -l failed: {error}")))?;
    if !segments.status.success() {
        return Err(validation(format!(
            "readelf -l failed for {}",
            path.display()
        )));
    }
    let segment_text = String::from_utf8_lossy(&segments.stdout);
    if !segment_text.contains("LOAD") {
        return Err(validation("kernel must contain a loadable segment"));
    }
    let digest = sha256(path).map_err(|error| io(error.to_string()))?;
    let artifact = crate::cli::image_build::ArtifactManifest::for_kernel(path, &digest)?;
    artifact.validate()?;
    let artifact_manifest_path = artifact.write_sidecar(path)?;
    Ok(json!({
        "ok": true,
        "kernel": path,
        "checks": checks,
        "loadable_segment": true,
        "digest": format!("sha256:{digest}"),
        "artifact_manifest_file": artifact_manifest_path,
    }))
}

/// Build the runtime binary for a target triple and enforce static linkage.
/// `features` is the comma-separated cargo feature list (default `cli` at the
/// CLI layer); `rustpython`/`mlua` embed the interpreters.
/// This replaces `image/build-static.sh` while keeping package/build outputs
/// explicit and avoiding any package installation.
pub fn build_static_runtime(
    root: &Path,
    target: &str,
    package: &str,
    features: &str,
) -> Result<Value, CliError> {
    if target.trim().is_empty() || package.trim().is_empty() {
        return Err(validation("target and package must not be empty"));
    }
    if features.trim().is_empty() {
        return Err(validation("features must not be empty"));
    }
    if !tool_available("cargo") {
        return Err(external("cargo is required"));
    }
    if !tool_available("rustup") {
        return Err(external("rustup is required"));
    }
    let installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .current_dir(root)
        .output()
        .map_err(|error| external(format!("rustup target list failed: {error}")))?;
    let installed_text = String::from_utf8_lossy(&installed.stdout);
    if !installed_text.lines().any(|line| line.trim() == target) {
        return Err(validation(format!(
            "missing Rust target {target}; install it with rustup target add {target}"
        )));
    }
    if target.contains("musl")
        && std::env::var_os("CC_x86_64_UNKNOWN_LINUX_MUSL").is_none()
        && !tool_available("musl-gcc")
    {
        return Err(validation(
            "missing musl-gcc; install musl-tools or set CC_x86_64_UNKNOWN_LINUX_MUSL",
        ));
    }
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            target,
            "-p",
            package,
            "--features",
            features.trim(),
        ])
        .current_dir(root)
        .status()
        .map_err(|error| external(format!("cargo build failed to start: {error}")))?;
    if !status.success() {
        return Err(external(format!("cargo build failed with {status}")));
    }
    // `cargo build` honors CARGO_TARGET_DIR, so resolve the output against it
    // as well; otherwise a caller that exports it would never find the binary.
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let binary = target_dir.join(target).join("release").join(package);
    if !binary.is_file() {
        return Err(external(format!(
            "built binary is missing: {}",
            binary.display()
        )));
    }
    if is_dynamically_linked(&binary)? {
        return Err(validation(
            "runtime is dynamically linked; refusing final image",
        ));
    }
    let digest = sha256(&binary).map_err(|error| io(error.to_string()))?;
    let artifact = crate::cli::image_build::ArtifactManifest::for_static_runtime(
        &binary, &digest, package, target,
    )?;
    artifact.validate()?;
    let artifact_manifest_path = artifact.write_sidecar(&binary)?;
    Ok(json!({
        "ok": true,
        "target": target,
        "package": package,
        "features": features.trim(),
        "binary": binary,
        "linkage": "static",
        "digest": format!("sha256:{digest}"),
        "artifact_manifest_file": artifact_manifest_path,
    }))
}

pub(super) fn is_dynamically_linked(path: &Path) -> Result<bool, CliError> {
    if tool_available("readelf") {
        let output = Command::new("readelf")
            .args(["-l", path.to_str().unwrap_or_default()])
            .output()
            .map_err(|error| external(error.to_string()))?;
        if !output.status.success() {
            return Err(external(format!(
                "readelf -l failed for {}",
                path.display()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).contains("INTERP"))
    } else if tool_available("file") {
        let output = Command::new("file")
            .arg(path)
            .output()
            .map_err(|error| external(error.to_string()))?;
        Ok(String::from_utf8_lossy(&output.stdout).contains("dynamically linked"))
    } else {
        Err(validation(
            "readelf or file is required to verify runtime linkage",
        ))
    }
}

/// Run a debugfs request against an image, optionally capturing stdout.
pub fn run_debugfs(image: &Path, request: &str, capture_stdout: bool) -> Result<Vec<u8>, CliError> {
    // Keep inspection commands read-only. Only image mutation commands need
    // debugfs write mode; this prevents a validation/inspection path from
    // opening an image writable.
    let mut command = Command::new("debugfs");
    if request
        .split_whitespace()
        .next()
        .is_some_and(|op| matches!(op, "mkdir" | "write" | "set_inode_field" | "ln"))
    {
        command.arg("-w");
    }
    let output = command
        .args(["-R", request])
        .arg(image)
        .output()
        .map_err(|error| external(format!("debugfs failed to start: {error}")))?;
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let operation = request.split_whitespace().next().unwrap_or_default();
    let stderr_is_only_banner = output
        .stderr
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .all(|line| String::from_utf8_lossy(line).starts_with("debugfs "));
    if !output.status.success()
        || (!output.stderr.is_empty() && !stderr_is_only_banner)
        || (matches!(operation, "write" | "mkdir" | "set_inode_field")
            && ["usage:", "file not found", "error:", "no such", "couldn't"]
                .iter()
                .any(|marker| diagnostics.to_ascii_lowercase().contains(marker)))
    {
        return Err(external(format!(
            "debugfs request failed ({request}): {}",
            diagnostics.trim()
        )));
    }
    Ok(if capture_stdout {
        if output.stdout.is_empty() {
            output.stderr
        } else {
            output.stdout
        }
    } else {
        Vec::new()
    })
}
