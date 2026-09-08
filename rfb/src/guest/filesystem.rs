//! Structured filesystem DTOs: `ls`, `find`, and `grep`.

use super::limits::{
    default_guest_path, default_guest_result_bytes, default_guest_results, validate_fs_path,
    validate_limit, validate_pattern, MAX_GUEST_RESULTS, MAX_GUEST_RESULT_BYTES,
};
use crate::ContractError;
use serde::{Deserialize, Serialize};

/// A single directory entry returned by [`crate::Sandbox::ls`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirEntry {
    /// Entry name.
    pub name: String,
    /// Whether the entry is a directory.
    #[serde(default)]
    pub is_dir: bool,
    /// Entry size in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Structured `ls` request against a guest directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LsRequest {
    /// Directory path inside the guest.
    #[serde(default = "default_guest_path")]
    pub path: String,
    /// Maximum number of entries to return.
    #[serde(default = "default_guest_results")]
    pub max_results: usize,
}

impl LsRequest {
    /// Build an `ls` request for the given path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            max_results: MAX_GUEST_RESULTS,
        }
    }
    /// Validate the path and result cap.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_fs_path(&self.path)?;
        validate_limit(self.max_results, MAX_GUEST_RESULTS)?;
        Ok(())
    }
}

impl Default for LsRequest {
    fn default() -> Self {
        Self::new(default_guest_path())
    }
}

/// Result of a structured `ls` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LsResult {
    /// Directory entries.
    #[serde(default)]
    pub entries: Vec<DirEntry>,
    /// True when `max_results` truncated the listing.
    #[serde(default)]
    pub truncated: bool,
}

/// Structured `find` request within a guest directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindRequest {
    /// Root path to search inside the guest.
    #[serde(default = "default_guest_path")]
    pub path: String,
    /// Name pattern to match.
    pub pattern: String,
    /// Maximum number of matches to return.
    #[serde(default = "default_guest_results")]
    pub max_results: usize,
}

impl FindRequest {
    /// Build a `find` request for the given path and pattern.
    pub fn new(path: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            pattern: pattern.into(),
            max_results: MAX_GUEST_RESULTS,
        }
    }
    /// Validate path, pattern, and result cap.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_fs_path(&self.path)?;
        validate_pattern(&self.pattern)?;
        validate_limit(self.max_results, MAX_GUEST_RESULTS)?;
        Ok(())
    }
}

impl Default for FindRequest {
    fn default() -> Self {
        Self::new(default_guest_path(), String::new())
    }
}

/// Result of a structured `find` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindResult {
    /// Matching paths.
    #[serde(default)]
    pub matches: Vec<String>,
    /// True when `max_results` truncated the matches.
    #[serde(default)]
    pub truncated: bool,
}

/// One `grep` match with optional location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepMatch {
    /// Path of the matching file.
    pub path: String,
    /// 1-based line number, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// 1-based column number, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
    /// Matching line text.
    pub text: String,
}

/// Structured `grep` request within a guest directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepRequest {
    /// Root path to search inside the guest.
    #[serde(default = "default_guest_path")]
    pub path: String,
    /// Pattern to match against file contents.
    pub pattern: String,
    /// Maximum number of matches to return.
    #[serde(default = "default_guest_results")]
    pub max_results: usize,
    /// Maximum total bytes of matching content to return.
    #[serde(default = "default_guest_result_bytes")]
    pub max_bytes: usize,
}

impl GrepRequest {
    /// Build a `grep` request for the given path and pattern.
    pub fn new(path: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            pattern: pattern.into(),
            max_results: MAX_GUEST_RESULTS,
            max_bytes: MAX_GUEST_RESULT_BYTES,
        }
    }
    /// Validate path, pattern, and both caps.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_fs_path(&self.path)?;
        validate_pattern(&self.pattern)?;
        validate_limit(self.max_results, MAX_GUEST_RESULTS)?;
        validate_limit(self.max_bytes, MAX_GUEST_RESULT_BYTES)?;
        Ok(())
    }
}

impl Default for GrepRequest {
    fn default() -> Self {
        Self::new(default_guest_path(), String::new())
    }
}

/// Result of a structured `grep` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepResult {
    /// Matching lines.
    #[serde(default)]
    pub matches: Vec<GrepMatch>,
    /// True when a cap truncated the matches.
    #[serde(default)]
    pub truncated: bool,
}
