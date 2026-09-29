//! Rootfs/kernel/static-runtime verification: read-only contract gates and the
//! debugfs/readelf helpers shared by image building and `rfb-cli` checks.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::image_build::build::debugfs_quote;
use crate::cli::image_build::manifest::sha256;
use crate::cli::tool::tool_available;
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

/// Read the rootfs protocol marker and entrypoint stat (contract gate).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
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
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
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
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
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
    if target.contains("musl") && !tool_available("musl-gcc") {
        // The caller may provide its own cross C compiler / linker via the
        // target-specific env keys; only fail when neither is wired. Note the
        // cc crate reads the target in its original (lowercase) form for
        // `CC_*`, while cargo uppercases it for `CARGO_TARGET_*_LINKER`.
        let cc_target = target.replace('-', "_");
        let upper = target.to_uppercase().replace('-', "_");
        let cc_set = std::env::var_os(format!("CC_{cc_target}")).is_some()
            || std::env::var_os(format!("CC_{}", target)).is_some();
        let linker_set = std::env::var_os(format!("CARGO_TARGET_{upper}_LINKER")).is_some();
        if !cc_set && !linker_set {
            return Err(validation(
                "missing musl-gcc; install musl-tools or set CC_<target>/CARGO_TARGET_<TARGET>_LINKER",
            ));
        }
    }
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--release",
        "--target",
        target,
        "-p",
        package,
        "--features",
        features.trim(),
    ])
    .current_dir(root);
    // Wire the musl cross toolchain by default. Without this, the cc crate
    // compiles the embedded C code (vendored Lua, interpreter shims) against
    // host glibc and the link dies with `undefined reference to errno` /
    // `_dl_x86_cpu_features` from glibc's static libm. An env the caller set
    // explicitly always wins. The cc crate reads the target in its original
    // lowercase form (`CC_x86_64_unknown_linux_musl`, or dashed
    // `CC_x86_64-unknown-linux-musl`); a fully uppercased key is ignored.
    if target.contains("musl") {
        let cc_target = target.replace('-', "_");
        if std::env::var_os(format!("CC_{cc_target}")).is_none()
            && std::env::var_os(format!("CC_{}", target)).is_none()
        {
            cmd.env(format!("CC_{cc_target}"), "musl-gcc");
        }
        let upper = target.to_uppercase().replace('-', "_");
        if std::env::var_os(format!("CARGO_TARGET_{upper}_LINKER")).is_none() {
            cmd.env(format!("CARGO_TARGET_{upper}_LINKER"), "musl-gcc");
        }
    }
    let status = cmd
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

/// Whether the binary's bytes contain `needle` — used to verify a runtime was
/// really built with an optional embedded interpreter before an image install
/// hardlinks it as an applet (a dead hardlink fails closed only at guest time).
/// Streams 64 KiB chunks with needle-length overlap, so the binary is never
/// fully resident (it is ~20 MB with interpreters baked in).
pub(super) fn binary_contains(path: &Path, needle: &[u8]) -> Result<bool, CliError> {
    let mut file = fs::File::open(path).map_err(|error| io(error.to_string()))?;
    const CHUNK: usize = 64 * 1024;
    let mut previous = Vec::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let len = read_fill(&mut file, &mut buffer).map_err(|error| io(error.to_string()))?;
        if len == 0 {
            return Ok(false);
        }
        let mut window = Vec::with_capacity(previous.len() + len);
        window.extend_from_slice(&previous);
        window.extend_from_slice(&buffer[..len]);
        if window.windows(needle.len()).any(|w| w == needle) {
            return Ok(true);
        }
        // Keep the last needle.len()-1 bytes so a needle spanning the chunk
        // boundary is still found.
        let keep = needle.len().saturating_sub(1).min(window.len());
        previous = window[window.len() - keep..].to_vec();
    }
}

/// Fill `buf` from `reader`, returning the filled length (short only at EOF).
fn read_fill(reader: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match std::io::Read::read(reader, &mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Byte-compare an installed image file against its source in 64 KiB chunks —
/// neither side is ever fully resident (the runtime binary alone is ~20 MB),
/// so image builds stay flat-memory regardless of payload size. Mirrors
/// [`run_debugfs`]'s error semantics: a byte mismatch, a failed debugfs
/// status, or non-banner stderr is a verification failure even when the
/// streams happen to look equal.
pub fn verify_installed_file(
    image: &Path,
    source: &Path,
    destination: &str,
) -> Result<(), CliError> {
    let mut child = Command::new("debugfs")
        .args(["-R", &format!("cat {}", debugfs_quote(destination))])
        .arg(image)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| external(format!("debugfs failed to start: {error}")))?;
    let mut actual = child.stdout.take().expect("piped stdout");
    let stderr_handle = child.stderr.take().expect("piped stderr");
    // Drain stderr on its own thread: a chatty debugfs must never deadlock
    // the stdout compare loop against a full pipe buffer.
    let stderr_bytes = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = std::io::Read::read_to_end(&mut { stderr_handle }, &mut buffer);
        buffer
    });
    let mut expected = fs::File::open(source).map_err(|error| io(error.to_string()))?;
    const CHUNK: usize = 64 * 1024;
    let mut expected_buf = vec![0u8; CHUNK];
    let mut actual_buf = vec![0u8; CHUNK];
    let mut mismatch = false;
    loop {
        let expected_len = read_fill(&mut expected, &mut expected_buf);
        let actual_len = read_fill(&mut actual, &mut actual_buf);
        match (expected_len, actual_len) {
            (Ok(expected_len), Ok(actual_len))
                if expected_len == actual_len
                    && expected_buf[..expected_len] == actual_buf[..actual_len] =>
            {
                if expected_len == 0 {
                    break;
                }
            }
            _ => {
                mismatch = true;
                break;
            }
        }
    }
    if mismatch {
        let _ = child.kill();
    }
    let stderr = stderr_bytes.join().unwrap_or_default();
    let status = child
        .wait()
        .map_err(|error| external(format!("debugfs wait failed: {error}")))?;
    let stderr_banner_only = String::from_utf8_lossy(&stderr)
        .lines()
        .filter(|line| !line.is_empty())
        .all(|line| line.starts_with("debugfs "));
    if mismatch {
        return Err(external(format!(
            "image file verification failed: {destination}"
        )));
    }
    if !status.success() || (!stderr.is_empty() && !stderr_banner_only) {
        return Err(external(format!(
            "debugfs cat failed ({destination}): {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    Ok(())
}

/// Run a debugfs request against an image, optionally capturing stdout.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn run_debugfs(image: &Path, request: &str, capture_stdout: bool) -> Result<Vec<u8>, CliError> {
    // Keep inspection commands read-only. Only image mutation commands need
    // debugfs write mode; this prevents a validation/inspection path from
    // opening an image writable.
    let mut command = Command::new("debugfs");
    if request.split_whitespace().next().is_some_and(|op| {
        matches!(
            op,
            "mkdir" | "write" | "set_inode_field" | "ln" | "rm" | "unlink"
        )
    }) {
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
