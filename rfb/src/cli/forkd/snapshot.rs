//! Thin wrappers over the official `forkd` snapshot management CLI.
//!
//! These commands delegate snapshot create/info/delete to the official `forkd`
//! binary (`forkd snapshot`, `forkd snapshot-info`, `forkd rmi`) instead of
//! re-implementing controller internals. Resources (kernel, rootfs, tap, forkd
//! binary) are resolved from explicit flags, `resx/`, and the environment.
//!
//! Safety invariants carried over from the scripts and docs:
//! - the controller URL must be a loopback target ([`crate::cli::localhost`]);
//! - the snapshot tag must satisfy the forkd naming constraint;
//! - kernel/rootfs must be readable regular files (and, for rootfs, must not be
//!   an rfb-runtime vsock image);
//! - assets under `resx/` are cross-checked against `resx/SHA256SUMS` when
//!   available;
//! - output is sanitized (never echoes raw daemon payloads or the token), and
//!   provenance verification remains fail-closed.

use crate::cli::commands::{
    ForkdSnapshotCreateArgs, ForkdSnapshotDeleteArgs, ForkdSnapshotInfoArgs,
};
use crate::cli::error::{external, io, validation, CliError};
use crate::cli::image_build::sha256;
use crate::cli::localhost::{require_localhost, require_snapshot_tag};
use crate::cli::tool::{resolve_binary, tool_available};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::snapshot_paths::prepare_rootfs_private_copy;

/// A sanitized snapshot operation result: the machine-readable value plus a
/// short human-readable summary line.
pub struct SnapshotOutput {
    /// Structured result emitted for `--json` mode.
    pub value: Value,
    /// Short human-readable summary emitted for text mode.
    pub text: String,
}

/// Locate the official `forkd` CLI binary: `--forkd-bin`, `FORKD_BIN`,
/// `resx/forkd/forkd`, or `forkd` on PATH. Assets resolved from `resx/` are
/// cross-checked against `resx/SHA256SUMS`.
pub fn resolve_forkd_bin(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    let candidate = if let Some(path) = explicit {
        if !path.is_file() {
            return Err(validation(format!(
                "forkd binary is not a readable regular file: {}",
                path.display()
            )));
        }
        path.to_path_buf()
    } else if let Ok(value) = std::env::var("FORKD_BIN") {
        if !value.trim().is_empty() {
            let path = PathBuf::from(&value);
            if !path.is_file() {
                return Err(validation(format!(
                    "FORKD_BIN is not a readable regular file: {value}"
                )));
            }
            path
        } else {
            resolve_forkd_bin_search()?
        }
    } else {
        resolve_forkd_bin_search()?
    };
    verify_resx_checksum(&candidate, "forkd binary")?;
    Ok(candidate)
}

fn resolve_forkd_bin_search() -> Result<PathBuf, CliError> {
    if let Some(resx) = find_resx() {
        let candidate = resx.join("forkd").join("forkd");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(path) = resolve_binary("forkd") {
        return Ok(path);
    }
    Err(external(
        "forkd CLI binary not found; set FORKD_BIN, place it at resx/forkd/forkd, or add forkd to PATH",
    ))
}

/// Locate the `resx/` asset tree by walking up from the current directory.
pub fn find_resx() -> Option<PathBuf> {
    std::env::current_dir().ok()?.ancestors().find_map(|dir| {
        let candidate = dir.join("resx");
        candidate.is_dir().then_some(candidate)
    })
}

/// Cross-check an asset digest against `resx/SHA256SUMS` when the asset lives
/// under `resx/` and a checksum file can be read. Missing sums are tolerated
/// (best-effort), mismatches are a hard validation failure.
fn verify_resx_checksum(path: &Path, label: &str) -> Result<(), CliError> {
    let Some(resx) = find_resx() else {
        return Ok(());
    };
    let relative = match path.strip_prefix(&resx) {
        Ok(relative) => relative,
        Err(_) => return Ok(()), // asset is outside the resx tree
    };
    let sums = resx.join("SHA256SUMS");
    if !sums.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&sums).map_err(|error| io(error.to_string()))?;
    let key = format!("resx/{}", relative.to_string_lossy().replace('\\', "/"));
    let expected = content.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == key).then(|| hash.to_ascii_lowercase())
    });
    if let Some(expected) = expected {
        let actual = sha256(path).map_err(|error| io(error.to_string()))?;
        if actual != expected {
            return Err(validation(format!(
                "{label} sha256 mismatch for {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Require a path to be a readable, non-empty regular file.
fn require_asset(path: &Path, label: &str) -> Result<(), CliError> {
    if !path.is_file() {
        return Err(validation(format!(
            "{label} is not a readable regular file: {}",
            path.display()
        )));
    }
    if let Ok(metadata) = fs::metadata(path) {
        if metadata.len() == 0 {
            return Err(validation(format!("{label} is empty: {}", path.display())));
        }
    }
    verify_resx_checksum(path, label)
}

/// Reject rfb-runtime vsock rootfs images (protocol family isolation).
fn reject_vsock_rootfs(path: &Path) -> Result<(), CliError> {
    if tool_available("debugfs") && super::preflight::debugfs_has(path, "/sbin/rfb-runtime") {
        return Err(validation(format!(
            "rootfs {} is the rfb-runtime vsock image, not forkd-agent",
            path.display()
        )));
    }
    Ok(())
}

/// Resolve the kernel: `--kernel`, `FORKD_KERNEL`, or `resx/kernel` (preferring
/// `vmlinux-arcbox-0.0.24`, else the first `vmlinux*` file).
fn resolve_kernel(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        require_asset(path, "kernel")?;
        return Ok(path.to_path_buf());
    }
    if let Ok(value) = std::env::var("FORKD_KERNEL") {
        if !value.trim().is_empty() {
            let path = PathBuf::from(&value);
            require_asset(&path, "kernel")?;
            return Ok(path);
        }
    }
    if let Some(resx) = find_resx() {
        let kernel_dir = resx.join("kernel");
        let preferred = kernel_dir.join("vmlinux-arcbox-0.0.24");
        if preferred.is_file() {
            verify_resx_checksum(&preferred, "kernel")?;
            return Ok(preferred);
        }
        if let Ok(entries) = fs::read_dir(&kernel_dir) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("vmlinux"))
                })
                .collect();
            files.sort();
            if let Some(path) = files.into_iter().next() {
                verify_resx_checksum(&path, "kernel")?;
                return Ok(path);
            }
        }
    }
    Err(validation(
        "kernel not found; pass --kernel, set FORKD_KERNEL, or place a vmlinux under resx/kernel",
    ))
}

/// Resolve the rootfs: `--rootfs`, `FORKD_ROOTFS`, or `resx/rootfs` (preferring
/// `forkd-agent.ext4`). Rejects rfb-runtime vsock images.
fn resolve_rootfs(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        require_asset(path, "rootfs")?;
        reject_vsock_rootfs(path)?;
        return Ok(path.to_path_buf());
    }
    if let Ok(value) = std::env::var("FORKD_ROOTFS") {
        if !value.trim().is_empty() {
            let path = PathBuf::from(&value);
            require_asset(&path, "rootfs")?;
            reject_vsock_rootfs(&path)?;
            return Ok(path);
        }
    }
    if let Some(resx) = find_resx() {
        let rootfs_dir = resx.join("rootfs");
        let preferred = rootfs_dir.join("forkd-agent.ext4");
        if preferred.is_file() {
            verify_resx_checksum(&preferred, "rootfs")?;
            reject_vsock_rootfs(&preferred)?;
            return Ok(preferred);
        }
        if let Ok(entries) = fs::read_dir(&rootfs_dir) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && !path.file_name().is_some_and(|n| {
                            let name = n.to_string_lossy();
                            name.contains("rfb-runtime") || name.contains("zeroboot")
                        })
                })
                .collect();
            files.sort();
            if let Some(path) = files.into_iter().next() {
                verify_resx_checksum(&path, "rootfs")?;
                reject_vsock_rootfs(&path)?;
                return Ok(path);
            }
        }
    }
    Err(validation(
        "forkd-agent rootfs not found; pass --rootfs, set FORKD_ROOTFS, or place forkd-agent.ext4 under resx/rootfs",
    ))
}

/// Resolve the host tap device: `--tap`, `FORKD_TAP`, or `forkd-tap0`.
fn resolve_tap(explicit: Option<&str>) -> Result<String, CliError> {
    let tap = explicit
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("FORKD_TAP")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "forkd-tap0".to_owned());
    let ok = tap.len() <= 64
        && tap
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !ok {
        return Err(validation(format!("invalid tap device name: {tap}")));
    }
    Ok(tap)
}

/// Run the forkd binary with a sanitized failure message. The token is never
/// placed on the command line; it is inherited via `FORKD_TOKEN` in the child
/// environment (matching the official `forkd` `--daemon-token` env contract).
fn run_forkd(bin: &Path, args: &[String]) -> Result<Output, CliError> {
    Command::new(bin).args(args).output().map_err(|error| {
        external(format!(
            "failed to run forkd binary {}: {error}",
            bin.display()
        ))
    })
}

/// Redact potential credentials (the controller token) and bound output length.
fn sanitize_output(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).trim().to_owned();
    if let Ok(token) = std::env::var("FORKD_TOKEN") {
        if !token.is_empty() {
            text = text.replace(&token, "***");
        }
    }
    const MAX: usize = 2000;
    if text.chars().count() > MAX {
        text = text.chars().take(MAX).collect::<String>() + "\n...(truncated)";
    }
    text
}

/// Summarize the provenance record attached to a snapshot-info payload.
///
/// Fail-closed semantics: without a complete record carrying kernel/memory/vmstate
/// digests the provenance is `partial`/`unverified`, never promoted to `complete`.
pub fn provenance_state(provenance: Option<&Value>) -> &'static str {
    let Some(value) = provenance else {
        return "unavailable";
    };
    if !value.is_object() {
        return "unavailable";
    }
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    if status.eq_ignore_ascii_case("complete") {
        let has_kernel = value.get("kernel_sha256").and_then(Value::as_str).is_some();
        let has_memory = value.get("memory_sha256").and_then(Value::as_str).is_some();
        let has_vmstate = value
            .get("vmstate_sha256")
            .and_then(Value::as_str)
            .is_some();
        if has_kernel && has_memory && has_vmstate {
            "complete"
        } else {
            "partial"
        }
    } else if status.is_empty() || status.eq_ignore_ascii_case("partial") {
        "partial"
    } else if status.eq_ignore_ascii_case("unverified") {
        "unverified"
    } else {
        "partial"
    }
}

/// Extract a safe, whitelisted summary from a raw snapshot-info daemon payload.
/// The opaque provenance record is reduced to a status string; paths and body
/// fields that are not on the allowlist are dropped.
pub fn sanitize_snapshot_info(raw: Value) -> Value {
    if !raw.is_object() {
        return json!({"status": "unavailable", "provenance": "unavailable"});
    }
    let mut out = serde_json::Map::new();
    for key in [
        "tag",
        "status",
        "bootable",
        "digest",
        "created_at_unix",
        "branched_from",
        "pause_ms",
        "diff_ms",
        "diff_physical_bytes",
        "diff_logical_bytes",
        "warning",
    ] {
        if let Some(value) = raw.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    let provenance = raw.get("provenance").cloned();
    out.insert(
        "provenance".to_owned(),
        json!(provenance_state(provenance.as_ref())),
    );
    Value::Object(out)
}

/// `rfb-cli forkd snapshot-info`: fetch the daemon info record via the official
/// `forkd snapshot-info --json` and render a sanitized summary. With
/// `--require-provenance` the command fails closed unless provenance is
/// complete.
pub fn snapshot_info(args: &ForkdSnapshotInfoArgs) -> Result<SnapshotOutput, CliError> {
    let url = require_localhost(&args.url)?;
    let tag = require_snapshot_tag(&args.tag)?;
    let bin = resolve_forkd_bin(args.forkd_bin.as_deref())?;
    let argv: Vec<String> = vec![
        "snapshot-info".into(),
        "--json".into(),
        "--daemon-url".into(),
        url.clone(),
        tag.to_owned(),
    ];
    let output = run_forkd(&bin, &argv)?;
    if !output.status.success() {
        return Err(external(format!(
            "forkd snapshot-info failed: {}",
            sanitize_output(&output.stderr)
        )));
    }
    let raw: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        external(format!(
            "forkd snapshot-info returned invalid JSON: {error}"
        ))
    })?;
    let value = sanitize_snapshot_info(raw);
    let provenance = value["provenance"]
        .as_str()
        .unwrap_or("unavailable")
        .to_owned();
    if args.require_provenance && provenance != "complete" {
        return Err(validation(format!(
            "snapshot {tag} provenance is {provenance}; --require-provenance fails closed"
        )));
    }
    let status = value["status"].as_str().unwrap_or("unknown");
    let text = format!("snapshot {tag}: status {status}, provenance {provenance}");
    Ok(SnapshotOutput { value, text })
}

/// `rfb-cli forkd snapshot-create`: delegate to `forkd snapshot`, then fetch
/// the post-creation info record (operations guide: 创建后必须调用 info). With
/// `--require-provenance` the create fails closed when the resulting snapshot
/// cannot be confirmed as having complete provenance.
///
/// The resolved rootfs is first copied into the forkd snapshots data directory
/// (`$XDG_DATA_HOME/forkd/snapshots/<tag>/rootfs.ext4`, else
/// `$HOME/.local/share/forkd/snapshots/<tag>/rootfs.ext4`) and the copy is what
/// gets passed to `forkd snapshot`, so the rw boot never pollutes the artifact
/// image that `--rootfs`/`FORKD_ROOTFS` resolved to. The output reports both
/// the artifact rootfs and the private copy path.
pub fn snapshot_create(args: &ForkdSnapshotCreateArgs) -> Result<SnapshotOutput, CliError> {
    let url = require_localhost(&args.url)?;
    let tag = require_snapshot_tag(&args.tag)?;
    if args.boot_wait_secs == 0 {
        return Err(validation("--boot-wait-secs must be a positive integer"));
    }
    if let Some(mib) = args.mem_size_mib {
        if mib == 0 {
            return Err(validation("--mem-size-mib must be a positive integer"));
        }
    }
    let bin = resolve_forkd_bin(args.forkd_bin.as_deref())?;
    let kernel = resolve_kernel(args.kernel.as_deref())?;
    let rootfs = resolve_rootfs(args.rootfs.as_deref())?;
    let tap = resolve_tap(args.tap.as_deref())?;
    // Private writable copy: keep the artifact rootfs read-only for forkd's rw
    // boot. Do this only after every resolution succeeded so a validation
    // failure never leaves a stray copy behind.
    let rootfs_copy = prepare_rootfs_private_copy(&rootfs, tag, args.rootfs_copy.as_deref())?;

    let mut argv: Vec<String> = vec![
        "snapshot".into(),
        "--tag".into(),
        tag.to_owned(),
        "--kernel".into(),
        kernel.to_string_lossy().into_owned(),
        "--rootfs".into(),
        rootfs_copy.to_string_lossy().into_owned(),
        "--tap".into(),
        tap.clone(),
    ];
    if let Some(mib) = args.mem_size_mib {
        argv.push("--mem-size-mib".into());
        argv.push(mib.to_string());
    }
    argv.push("--boot-wait-secs".into());
    argv.push(args.boot_wait_secs.to_string());
    argv.push("--daemon-url".into());
    argv.push(url.clone());

    let output = run_forkd(&bin, &argv)?;
    if !output.status.success() {
        return Err(external(format!(
            "forkd snapshot failed: {}",
            sanitize_output(&output.stderr)
        )));
    }

    let mut value = json!({
        "ok": true,
        "tag": tag,
        "created": true,
        "delegated": "forkd",
        "kernel": kernel.to_string_lossy(),
        "rootfs": rootfs.to_string_lossy(),
        "rootfs_copy": rootfs_copy.to_string_lossy(),
        "tap": tap,
    });
    // Post-create info per operations guide. If the daemon is unavailable the
    // snapshot still exists; provenance then cannot be confirmed, which is a
    // fail-closed state only when --require-provenance was requested.
    let info_argv: Vec<String> = vec![
        "snapshot-info".into(),
        "--json".into(),
        "--daemon-url".into(),
        url.clone(),
        tag.to_owned(),
    ];
    match run_forkd(&bin, &info_argv) {
        Ok(info_output) if info_output.status.success() => {
            let raw: Value = serde_json::from_slice(&info_output.stdout)
                .unwrap_or_else(|_| json!({"status": "unreadable", "provenance": null}));
            let info = sanitize_snapshot_info(raw);
            let provenance = info["provenance"]
                .as_str()
                .unwrap_or("unavailable")
                .to_owned();
            if args.require_provenance && provenance != "complete" {
                return Err(validation(format!(
                    "snapshot {tag} was created but its provenance is {provenance}; --require-provenance fails closed"
                )));
            }
            if let Value::Object(map) = &mut value {
                map.insert("snapshot_info".to_owned(), info);
            }
        }
        Ok(info_output) => {
            let detail = sanitize_output(info_output.stderr.as_slice());
            if args.require_provenance {
                return Err(external(format!(
                    "snapshot {tag} was created but its info record could not be fetched: {detail}"
                )));
            }
            if let Value::Object(map) = &mut value {
                map.insert("info".to_owned(), json!("unavailable"));
            }
        }
        Err(error) => {
            if args.require_provenance {
                return Err(validation(format!(
                    "snapshot {tag} was created but its info record could not be fetched (fail-closed): {}",
                    error.message,
                )));
            }
            if let Value::Object(map) = &mut value {
                map.insert("info".to_owned(), json!("unavailable"));
            }
        }
    }

    let text = format!("snapshot {tag} created (delegated to forkd)");
    Ok(SnapshotOutput { value, text })
}

/// `rfb-cli forkd snapshot-delete`: delegate to `forkd rmi`. `--force` and
/// `--cascade` mirror the official flags; by default the deletion is refused if
/// it would orphan child snapshots.
pub fn snapshot_delete(args: &ForkdSnapshotDeleteArgs) -> Result<SnapshotOutput, CliError> {
    let url = require_localhost(&args.url)?;
    let tag = require_snapshot_tag(&args.tag)?;
    if args.force && args.cascade {
        return Err(validation("--force and --cascade are mutually exclusive"));
    }
    let bin = resolve_forkd_bin(args.forkd_bin.as_deref())?;
    let mut argv: Vec<String> = vec!["rmi".into(), "--daemon-url".into(), url.clone()];
    if args.cascade {
        argv.push("--cascade".into());
    }
    if args.force {
        argv.push("--force".into());
    }
    argv.push(tag.to_owned());
    let output = run_forkd(&bin, &argv)?;
    if !output.status.success() {
        return Err(external(format!(
            "forkd rmi failed: {}",
            sanitize_output(&output.stderr)
        )));
    }
    Ok(SnapshotOutput {
        value: json!({"ok": true, "tag": tag, "deleted": true}),
        text: format!("snapshot {tag} deleted"),
    })
}
