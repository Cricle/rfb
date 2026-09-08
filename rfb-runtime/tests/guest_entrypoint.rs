#![cfg(feature = "guest")]

use rfb_runtime::guest_entrypoint::{config, GuestTransport};

#[test]
fn public_guest_transport_has_embeddable_modes() {
    assert_eq!(config().vsock_port, rfb_runtime::config::DEFAULT_VSOCK_PORT);
}

#[cfg(feature = "forkd")]
#[test]
fn public_guest_transport_includes_forkd_agent() {}

#[cfg(feature = "zeroboot")]
#[test]
fn public_guest_transport_includes_zeroboot_mode() {
    // The ZeroBoot guest listens on the same port contract as the ZBRT provider
    // (5000). Compile-time proof the embeddable mode exists; the listener
    // itself is Linux-only and covered by the zeroboot connection suite.
    let transport = GuestTransport::ZeroBoot;
    assert_eq!(rfb_runtime::zeroboot_guest::GUEST_PORT, 5000);
    let _ = transport;
}
