//! PID-1 boot for the guest runtime: console attach, mount setup, and the
//! `RFB_RUNTIME_READY` readiness marker for stdio mode.

use std::io;

/// Boot the guest as PID 1 when required (`attach_console` toggles vsock vs
/// stdio console wiring), then print the readiness marker on stdio boot only.
pub fn boot_environment(attach_console: bool) -> io::Result<()> {
    rfb_runtime::guest::init_pid1_with_console(attach_console)?;
    #[cfg(target_os = "linux")]
    if !attach_console && std::process::id() == 1 {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(b"RFB_RUNTIME_READY\n")
            .map_err(io::Error::other)?;
        stdout.flush().map_err(io::Error::other)?;
    }
    Ok(())
}
