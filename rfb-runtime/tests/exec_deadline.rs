//! Exec-deadline resolution contract (integration tests; moved out of
//! src/ per the check-tests-folder boundary).
#![cfg(feature = "forkd")]

mod exec_deadline_tests {
    use rfb_runtime::agent::{effective_exec_timeout, DEFAULT_EXEC_TIMEOUT};
    use std::time::Duration;

    use serde_json::json;

    #[test]
    fn missing_timeout_gets_the_default_deadline() {
        let request = json!({"action":"exec","args":["/bin/true"],"cwd":"."});
        assert_eq!(
            effective_exec_timeout(&request),
            DEFAULT_EXEC_TIMEOUT,
            "a request without `timeout` must never wait forever"
        );
    }

    #[test]
    fn explicit_timeout_wins_over_the_default() {
        let request = json!({"action":"exec","args":["/bin/sleep","5"],"timeout":2});
        assert_eq!(effective_exec_timeout(&request), Duration::from_secs(2));
    }

    #[test]
    fn explicit_zero_keeps_immediate_expiry_contract() {
        let request = json!({"action":"exec","args":["/bin/true"],"timeout":0});
        assert_eq!(effective_exec_timeout(&request), Duration::ZERO);
    }

    #[test]
    fn non_numeric_timeout_falls_back_to_the_default() {
        // A non-numeric `timeout` was historically ignored (wait forever);
        // ignoring it now must fall back to the default deadline, not remove
        // the bound.
        let request = json!({"action":"exec","args":["/bin/true"],"timeout":"soon"});
        assert_eq!(effective_exec_timeout(&request), DEFAULT_EXEC_TIMEOUT);
    }

    #[test]
    fn default_deadline_is_client_patience_scale() {
        // The default exists so an abandoned exec is reaped around the moment
        // its client's read timeout would have fired (host clients read with a
        // 10 s base timeout, SDKs 10-60 s). It must stay on that scale — not
        // the executor's 1800 s max runtime, and not an unbounded wait.
        assert!(DEFAULT_EXEC_TIMEOUT >= Duration::from_secs(10));
        assert!(DEFAULT_EXEC_TIMEOUT <= Duration::from_secs(60));
    }
}
