//! Guest path confinement and NDJSON limit validation for the forkd agent.

use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_PATH: usize = 4096;
pub const MAX_PATTERN: usize = 1024;
pub const MAX_RESULTS: usize = 1000;
pub const MAX_BYTES: usize = 50 * 1024;
pub const MAX_CODE: usize = 1024 * 1024;
pub const WORKSPACE: &str = "/workspace";

/// Effective agent workspace root. The guest default is `/workspace` (tmpfs
/// mounted by the rootfs init); `RFB_AGENT_WORKSPACE` overrides it so
/// host-side contract tests can point the agent at a temporary directory —
/// a CI runner's non-root user cannot create `/workspace`.
pub fn workspace_root() -> &'static str {
    static ROOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        std::env::var("RFB_AGENT_WORKSPACE").unwrap_or_else(|_| WORKSPACE.to_owned())
    })
}

/// Resolve a guest-relative path against the opaque `/workspace` root. The
/// caller only ever sees paths underneath the workspace; absolute paths must
/// stay inside it and any `..` segment is rejected outright.
pub fn guest_path(value: Option<&Value>, _directory: bool) -> io::Result<PathBuf> {
    let raw = value.and_then(Value::as_str).unwrap_or(".");
    if raw.is_empty()
        || raw.len() > MAX_PATH
        || raw.as_bytes().contains(&0)
        || raw.contains('\\')
        || raw.split('/').any(|p| p == "..")
        || raw.len() >= 2 && raw.as_bytes()[1] == b':'
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid guest path",
        ));
    }
    let workspace = workspace_root();
    let rel = raw
        .strip_prefix(workspace)
        .map(|s| s.trim_start_matches('/'))
        .unwrap_or(raw);
    if raw.starts_with('/') && !raw.starts_with(workspace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid guest path",
        ));
    }
    let root = std::fs::canonicalize(Path::new(workspace))
        .unwrap_or_else(|_| Path::new(workspace).to_path_buf());
    let path = root.join(rel);
    // Canonicalize the deepest *existing* ancestor and re-join the remainder
    // lexically. This lets callers create new subdirectories (write already
    // creates parents) instead of failing when the file's parent does not
    // exist yet. The canonicalized prefix must stay inside the workspace.
    let mut existing = path;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.parent() {
            Some(parent) if parent != existing => {
                suffix.push(
                    existing
                        .file_name()
                        .map(|n| n.to_os_string())
                        .unwrap_or_default(),
                );
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let anchored = if existing.exists() {
        std::fs::canonicalize(&existing)?
    } else {
        existing
    };
    if anchored != root && !anchored.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guest path escapes workspace",
        ));
    }
    let final_path = suffix.iter().rev().fold(anchored, |acc, seg| acc.join(seg));
    Ok(final_path)
}

/// Validate a numeric result limit against a hard cap.
pub fn limit(value: Option<&Value>, max: usize, default: usize) -> io::Result<usize> {
    let n = value
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(default);
    if n == 0 || n > max {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guest result limit exceeded",
        ))
    } else {
        Ok(n)
    }
}

/// Validate a search pattern (non-empty, bounded, NUL-free).
pub fn pattern(request: &Value) -> io::Result<&str> {
    let p = request
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pattern must be a string"))?;
    if p.is_empty() || p.len() > MAX_PATTERN || p.as_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid guest pattern",
        ));
    }
    Ok(p)
}
