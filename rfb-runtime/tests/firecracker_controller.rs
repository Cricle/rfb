#![cfg(target_os = "linux")]

use rfb_runtime::firecracker_core::firecracker::{
    parse_api_response, read_http_response, readiness_error,
};
use std::io::Cursor;
use std::time::Duration;

#[test]
fn reads_http_response_by_content_length() {
    let mut stream =
        Cursor::new(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\ntrailing".to_vec());
    let response = read_http_response(&mut stream).unwrap();
    assert_eq!(
        response,
        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
    );
}

#[test]
fn rejects_unsupported_transfer_encoding_and_invalid_content_length() {
    for response in [
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nxx".as_slice(),
    ] {
        assert!(read_http_response(&mut Cursor::new(response.to_vec())).is_err());
    }
}

#[test]
fn rejects_non_utf8_and_truncated_http_body() {
    assert!(read_http_response(&mut Cursor::new(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".to_vec()
    ))
    .is_err());
    assert!(read_http_response(&mut Cursor::new(
        b"NOT-HTTP\r\nContent-Length: 0\r\n\r\n".to_vec()
    ))
    .is_err());
    assert!(read_http_response(&mut Cursor::new(
        b"HTTP/1.1 200 OK\r\nX-Name: \xff\r\n\r\n".to_vec()
    ))
    .is_err());
}

#[test]
fn rejects_oversized_http_headers_before_body_limit() {
    let mut response = b"HTTP/1.1 200 OK\r\nX-Large: ".to_vec();
    response.extend(std::iter::repeat_n(b'x', 32 * 1024));
    let error = read_http_response(&mut Cursor::new(response)).unwrap_err();
    assert!(error.to_string().contains("headers exceed limit"));
}

#[test]
fn parses_success_and_rejects_bad_http_status() {
    let ok = parse_api_response(
        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        "PUT",
        "/machine-config",
    )
    .unwrap();
    assert!(ok.starts_with("HTTP/1.1 204"));
    let error = parse_api_response(
        b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n",
        "PUT",
        "/actions",
    )
    .unwrap_err();
    assert!(error.to_string().contains("422"));
}

#[test]
fn readiness_distinguishes_child_exit_and_timeout() {
    let status = std::process::Command::new("sh")
        .args(["-c", "exit 7"])
        .status()
        .unwrap();
    assert_eq!(
        readiness_error(Duration::from_millis(1), Some(status)),
        Some("Firecracker exited before API became ready")
    );
    assert_eq!(
        readiness_error(Duration::from_secs(6), None),
        Some("Firecracker API socket did not become ready")
    );
}
