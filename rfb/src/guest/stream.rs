//! Interactive guest stream events, request, and the object-safe stream trait.

use super::limits::validate_guest_cwd;
use crate::{core::duration_millis, BoxFuture, ContractError, SandboxError};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One event emitted by an interactive guest stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StreamEvent {
    /// The guest acknowledged the stream and is ready to accept input.
    Started,
    /// A chunk of standard output.
    Stdout {
        /// Standard output bytes.
        data: Vec<u8>,
    },
    /// A chunk of standard error.
    Stderr {
        /// Standard error bytes.
        data: Vec<u8>,
    },
    /// The stream terminated. `code` is the exit code when the guest reported one.
    Exit {
        /// Exit code when the guest reported one.
        code: Option<i32>,
    },
}

/// Request to open an interactive, bidirectional guest stream.
///
/// `cwd` is an opaque guest path and is never interpreted as a host path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSpec {
    /// Command to run in the guest.
    pub command: String,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory inside the guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Request a pseudo-terminal when `Some(true)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<bool>,
    /// Ordered environment pairs, provided as typed tuples (never JSON `Value`).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Optional timeout; zero is rejected by validation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration_millis"
    )]
    pub timeout: Option<Duration>,
}

impl StreamSpec {
    /// Build a stream spec for the given command.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            pty: None,
            env: Vec::new(),
            timeout: None,
        }
    }
    /// Validate command, timeout, working directory, and environment keys.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.command.trim().is_empty() {
            return Err(ContractError::EmptyCommand);
        }
        if self.timeout == Some(Duration::ZERO) {
            return Err(ContractError::InvalidTimeout);
        }
        if let Some(cwd) = &self.cwd {
            validate_guest_cwd(cwd)?;
        }
        for (name, _value) in &self.env {
            if name.is_empty() || name.as_bytes().contains(&0) {
                return Err(ContractError::InvalidEnv);
            }
        }
        Ok(())
    }
}

impl Default for StreamSpec {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// A bidirectional, object-safe guest stream.
///
/// Implementations are typically backed by a single long-lived connection and
/// must be safe to hold while events are consumed and input is written.
pub trait GuestStream: Send {
    /// Read the next protocol event. `None` means the connection closed cleanly.
    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<StreamEvent>, SandboxError>>;
    /// Send a line of input to the running stream.
    fn send_input<'a>(&'a mut self, input: String) -> BoxFuture<'a, Result<(), SandboxError>>;
    /// Request that the stream terminate. Idempotent.
    fn stop<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SandboxError>>;
}
