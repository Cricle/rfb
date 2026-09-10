#![cfg(target_os = "linux")]

use rfb_runtime::vsock::{validate_endpoint, DEFAULT_PORT};
use tokio_vsock::VMADDR_CID_ANY;

#[test]
fn validates_guest_vsock_endpoint() {
    assert!(validate_endpoint(VMADDR_CID_ANY, 0).is_err());
    assert!(validate_endpoint(52, DEFAULT_PORT - 1).is_err());
    assert!(validate_endpoint(52, DEFAULT_PORT).is_ok());
    assert!(validate_endpoint(52, DEFAULT_PORT + 1).is_err());
}
