use rfb::guest::{CancelRequest, CancelResult, EvalRequest, EvalResult, MAX_GUEST_CODE_BYTES};
use rfb::ContractError;
use serde_json::json;
use std::time::Duration;

#[test]
fn cancel_request_constructors_defaults_and_validation() {
    assert_eq!(CancelRequest::new(), CancelRequest { id: None });
    assert_eq!(
        CancelRequest::with_id("op_123"),
        CancelRequest {
            id: Some("op_123".into())
        }
    );
    assert!(CancelRequest::new().validate().is_ok());
    for id in ["", "bad/id", "bad id", "bad.id", "bad\\id", "é"] {
        assert_eq!(
            CancelRequest::with_id(id).validate(),
            Err(ContractError::InvalidId)
        );
    }
    assert!(CancelRequest::with_id("x".repeat(128)).validate().is_ok());
    assert_eq!(
        CancelRequest::with_id("x".repeat(129)).validate(),
        Err(ContractError::InvalidId)
    );
}

#[test]
fn cancel_request_serialization_is_optional_and_strict() {
    let broad = serde_json::to_value(CancelRequest::new()).unwrap();
    assert_eq!(broad, json!({}));
    assert_eq!(
        serde_json::from_value::<CancelRequest>(json!({})).unwrap(),
        CancelRequest::new()
    );
    let targeted = CancelRequest::with_id("abc-1");
    assert_eq!(
        serde_json::to_value(&targeted).unwrap(),
        json!({"id": "abc-1"})
    );
    assert_eq!(
        serde_json::from_value::<CancelRequest>(json!({"id":"abc-1"})).unwrap(),
        targeted
    );
    assert!(serde_json::from_value::<CancelRequest>(json!({"id": null})).is_ok());
    assert!(serde_json::from_value::<CancelRequest>(json!({"unknown": true})).is_err());
    assert!(serde_json::from_value::<CancelRequest>(json!({"id": 1})).is_err());
}

#[test]
fn cancel_result_round_trips_and_rejects_unknown_fields() {
    for cancelled in [false, true] {
        let result = CancelResult { cancelled };
        let encoded = serde_json::to_value(result).unwrap();
        assert_eq!(encoded, json!({"cancelled": cancelled}));
        assert_eq!(
            serde_json::from_value::<CancelResult>(encoded).unwrap(),
            result
        );
    }
    assert!(serde_json::from_value::<CancelResult>(json!({})).is_err());
    assert!(serde_json::from_value::<CancelResult>(json!({"cancelled":true,"x":1})).is_err());
}

#[test]
fn eval_request_constructor_default_and_validation_errors() {
    assert_eq!(
        EvalRequest::new("code"),
        EvalRequest {
            cwd: None,
            code: "code".into(),
            timeout: None
        }
    );
    assert_eq!(EvalRequest::default(), EvalRequest::new(""));
    for code in ["", " ", "\n\t"] {
        assert_eq!(
            EvalRequest::new(code).validate(),
            Err(ContractError::EmptyCode)
        );
    }
    let mut too_large = EvalRequest::new("x".repeat(MAX_GUEST_CODE_BYTES + 1));
    assert_eq!(too_large.validate(), Err(ContractError::LimitExceeded));
    too_large.code = "x".repeat(MAX_GUEST_CODE_BYTES);
    assert!(too_large.validate().is_ok());

    let mut zero_timeout = EvalRequest::new("x");
    zero_timeout.timeout = Some(Duration::ZERO);
    assert_eq!(zero_timeout.validate(), Err(ContractError::InvalidTimeout));
    zero_timeout.timeout = Some(Duration::from_millis(1));
    assert!(zero_timeout.validate().is_ok());

    let mut bad_cwd = EvalRequest::new("x");
    for cwd in ["", "../escape", "C:/host", "a\\b", "/\\"] {
        bad_cwd.cwd = Some(cwd.into());
        assert!(
            matches!(bad_cwd.validate(), Err(ContractError::InvalidPath(_))),
            "cwd {cwd:?}"
        );
    }
    bad_cwd.cwd = Some("/workspace/app".into());
    assert!(bad_cwd.validate().is_ok());
}

#[test]
fn eval_request_serialization_defaults_duration_and_strictness() {
    let request = EvalRequest {
        cwd: Some("/workspace".into()),
        code: "1+1".into(),
        timeout: Some(Duration::from_millis(2500)),
    };
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        encoded,
        json!({"cwd":"/workspace", "code":"1+1", "timeout":2500})
    );
    assert_eq!(
        serde_json::from_value::<EvalRequest>(encoded).unwrap(),
        request
    );

    let minimal = serde_json::from_value::<EvalRequest>(json!({"code":"x"})).unwrap();
    assert_eq!(minimal, EvalRequest::new("x"));
    assert_eq!(serde_json::to_value(minimal).unwrap(), json!({"code":"x"}));
    assert!(serde_json::from_value::<EvalRequest>(json!({"code":"x", "extra":1})).is_err());
    assert!(serde_json::from_value::<EvalRequest>(json!({"code":"x", "timeout":0})).is_ok());
    assert!(serde_json::from_value::<EvalRequest>(json!({"code":"x", "timeout":"bad"})).is_err());
}

#[test]
fn eval_result_defaults_and_round_trip_all_fields() {
    assert_eq!(
        EvalResult::default(),
        EvalResult {
            output: vec![],
            status: None,
            timed_out: false
        }
    );
    let result = EvalResult {
        output: vec![0, 1, 255],
        status: Some(-2),
        timed_out: true,
    };
    let encoded = serde_json::to_value(&result).unwrap();
    assert_eq!(
        encoded,
        json!({"output":[0,1,255], "status":-2, "timed_out":true})
    );
    assert_eq!(
        serde_json::from_value::<EvalResult>(encoded).unwrap(),
        result
    );
    assert_eq!(
        serde_json::from_value::<EvalResult>(json!({})).unwrap(),
        EvalResult::default()
    );
    assert!(serde_json::from_value::<EvalResult>(
        json!({"output":[],"status":null,"timed_out":false,"x":1})
    )
    .is_err());
    assert!(serde_json::from_value::<EvalResult>(json!({"output":[256]})).is_err());
}
