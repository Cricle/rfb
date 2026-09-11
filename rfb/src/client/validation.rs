//! Local, fail-closed validation (`sdk/PROTOCOL.md` §2.3). Every rule runs
//! before a request is sent so invalid input never reaches the wire.

use super::RfbError;
use crate::guest::{MAX_GUEST_CODE_BYTES, MAX_GUEST_PATH_BYTES, MAX_GUEST_PATTERN_BYTES};

fn invalid(message: &'static str) -> RfbError {
    RfbError::Validation(message.to_owned())
}

/// Validate a structured fs path (`ls`/`find`/`grep`).
pub(super) fn fs_path(path: &str) -> Result<(), RfbError> {
    if path.is_empty()
        || path.len() > MAX_GUEST_PATH_BYTES
        || path.as_bytes().contains(&0)
        // PROTOCOL.md §2.3: an absolute fs path must be /workspace itself or
        // a child of it — "/workspacefoo" is NOT inside the workspace.
        || (path.starts_with('/')
            && path != "/workspace"
            && !path.starts_with("/workspace/"))
        || path.contains('\\')
        || path.split('/').any(|component| component == "..")
    {
        return Err(invalid(
            "fs path must be non-empty, at most 4096 bytes, backslash-free, and non-escaping",
        ));
    }
    Ok(())
}

/// Validate a file path (`read`/`write` and `eval`/`stream` cwd).
pub(super) fn file_path(path: &str) -> Result<(), RfbError> {
    if path.is_empty()
        || path.len() > MAX_GUEST_PATH_BYTES
        || path.as_bytes().contains(&0)
        || path.contains('\\')
        || (path.len() >= 2 && path.as_bytes()[1] == b':')
        || path.split('/').any(|component| component == "..")
    {
        return Err(invalid(
            "file path must be non-empty, at most 4096 bytes, backslash-free, drive-free, and non-escaping",
        ));
    }
    Ok(())
}

/// Validate a `find`/`grep` pattern.
pub(super) fn pattern(pattern: &str) -> Result<(), RfbError> {
    if pattern.is_empty() || pattern.as_bytes().contains(&0) {
        return Err(invalid("pattern must be non-empty and NUL-free"));
    }
    if pattern.len() > MAX_GUEST_PATTERN_BYTES {
        return Err(invalid("pattern exceeds 1024 bytes"));
    }
    Ok(())
}

/// Validate a quantity limit: strictly positive and within `max`.
pub(super) fn limit(value: usize, max: usize, what: &'static str) -> Result<(), RfbError> {
    if value == 0 || value > max {
        return Err(invalid(what));
    }
    Ok(())
}

/// Validate eval source: non-empty after trimming, at most 1 MiB.
pub(super) fn eval_code(code: &str) -> Result<(), RfbError> {
    if code.trim().is_empty() {
        return Err(invalid("eval code must not be empty"));
    }
    if code.len() > MAX_GUEST_CODE_BYTES {
        return Err(invalid("eval code exceeds 1 MiB"));
    }
    Ok(())
}

/// Validate a timeout expressed in seconds: finite and strictly positive.
/// Returns the ceil-ed whole-second value used on the NDJSON wire.
pub(super) fn timeout_secs(timeout_s: f64) -> Result<u64, RfbError> {
    if !timeout_s.is_finite() || timeout_s <= 0.0 {
        return Err(invalid("timeout must be a positive number of seconds"));
    }
    Ok((timeout_s.ceil() as u64).max(1))
}

/// Validate a forkd sandbox id: `[A-Za-z0-9_-]{1,128}`.
pub(super) fn sandbox_id(id: &str) -> Result<(), RfbError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid("sandbox id must match [A-Za-z0-9_-]{1,128}"));
    }
    Ok(())
}
