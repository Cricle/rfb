#[cfg(feature = "forkd")]
use rfb_runtime::agent;
use rfb_runtime::guest_entrypoint::{self, GuestTransport};
use std::io;

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mode = std::env::args().nth(1);
    let init_script = std::env::args()
        .next()
        .and_then(|arg0| {
            std::path::Path::new(&arg0)
                .file_name()
                .map(|n| n.to_owned())
        })
        .unwrap_or_default();
    let forkd_init_file = std::path::Path::new("/forkd-init.sh").is_file();
    let forkd_agent = mode.as_deref() == Some("forkd-agent")
        || std::env::var("RFB_RUNTIME_MODE").ok().as_deref() == Some("forkd-agent")
        || (init_script == "forkd-init.sh" && forkd_init_file)
        || (forkd_init_file && unsafe { libc::getpid() } == 1);
    #[cfg(feature = "zeroboot")]
    let zeroboot_mode = mode.as_deref() == Some("zeroboot")
        || std::env::var("RFB_RUNTIME_MODE").ok().as_deref() == Some("zeroboot")
        // The ZBRT image built by `rfb-cli image build-rootfs --mode
        // zeroboot-zbrt` installs this binary as /init with no argv/env hint;
        // the protocol marker written next to it is the only mode signal.
        || zeroboot_marker_active();
    #[cfg(not(feature = "zeroboot"))]
    let zeroboot_mode = false;
    let vsock_mode =
        mode.as_deref() == Some("vsock") || (!forkd_agent && !zeroboot_mode && mode.is_none());
    if !forkd_agent && !zeroboot_mode && !vsock_mode {
        eprintln!("usage: rfb-runtime [vsock|zeroboot|forkd-agent]");
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown runtime mode",
        ));
    }
    if forkd_agent {
        return agent::run(
            &rfb_runtime::config::RuntimeConfig::from_environment().forkd_agent_addr,
        )
        .await;
    }
    #[cfg(feature = "zeroboot")]
    if zeroboot_mode {
        return guest_entrypoint::run(GuestTransport::ZeroBoot).await;
    }
    guest_entrypoint::run(GuestTransport::Vsock).await
}

/// True when the guest filesystem carries the ZBRT protocol marker written by
/// `image build-rootfs --mode zeroboot-zbrt` (`/etc/zeroboot-protocol: zbrt`).
#[cfg(feature = "zeroboot")]
fn zeroboot_marker_active() -> bool {
    std::fs::read_to_string("/etc/zeroboot-protocol")
        .map(|value| value.trim().eq_ignore_ascii_case("zbrt"))
        .unwrap_or(false)
}
