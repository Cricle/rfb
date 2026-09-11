//! Guest-side limits and shared path/pattern/id validators.

use crate::ContractError;

/// Upper bound on a guest filesystem path, in bytes.
pub const MAX_GUEST_PATH_BYTES: usize = 4096;
/// Upper bound on a `find`/`grep` pattern, in bytes.
pub const MAX_GUEST_PATTERN_BYTES: usize = 1024;
/// Upper bound on the number of `ls`/`find`/`grep` results returned.
pub const MAX_GUEST_RESULTS: usize = 1000;
/// Upper bound on a single guest payload (`grep` bytes, `read` bytes, `write` bytes).
pub const MAX_GUEST_RESULT_BYTES: usize = 50 * 1024;
/// Upper bound on an `eval` source snippet, in bytes.
pub const MAX_GUEST_CODE_BYTES: usize = 1024 * 1024;

pub(super) fn default_guest_path() -> String {
    ".".to_owned()
}

pub(super) fn default_guest_results() -> usize {
    MAX_GUEST_RESULTS
}

pub(super) fn default_guest_result_bytes() -> usize {
    MAX_GUEST_RESULT_BYTES
}

pub(super) fn validate_fs_path(path: &str) -> Result<(), ContractError> {
    // Structured filesystem RPCs accept a guest-relative path or the canonical
    // forkd workspace root. Host paths and other guest roots remain rejected.
    if path.is_empty()
        || path.len() > MAX_GUEST_PATH_BYTES
        || path.as_bytes().contains(&0)
        || (path.starts_with('/') && !path.starts_with("/workspace"))
        || path.starts_with('\\')
        || path.contains('\\')
        || path.split('/').any(|component| component == "..")
    {
        return Err(ContractError::InvalidPath(
            "must be a non-empty, relative, non-escaping guest path",
        ));
    }
    Ok(())
}

pub(super) fn validate_guest_file_path(path: &str) -> Result<(), ContractError> {
    // File read/write targets a guest-absolute or guest-relative path, but must
    // not look like a host path (drive prefix, backslash) or escape with `..`.
    if path.is_empty()
        || path.len() > MAX_GUEST_PATH_BYTES
        || path.as_bytes().contains(&0)
        || path.starts_with('\\')
        || path.contains('\\')
        || (path.len() >= 2 && path.as_bytes()[1] == b':')
        || path.split('/').any(|component| component == "..")
    {
        return Err(ContractError::InvalidPath(
            "must be a non-empty, non-escaping guest path",
        ));
    }
    Ok(())
}

pub(super) fn validate_guest_cwd(cwd: &str) -> Result<(), ContractError> {
    validate_guest_file_path(cwd)
}

pub(super) fn validate_limit(value: usize, max: usize) -> Result<(), ContractError> {
    if value == 0 || value > max {
        Err(ContractError::LimitExceeded)
    } else {
        Ok(())
    }
}

pub(super) fn validate_payload_size(value: usize, max: usize) -> Result<(), ContractError> {
    if value > max {
        Err(ContractError::LimitExceeded)
    } else {
        Ok(())
    }
}

pub(super) fn validate_pattern(pattern: &str) -> Result<(), ContractError> {
    if pattern.is_empty() || pattern.as_bytes().contains(&0) {
        Err(ContractError::InvalidPattern)
    } else if pattern.len() > MAX_GUEST_PATTERN_BYTES {
        Err(ContractError::LimitExceeded)
    } else {
        Ok(())
    }
}

pub(super) fn validate_cancel_id(id: &str) -> Result<(), ContractError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Err(ContractError::InvalidId)
    } else {
        Ok(())
    }
}
