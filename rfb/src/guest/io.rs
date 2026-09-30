//! Guest file read/write DTOs.

use super::limits::{validate_guest_file_path, validate_limit, validate_payload_size};
use crate::ContractError;
use serde::{Deserialize, Serialize};

/// Request to read a file inside the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    /// Path of the file to read inside the guest.
    pub path: String,
    /// Byte offset to start reading from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Maximum bytes to read. When unset the backend applies its own cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

impl ReadRequest {
    /// Build a read request for the given path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: None,
            max_bytes: None,
        }
    }
    /// Validate the path and, when set, the byte cap.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_guest_file_path(&self.path)?;
        if let Some(max_bytes) = self.max_bytes {
            validate_limit(max_bytes, super::limits::MAX_GUEST_RESULT_BYTES)?;
        }
        Ok(())
    }
}

impl Default for ReadRequest {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// Result of a guest file read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadResult {
    /// Bytes read from the file.
    #[serde(default)]
    pub data: Vec<u8>,
    /// True when the read was cut short by `max_bytes`/a backend cap.
    #[serde(default)]
    pub truncated: bool,
    /// Total file size when the backend knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

/// Request to write a file inside the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteRequest {
    /// Path of the file to write inside the guest.
    pub path: String,
    /// Bytes to write.
    #[serde(default)]
    pub data: Vec<u8>,
    /// Append to an existing file instead of replacing it.
    #[serde(default)]
    pub append: bool,
    /// Optional POSIX-style mode bits requested for a new file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

impl WriteRequest {
    /// Build a write request for the given path and data.
    pub fn new(path: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            data: data.into(),
            append: false,
            mode: None,
        }
    }
    /// Validate the path and payload size.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_guest_file_path(&self.path)?;
        validate_payload_size(self.data.len(), super::limits::MAX_GUEST_RESULT_BYTES)?;
        Ok(())
    }
}

impl Default for WriteRequest {
    fn default() -> Self {
        Self::new(String::new(), Vec::new())
    }
}

/// Result of a guest file write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteResult {
    /// Number of bytes written.
    pub bytes_written: u64,
}
