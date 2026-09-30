//! Accept-failure classifier contract (integration tests; moved out of
//! src/ per the check-tests-folder boundary).
#![cfg(feature = "zeroboot")]

mod accept_failure_tests {
    use rfb_runtime::zeroboot_guest::{accept_failure_action, AcceptFailure, ACCEPT_FAILURE_LIMIT};
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
