#![cfg(all(feature = "zeroboot", target_os = "linux"))]

// Exercise request construction and failure/resource boundaries without
// launching a VM. Including the implementation keeps its private test seam
// private to this test module and does not alter production visibility.
mod firecracker {
    include!("../src/zeroboot/firecracker.rs");

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
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
                process: child,
                socket_path: path.to_owned(),
                snapshot_dir: String::new(),
                vsock_uds_path: None,
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
        fn boot_spawn_failure_leaves_no_vm_and_keeps_setup_diagnostics() {
            let dir = tempdir().unwrap();
            let work = dir.path().join("work");
            let error = FirecrackerVm::boot_internal(
                "/definitely/missing/firecracker",
                "/kernel",
                "/rootfs",
                work.to_str().unwrap(),
                0,
                "/init",
                None,
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains("Failed to start Firecracker"));
            assert!(work.join("snapshot").is_dir());
            assert!(work.join("firecracker.log").is_file());
        }

        #[test]
        fn vsock_boundary_values_are_serialized() {
            let value = serde_json::to_value(VsockDevice {
                guest_cid: u32::MAX,
                uds_path: "/tmp/v.sock".into(),
            })
            .unwrap();
            assert_eq!(value["guest_cid"], u32::MAX);
            assert_eq!(value["uds_path"], "/tmp/v.sock");
        }
    }
}
