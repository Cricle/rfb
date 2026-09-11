use rfb::guest::{ReadRequest, WriteRequest, MAX_GUEST_PATH_BYTES, MAX_GUEST_RESULT_BYTES};
use rfb::ContractError;

fn assert_invalid_path(result: Result<(), ContractError>) {
    assert!(matches!(result, Err(ContractError::InvalidPath(_))));
}

#[test]
fn read_rejects_every_invalid_guest_file_path_form() {
    for path in [
        "",
        "\\absolute",
        "dir\\file",
        "C:/file",
        "C:\\file",
        "dir/../file",
        "..",
        "dir/..",
    ] {
        assert_invalid_path(ReadRequest::new(path).validate());
    }
    assert_invalid_path(ReadRequest::new("nul\0byte").validate());
    assert_invalid_path(ReadRequest::new("x".repeat(MAX_GUEST_PATH_BYTES + 1)).validate());
}

#[test]
fn write_rejects_every_invalid_guest_file_path_form() {
    for path in [
        "",
        "\\absolute",
        "dir\\file",
        "C:/file",
        "C:\\file",
        "dir/../file",
        "..",
        "dir/..",
    ] {
        assert_invalid_path(WriteRequest::new(path, Vec::<u8>::new()).validate());
    }
    assert_invalid_path(WriteRequest::new("nul\0byte", Vec::<u8>::new()).validate());
    assert_invalid_path(
        WriteRequest::new("x".repeat(MAX_GUEST_PATH_BYTES + 1), Vec::<u8>::new()).validate(),
    );
}

#[test]
fn read_limit_rejects_zero_and_values_above_max() {
    let mut request = ReadRequest::new("/workspace/file");
    request.max_bytes = Some(0);
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
    request.max_bytes = Some(MAX_GUEST_RESULT_BYTES + 1);
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
    request.max_bytes = Some(MAX_GUEST_RESULT_BYTES);
    assert!(request.validate().is_ok());
}

#[test]
fn write_payload_limit_rejects_values_above_max_but_allows_empty() {
    let request = WriteRequest::new("/workspace/file", vec![0; MAX_GUEST_RESULT_BYTES + 1]);
    assert!(matches!(
        request.validate(),
        Err(ContractError::LimitExceeded)
    ));
    assert!(WriteRequest::new("/workspace/file", Vec::<u8>::new())
        .validate()
        .is_ok());
    assert!(
        WriteRequest::new("/workspace/file", vec![0; MAX_GUEST_RESULT_BYTES])
            .validate()
            .is_ok()
    );
}

#[test]
fn io_requests_accept_valid_guest_paths_and_boundary_limits() {
    assert!(ReadRequest::new("relative/path").validate().is_ok());
    assert!(ReadRequest::new("/workspace/file").validate().is_ok());
    assert!(WriteRequest::new("/workspace/file", b"data".to_vec())
        .validate()
        .is_ok());
}
