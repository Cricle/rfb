//! Guest code-evaluation DTOs.

use super::limits::{validate_guest_cwd, MAX_GUEST_CODE_BYTES};
use crate::{core::duration_millis, ContractError};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Request to evaluate a snippet of code in the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalRequest {
    /// Working directory inside the guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Code snippet to evaluate.
    pub code: String,
    /// Optional timeout; zero is rejected by validation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration_millis"
    )]
    pub timeout: Option<Duration>,
}

impl EvalRequest {
    /// Build an eval request for the given code.
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            cwd: None,
            code: code.into(),
            timeout: None,
        }
    }
    /// Validate code, timeout, and working directory.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.code.trim().is_empty() {
            return Err(ContractError::EmptyCode);
        }
        if self.code.len() > MAX_GUEST_CODE_BYTES {
            return Err(ContractError::LimitExceeded);
        }
        if self.timeout == Some(Duration::ZERO) {
            return Err(ContractError::InvalidTimeout);
        }
        if let Some(cwd) = &self.cwd {
            validate_guest_cwd(cwd)?;
        }
        Ok(())
    }
}

impl Default for EvalRequest {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// Result of a guest code evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalResult {
    /// Captured standard output of the evaluation.
    #[serde(default)]
    pub output: Vec<u8>,
    /// Exit status when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    /// Whether evaluation was killed by a timeout.
    #[serde(default)]
    pub timed_out: bool,
}
