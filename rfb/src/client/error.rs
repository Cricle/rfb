//! Unified SDK error hierarchy (`sdk/UNIFIED_API.md` §7): transport, HTTP
//! status, decode, remote, and validation failures.

use std::io;

/// Errors raised by the unified RFB client facade. The five variants mirror
/// the five error classes shared by all four language SDKs.
#[derive(Debug, thiserror::Error)]
pub enum RfbError {
    /// Connection, read/write, or timeout failure.
    #[error("transport failure: {0}")]
    Transport(#[from] io::Error),
    /// The forkd controller returned a non-2xx HTTP status.
    #[error("http status {status}: {message}")]
    Http {
        /// HTTP status code returned by the controller.
        status: u16,
        /// Error message derived from the controller response body.
        message: String,
    },
    /// A response or frame could not be decoded (including strict codec
    /// rejections).
    #[error("decode failure: {0}")]
    Decode(String),
    /// The peer reported an error (guest `error` line, ZBRT `Error` frame,
    /// forkd `error` field).
    #[error("remote error: {0}")]
    Remote(String),
    /// Local validation failed before the request was sent (fail closed).
    #[error("validation failure: {0}")]
    Validation(String),
}

impl From<crate::controller::ForkdClientError> for RfbError {
    fn from(error: crate::controller::ForkdClientError) -> Self {
        match error {
            crate::controller::ForkdClientError::Transport(err) => {
                RfbError::Transport(io::Error::other(err))
            }
            crate::controller::ForkdClientError::Http { status, message } => RfbError::Http {
                status: status.as_u16(),
                message,
            },
            crate::controller::ForkdClientError::Decode(message) => RfbError::Decode(message),
        }
    }
}

impl From<crate::forkd_guest::ForkdGuestError> for RfbError {
    fn from(error: crate::forkd_guest::ForkdGuestError) -> Self {
        match error {
            crate::forkd_guest::ForkdGuestError::Io(err) => RfbError::Transport(err),
            crate::forkd_guest::ForkdGuestError::TooLarge => {
                RfbError::Decode("guest response exceeded the 1 MiB line limit".to_owned())
            }
            crate::forkd_guest::ForkdGuestError::Json(err) => RfbError::Decode(err.to_string()),
            crate::forkd_guest::ForkdGuestError::Remote(message) => RfbError::Remote(message),
            crate::forkd_guest::ForkdGuestError::InvalidPath => {
                RfbError::Validation("invalid guest path".to_owned())
            }
            crate::forkd_guest::ForkdGuestError::LimitExceeded => {
                RfbError::Validation("guest result limit exceeded".to_owned())
            }
            crate::forkd_guest::ForkdGuestError::UnsupportedGuestRpc(tool) => {
                RfbError::Remote(format!("unsupported guest RPC: {tool}"))
            }
        }
    }
}

impl From<crate::ContractError> for RfbError {
    fn from(error: crate::ContractError) -> Self {
        RfbError::Validation(error.to_string())
    }
}

/// Build a transport timeout error with a stable message.
pub(super) fn transport_timeout(what: &'static str) -> RfbError {
    RfbError::Transport(io::Error::new(io::ErrorKind::TimedOut, what))
}
