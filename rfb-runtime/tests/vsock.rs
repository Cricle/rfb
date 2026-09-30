#![cfg(target_os = "linux")]

use rfb_runtime::vsock::{validate_endpoint, DEFAULT_PORT};
use tokio_vsock::VMADDR_CID_ANY;

#[test]
fn validates_guest_vsock_endpoint() {
    assert!(validate_endpoint(VMADDR_CID_ANY, 0).is_err());
    assert!(validate_endpoint(52, DEFAULT_PORT - 1).is_err());
    assert!(validate_endpoint(52, DEFAULT_PORT).is_ok());
    assert!(validate_endpoint(52, DEFAULT_PORT + 1).is_err());
}

// The RFB1 guest accept loop shares the accept-failure classifier with the
// ZeroBoot guest listener (both live in [`rfb_runtime::vsock`]); the contract
// below mirrors tests/zeroboot_guest.rs so the shared implementation stays
// honest for both consumers.
mod accept_failure_tests {
    use rfb_runtime::vsock::{accept_failure_action, AcceptFailure, ACCEPT_FAILURE_LIMIT};
    use std::io;

    fn error(kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, "probe")
    }

    #[test]
    fn transient_accept_errors_retry_with_backoff() {
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::Interrupted,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Other,
        ] {
            assert!(
                matches!(accept_failure_action(&error(kind), 1), AcceptFailure::Retry),
                "{kind:?} must be retried"
            );
        }
    }

    #[test]
    fn config_class_accept_errors_give_up_immediately() {
        for kind in [io::ErrorKind::InvalidInput, io::ErrorKind::Unsupported] {
            assert!(
                matches!(
                    accept_failure_action(&error(kind), 0),
                    AcceptFailure::GiveUp
                ),
                "{kind:?} is never recoverable by retrying"
            );
        }
    }

    #[test]
    fn persistent_failure_streak_gives_up_at_the_limit() {
        // Just under the limit the loop still retries...
        assert!(matches!(
            accept_failure_action(&error(io::ErrorKind::Other), ACCEPT_FAILURE_LIMIT - 1),
            AcceptFailure::Retry
        ));
        // ...and exactly at the limit it gives up (no off-by-one: the count
        // includes the failure being classified).
        assert!(matches!(
            accept_failure_action(&error(io::ErrorKind::Other), ACCEPT_FAILURE_LIMIT),
            AcceptFailure::GiveUp
        ));
    }
}
