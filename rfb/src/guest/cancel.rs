//! Guest cancel-operation DTOs.

use super::limits::validate_cancel_id;
use crate::ContractError;
use serde::{Deserialize, Serialize};

/// Request to cancel an in-flight guest operation (`exec`/`stream`/`eval`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    /// Optional identifier of the operation to cancel. `None` cancels broadly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl CancelRequest {
    /// Build an empty cancel request that cancels broadly.
    pub fn new() -> Self {
        Self::default()
    }
    /// Build a cancel request targeting a specific operation id.
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
        }
    }
    /// Validate the optional cancel id.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate(&self) -> Result<(), ContractError> {
        if let Some(id) = &self.id {
            validate_cancel_id(id)?;
        }
        Ok(())
    }
}

/// Result of a cancel request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelResult {
    /// Whether an in-flight operation was cancelled.
    pub cancelled: bool,
}
