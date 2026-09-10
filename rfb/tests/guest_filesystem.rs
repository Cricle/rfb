//! Boundary and serialization tests for guest filesystem DTOs.

use rfb::guest::{
    DirEntry, FindRequest, FindResult, GrepMatch, GrepRequest, GrepResult, LsRequest, LsResult,
    ReadRequest, ReadResult, WriteRequest, WriteResult, MAX_GUEST_PATTERN_BYTES, MAX_GUEST_RESULTS,
    MAX_GUEST_RESULT_BYTES,
};
use rfb::ContractError;

#[test]
fn read_accepts_guest_paths_and_optional_bounds() {
    let mut request = ReadRequest::new("/workspace/data.bin");
    request.offset = Some(u64::MAX);
    request.max_bytes = Some(MAX_GUEST_RESULT_BYTES);
    assert!(request.validate().is_ok());
    assert_eq!(ReadRequest::new("file").offset, None);
}

#[test]
fn read_rejects_invalid_paths_and_zero_or_oversized_cap() {
    for path in [
        "",
        "C:/file",
        "C:\\file",
        "../file",
        "a/../file",
        "\\file",
        "a\0b",
    ] {
        assert!(ReadRequest::new(path).validate().is_err(), "{path:?}");
    }
    let mut request = ReadRequest::new("file");
    for cap in [0, MAX_GUEST_RESULT_BYTES + 1] {
        request.max_bytes = Some(cap);
        assert!(matches!(
            request.validate(),
            Err(ContractError::LimitExceeded)
        ));
    }
}

#[test]
fn read_round_trip_preserves_empty_and_metadata() {
    let cases = [
        ReadResult::default(),
        ReadResult {
            data: vec![0, 1, 255],
            truncated: true,
            total_bytes: Some(u64::MAX),
        },
    ];
    for value in cases {
        let encoded = serde_json::to_value(&value).unwrap();
        assert_eq!(
            serde_json::from_value::<ReadResult>(encoded).unwrap(),
            value
        );
    }
}

#[test]
fn write_accepts_empty_and_maximum_payload() {
    assert!(WriteRequest::new("new.bin", Vec::<u8>::new())
        .validate()
        .is_ok());
    let mut request = WriteRequest::new("new.bin", vec![7; MAX_GUEST_RESULT_BYTES]);
    request.append = true;
    request.mode = Some(0o640);
    assert!(request.validate().is_ok());
}

#[test]
fn write_rejects_invalid_paths_and_oversized_payload() {
    for path in [
        "",
        "C:/file",
        "C:\\file",
        "../file",
        "a/../file",
        "\\file",
        "a\0b",
    ] {
        assert!(
            WriteRequest::new(path, vec![]).validate().is_err(),
            "{path:?}"
        );
    }
    let request = WriteRequest::new("file", vec![0; MAX_GUEST_RESULT_BYTES + 1]);
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
}

#[test]
fn write_round_trip_preserves_flags_and_binary_data() {
    let mut value = WriteRequest::new("out", vec![0, 10, 255]);
    value.append = true;
    value.mode = Some(u32::MAX);
    let encoded = serde_json::to_value(&value).unwrap();
    assert_eq!(
        serde_json::from_value::<WriteRequest>(encoded).unwrap(),
        value
    );
    let result = WriteResult {
        bytes_written: u64::MAX,
    };
    assert_eq!(
        serde_json::from_value::<WriteResult>(serde_json::to_value(result).unwrap()).unwrap(),
        result
    );
}

#[test]
fn ls_defaults_accept_workspace_and_enforce_result_limit() {
    let request = LsRequest::default();
    assert_eq!(request.path, ".");
    assert_eq!(request.max_results, MAX_GUEST_RESULTS);
    assert!(request.validate().is_ok());
    assert!(LsRequest::new("/workspace").validate().is_ok());
    let mut request = LsRequest::new(".");
    for limit in [0, MAX_GUEST_RESULTS + 1] {
        request.max_results = limit;
        assert!(matches!(
            request.validate(),
            Err(ContractError::LimitExceeded)
        ));
    }
}

#[test]
fn ls_result_round_trip_and_defaults() {
    let value = LsResult {
        entries: vec![DirEntry {
            name: "x".into(),
            is_dir: false,
            size: Some(0),
        }],
        truncated: true,
    };
    assert_eq!(
        serde_json::from_value::<LsResult>(serde_json::to_value(&value).unwrap()).unwrap(),
        value
    );
    assert_eq!(
        serde_json::from_value::<LsResult>(serde_json::json!({})).unwrap(),
        LsResult::default()
    );
}

#[test]
fn find_validates_path_pattern_and_result_edges() {
    assert!(FindRequest::new("src", "*.rs").validate().is_ok());
    for pattern in ["", "a\0b"] {
        assert!(matches!(
            FindRequest::new(".", pattern).validate(),
            Err(ContractError::InvalidPattern)
        ));
    }
    let mut request = FindRequest::new(".", "x");
    request.pattern = "x".repeat(MAX_GUEST_PATTERN_BYTES + 1);
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
    for limit in [0, MAX_GUEST_RESULTS + 1] {
        request.pattern = "x".into();
        request.max_results = limit;
        assert!(matches!(
            request.validate(),
            Err(ContractError::LimitExceeded)
        ));
    }
}

#[test]
fn find_result_round_trip_preserves_matches_and_truncation() {
    let value = FindResult {
        matches: vec!["a", "a/b"].into_iter().map(str::to_owned).collect(),
        truncated: true,
    };
    assert_eq!(
        serde_json::from_value::<FindResult>(serde_json::to_value(&value).unwrap()).unwrap(),
        value
    );
}

#[test]
fn grep_validates_both_limits_and_round_trips_locations() {
    let mut request = GrepRequest::new(".", "needle");
    assert!(request.validate().is_ok());
    request.max_results = 0;
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
    request.max_results = MAX_GUEST_RESULTS;
    request.max_bytes = 0;
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
    request.max_bytes = MAX_GUEST_RESULT_BYTES + 1;
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
    request.max_bytes = MAX_GUEST_RESULT_BYTES;
    request.pattern = "x".repeat(MAX_GUEST_PATTERN_BYTES + 1);
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));

    let value = GrepResult {
        matches: vec![GrepMatch {
            path: "a.txt".into(),
            line: Some(1),
            column: Some(2),
            text: "needle".into(),
        }],
        truncated: false,
    };
    assert_eq!(
        serde_json::from_value::<GrepResult>(serde_json::to_value(&value).unwrap()).unwrap(),
        value
    );
}
