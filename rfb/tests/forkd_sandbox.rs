#![cfg(feature = "cli")]

use rfb::cli::forkd::GUEST_READY_DEADLINE;
#[test]
fn guest_deadline_is_public_contract() {
    assert_eq!(GUEST_READY_DEADLINE, std::time::Duration::from_secs(30));
}
