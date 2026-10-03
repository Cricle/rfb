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

// The remaining seams (api_request wire format, boot spawn failure, vsock
// device serialization, parse_response_head) exercise private items of the
// merged driver, so they paste the source via `include!` — the same seam the
// former `rfb/tests/zeroboot_firecracker.rs` used. Every one of its assertions
// is preserved below (migrated with the Firecracker dual-driver merge). The
// pasted file must keep its plain `//` header and only reference
// `crate::boot_args`, `crate::config` and `crate::firecracker` (shimmed at the
// bottom of this file); `mod socket;`/`mod snapshot;` resolve through the
// test-only re-export stubs in `tests/firecracker_impl/`.
#[allow(dead_code)]
mod firecracker_impl {
    include!("../src/firecracker_core/firecracker/mod.rs");

    mod tests {
        use super::socket::endpoint_identity;
        use super::*;
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::path::Path;
        use std::process::{Command, Stdio};
        use tempfile::tempdir;

        fn vm_for_socket(path: &str) -> FirecrackerVm {
            let child = Command::new("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("test helper process");
            FirecrackerVm {
                process: Some(child),
                socket_path: path.to_owned(),
                socket_identity: endpoint_identity(Path::new(path)).unwrap(),
                snapshot_dir: None,
                vsock_uds_path: None,
                vsock_identity: None,
                // 非 PDEATHSIG fork 的测试替身：无 holder，直接 None。
                holder: None,
            }
        }

        #[test]
        fn api_request_constructs_http_and_reports_success() {
            let dir = tempdir().unwrap();
            let socket = dir.path().join("api.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                // UnixStream has no half-close here: read exactly the HTTP
                // request, then reply so the client is not blocked waiting
                // for EOF while the server is blocked waiting for a reply.
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0, "request ended before HTTP headers/body");
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    let Some(header_end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let content_length = text
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                let text = String::from_utf8(request).unwrap();
                assert!(text.starts_with("PUT /machine-config HTTP/1.1\r\n"));
                assert!(text.contains("Content-Type: application/json\r\n"));
                assert!(text.contains("\r\n\r\n{\"mem_size_mib\":128,\"vcpu_count\":1}"));
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .unwrap();
            });
            let vm = vm_for_socket(socket.to_str().unwrap());
            assert!(vm
                .api_put(
                    "/machine-config",
                    &serde_json::json!({"vcpu_count": 1, "mem_size_mib": 128}),
                )
                .is_ok());
            server.join().unwrap();
        }

        #[test]
        fn api_request_rejects_non_success_response() {
            let dir = tempdir().unwrap();
            let socket = dir.path().join("error.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\ninvalid")
                    .unwrap();
            });
            let vm = vm_for_socket(socket.to_str().unwrap());
            let error = vm
                .api_put("/actions", &serde_json::json!({"action_type": "Bad"}))
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("Firecracker API error on PUT /actions"));
            server.join().unwrap();
        }

        #[test]
        fn boot_spawn_failure_leaves_no_vm_and_keeps_log_diagnostics() {
            let dir = tempdir().unwrap();
            let work = dir.path().join("work");
            std::fs::create_dir_all(&work).unwrap();
            let error = FirecrackerVm::boot_internal(
                "/definitely/missing/firecracker",
                "/kernel",
                "/rootfs",
                work.to_str().unwrap(),
                &work.join("firecracker.sock").to_string_lossy(),
                None,
                None,
                VmResources::new(0, 1),
                "/init",
                false,
                Some("/dev/vda"),
                false,
                None,
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains("Failed to start Firecracker"));
            assert!(work.join("firecracker.log").is_file());
        }

        #[test]
        fn vsock_boundary_values_are_serialized() {
            let value = serde_json::to_value(VsockConfig {
                guest_cid: u32::MAX,
                uds_path: "/tmp/v.sock".into(),
            })
            .unwrap();
            assert_eq!(value["guest_cid"], u32::MAX);
            assert_eq!(value["uds_path"], "/tmp/v.sock");
        }
    }

    // Moved out of the driver source per the tests-folder gate
    // (`scripts/check-tests-folder.sh`); the `include!` above keeps the
    // private `parse_response_head` seam visible here.
    mod response_head_tests {
        use super::*;

        #[test]
        fn parses_204_without_body() {
            let (status, len) =
                parse_response_head("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .unwrap();
            assert_eq!(status, 204);
            assert_eq!(len, 0);
        }

        #[test]
        fn parses_status_from_line_not_substring() {
            // Regression: `Content-Length: 2048` must not read as a 204 status.
            let (status, len) = parse_response_head(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2048\r\n\r\n",
            )
            .unwrap();
            assert_eq!(status, 500);
            assert_eq!(len, 2048);
        }

        #[test]
        fn parses_200_with_body() {
            let (status, len) = parse_response_head(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\n\r\n",
            )
            .unwrap();
            assert_eq!(status, 200);
            assert_eq!(len, 12);
        }

        #[test]
        fn content_length_is_case_insensitive_and_optional() {
            let (status, len) =
                parse_response_head("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                    .unwrap();
            assert_eq!(status, 204);
            assert_eq!(len, 0);
            let (status, len) = parse_response_head("HTTP/1.1 204 No Content\r\n\r\n").unwrap();
            assert_eq!(status, 204);
            assert_eq!(len, 0);
        }

        #[test]
        fn rejects_malformed_heads() {
            assert!(parse_response_head("").is_err());
            assert!(parse_response_head("\r\n\r\n").is_err());
            assert!(parse_response_head("HTTP/2 204\r\n\r\n").is_err());
            assert!(parse_response_head("HTTP/1.1 ok\r\n\r\n").is_err());
        }
    }
}

// `crate::` shims for the pasted driver source (seam contract: the pasted
// `mod.rs` may only reference these three `crate::` paths).
mod boot_args {
    pub use rfb_runtime::boot_args::BootArgs;
}
mod config {
    pub use rfb_runtime::config::firecracker_bin;
}
mod firecracker {
    pub use rfb_runtime::firecracker::FirecrackerConfig;
}

/// 回归（E2E 卡死 20 分钟的根因）：204 响应**没有 Content-Length** 且连接是
/// keep-alive——缺 CL 必须当作"无 body"立即返回，绝不能继续等 1MiB 上限。
/// 这里 body 端永远有后续字节：修复前的实现会一路读满 MAX 再报
/// "exceeds limit"；修复后一次 read 就返回头部。
#[test]
fn response_without_content_length_never_waits_for_a_body() {
    struct HeadersThenInfiniteBody {
        sent_headers: bool,
    }
    impl std::io::Read for HeadersThenInfiniteBody {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent_headers {
                self.sent_headers = true;
                let headers = b"HTTP/1.1 204 No Content\r\nServer: Firecracker\r\n\r\n";
                buf[..headers.len()].copy_from_slice(headers);
                return Ok(headers.len());
            }
            // keep-alive 上"随时可能有更多数据"：永远再给 1 字节。
            buf[0] = b'x';
            Ok(1)
        }
    }
    let response = read_http_response(&mut HeadersThenInfiniteBody {
        sent_headers: false,
    })
    .expect("a header-only 204 must return without reading a body");
    assert_eq!(
        response,
        b"HTTP/1.1 204 No Content\r\nServer: Firecracker\r\n\r\n"
    );
}
