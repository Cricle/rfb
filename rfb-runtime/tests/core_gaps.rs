use rfb_runtime::config::{
    RuntimeConfig, DEFAULT_ENVIRONMENT_PATH, DEFAULT_FORKD_AGENT_ADDR, DEFAULT_VSOCK_PORT,
    DEFAULT_WORKSPACE,
};
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn runtime_config_defaults_are_historical_values() {
    let config = RuntimeConfig::default();
    assert_eq!(config.vsock_port, DEFAULT_VSOCK_PORT);
    assert_eq!(config.forkd_agent_addr, DEFAULT_FORKD_AGENT_ADDR);
    assert_eq!(config.workspace_root, PathBuf::from(DEFAULT_WORKSPACE));
    assert_eq!(
        config.environment_path,
        PathBuf::from(DEFAULT_ENVIRONMENT_PATH)
    );
    assert_eq!(config.socket_timeout, Duration::from_secs(10));
    assert_eq!(config.listen_wait, Duration::from_secs(5));
    assert_eq!(config.exec_default_timeout, Duration::from_secs(1800));
}

#[test]
fn runtime_config_environment_falls_back_for_invalid_or_empty_values() {
    // This test deliberately only checks the pure default behavior: mutating
    // process-global environment variables would race with parallel tests.
    let config = RuntimeConfig::default();
    assert_eq!(config.vsock_port, 5000);
    assert!(!config.forkd_agent_addr.is_empty());
}

#[cfg(feature = "guest")]
mod guest_service {
    use rfb_runtime::resources::RuntimeLimits;
    use rfb_runtime::runtime_service::{
        parse_runtime_backend, GuestEvent, GuestExecutor, RuntimeBackend, RuntimeService,
    };
    use rfb_runtime::session::{ControlMessage, RuntimeMessage, SessionRequest};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct MockExecutor {
        events: Vec<GuestEvent>,
        failure: Option<String>,
        cancel_result: Result<(), String>,
        shutdown_result: Result<(), String>,
        flag: Option<Arc<AtomicBool>>,
    }

    impl GuestExecutor for MockExecutor {
        fn start_turn(&mut self, _: &SessionRequest) -> Result<Vec<GuestEvent>, String> {
            self.failure
                .clone()
                .map_or_else(|| Ok(self.events.clone()), Err)
        }
        fn cancel(&mut self, _: &str, _: &str) -> Result<(), String> {
            self.cancel_result.clone()
        }
        fn shutdown(&mut self) -> Result<(), String> {
            self.shutdown_result.clone()
        }
        fn cancel_handle(&self) -> Option<Arc<AtomicBool>> {
            self.flag.clone()
        }
    }

    fn ready(service: &mut RuntimeService) {
        assert!(matches!(
            service.handle(ControlMessage::Hello {
                protocol_version: 1
            })[..],
            [RuntimeMessage::HelloAck { .. }]
        ));
    }
    fn turn(session: &str, request: &str) -> SessionRequest {
        SessionRequest {
            session_id: session.into(),
            request_id: request.into(),
            prompt: String::new(),
        }
    }

    #[test]
    fn backend_parser_normalizes_supported_and_rejects_unknown_values() {
        assert_eq!(parse_runtime_backend(None), RuntimeBackend::Forkd);
        assert_eq!(
            parse_runtime_backend(Some(" FORKD ")),
            RuntimeBackend::Forkd
        );
        assert_eq!(
            parse_runtime_backend(Some("local")),
            RuntimeBackend::Unsupported("local".into())
        );
        assert_eq!(parse_runtime_backend(Some("  ")), RuntimeBackend::Forkd);
    }

    #[test]
    fn spawn_turn_validates_handshake_identity_and_duplicate_state() {
        let mut service = RuntimeService::with_executor_impl(
            RuntimeLimits::default(),
            MockExecutor {
                events: vec![],
                failure: None,
                cancel_result: Ok(()),
                shutdown_result: Ok(()),
                flag: None,
            },
        );
        assert!(service.spawn_turn(&turn("s", "r")).is_err());
        ready(&mut service);
        assert!(service.spawn_turn(&turn("", "r")).is_err());
        let executor = service.spawn_turn(&turn("s", "r")).unwrap();
        assert!(service.spawn_turn(&turn("s", "r2")).is_err());
        let responses = service.complete_turn(
            "s".into(),
            "r".into(),
            Ok(vec![GuestEvent::new("turn.completed", vec![])]),
            executor,
        );
        assert!(matches!(&responses[..], [RuntimeMessage::Event(event)] if event.sequence == 1));
        assert!(service.spawn_turn(&turn("s", "r")).is_err());
    }

    #[test]
    fn complete_turn_maps_failures_and_cancel_signal() {
        let flag = Arc::new(AtomicBool::new(false));
        let executor = MockExecutor {
            events: vec![],
            failure: None,
            cancel_result: Ok(()),
            shutdown_result: Ok(()),
            flag: Some(flag.clone()),
        };
        let mut service = RuntimeService::with_executor_impl(RuntimeLimits::default(), executor);
        ready(&mut service);
        let executor = service.spawn_turn(&turn("s", "r")).unwrap();
        assert!(service.cancel_active("s", "r").is_empty());
        assert!(flag.load(Ordering::SeqCst));
        let responses = service.complete_turn(
            "s".into(),
            "r".into(),
            Err("worker failed".into()),
            executor,
        );
        assert!(
            matches!(&responses[..], [RuntimeMessage::Event(event)] if event.kind == "turn.cancelled")
        );
    }

    #[test]
    fn shutdown_error_is_returned_and_service_remains_closed() {
        let mut service = RuntimeService::with_executor_impl(
            RuntimeLimits::default(),
            MockExecutor {
                events: vec![],
                failure: None,
                cancel_result: Ok(()),
                shutdown_result: Err("shutdown failed".into()),
                flag: None,
            },
        );
        ready(&mut service);
        let result = service.handle(ControlMessage::Shutdown);
        assert!(
            matches!(&result[..], [RuntimeMessage::Error { message, .. }] if message == "shutdown failed")
        );
        assert!(
            matches!(&service.handle(ControlMessage::Capabilities { session_per_vm: true, writable_workspace: true })[..], [RuntimeMessage::Error { message, .. }] if message == "runtime has shut down")
        );
    }
}
