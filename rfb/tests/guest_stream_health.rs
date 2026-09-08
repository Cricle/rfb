use rfb::guest::{Health, StreamEvent, StreamSpec};
use rfb::ContractError;
use serde_json::json;
use std::time::Duration;

#[test]
fn stream_events_use_tagged_snake_case_dto_encoding() {
    let cases = [
        (StreamEvent::Started, json!({"kind": "started"})),
        (
            StreamEvent::Stdout {
                data: vec![0, 1, 255],
            },
            json!({"kind": "stdout", "data": [0, 1, 255]}),
        ),
        (
            StreamEvent::Stderr {
                data: b"oops".to_vec(),
            },
            json!({"kind": "stderr", "data": [111, 111, 112, 115]}),
        ),
        (
            StreamEvent::Exit { code: Some(17) },
            json!({"kind": "exit", "code": 17}),
        ),
        (
            StreamEvent::Exit { code: None },
            json!({"kind": "exit", "code": null}),
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<StreamEvent>(expected).unwrap(),
            event
        );
    }
}

#[test]
fn stream_spec_round_trips_duration_and_defaults_optional_fields() {
    let spec = StreamSpec {
        command: "python".into(),
        args: vec!["-c".into(), "print(1)".into()],
        cwd: Some("/workspace/app".into()),
        pty: Some(true),
        env: vec![("MODE".into(), "test".into())],
        timeout: Some(Duration::from_millis(1250)),
    };
    let encoded = serde_json::to_value(&spec).unwrap();
    assert_eq!(encoded["timeout"], json!(1250));
    assert_eq!(serde_json::from_value::<StreamSpec>(encoded).unwrap(), spec);

    let decoded: StreamSpec = serde_json::from_value(json!({"command": "echo"})).unwrap();
    assert_eq!(decoded, StreamSpec::new("echo"));
}

#[test]
fn stream_spec_rejects_unknown_fields() {
    let result = serde_json::from_value::<StreamSpec>(json!({
        "command": "echo",
        "unexpected": true
    }));
    assert!(result.is_err());
}

#[test]
fn stream_spec_validation_rejects_invalid_command_timeout_cwd_and_env() {
    let mut spec = StreamSpec::new("   ");
    assert_eq!(spec.validate(), Err(ContractError::EmptyCommand));

    spec = StreamSpec::new("echo");
    spec.timeout = Some(Duration::ZERO);
    assert_eq!(spec.validate(), Err(ContractError::InvalidTimeout));

    spec.timeout = None;
    spec.cwd = Some("../escape".into());
    assert!(matches!(
        spec.validate(),
        Err(ContractError::InvalidPath(_))
    ));

    spec.cwd = None;
    spec.env = vec![("BAD\0KEY".into(), "value".into())];
    assert_eq!(spec.validate(), Err(ContractError::InvalidEnv));
    spec.env = vec![(String::new(), "value".into())];
    assert_eq!(spec.validate(), Err(ContractError::InvalidEnv));
}

#[test]
fn health_constructors_and_duration_serialization_are_stable() {
    assert_eq!(
        Health::healthy(),
        Health {
            healthy: true,
            latency: None,
            message: None,
        }
    );
    assert_eq!(
        Health::unhealthy("guest unavailable"),
        Health {
            healthy: false,
            latency: None,
            message: Some("guest unavailable".into()),
        }
    );

    let health = Health {
        healthy: true,
        latency: Some(Duration::from_millis(42)),
        message: Some("ready".into()),
    };
    let encoded = serde_json::to_value(&health).unwrap();
    assert_eq!(
        encoded,
        json!({"healthy": true, "latency": 42, "message": "ready"})
    );
    assert_eq!(serde_json::from_value::<Health>(encoded).unwrap(), health);

    let minimal: Health = serde_json::from_value(json!({"healthy": false})).unwrap();
    assert_eq!(
        minimal,
        Health {
            healthy: false,
            latency: None,
            message: None,
        }
    );
}
