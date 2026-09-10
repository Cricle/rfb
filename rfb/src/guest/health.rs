//! Guest liveness/health reported by [`crate::Sandbox::ping`].

use crate::core::duration_millis;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Guest liveness/health reported by [`crate::Sandbox::ping`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// Whether the guest responded and is considered healthy.
    pub healthy: bool,
    /// Round-trip latency, when the backend can report it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration_millis"
    )]
    pub latency: Option<Duration>,
    /// Optional human-readable detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Health {
    /// Build a healthy report.
    pub fn healthy() -> Self {
        Self {
            healthy: true,
            latency: None,
            message: None,
        }
    }
    /// Build an unhealthy report carrying a reason.
    pub fn unhealthy(message: impl Into<String>) -> Self {
        Self {
            healthy: false,
            latency: None,
            message: Some(message.into()),
        }
    }
}
