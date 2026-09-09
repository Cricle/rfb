//! Guest kernel command line builder shared by every Firecracker driver.
//!
//! All boot-arg strings in the workspace MUST be produced here so the drivers
//! cannot drift apart again. Historical variants (before this builder):
//! - `console=ttyS0 reboot=k panic=1 pci=off init=<path>` (firecracker_core)
//! - `... random.trust_cpu=on root=/dev/vda rw init=<path>` (zeroboot, CLI)
//! - `... init=/sbin/rfb-runtime vsock` (RFB1 request model)
//!
//! Compose the flags each driver needs; `finish` renders them space-joined in
//! the canonical order (base, trust_cpu, root/rw, init, vsock).

/// Builder for the guest kernel command line.
#[derive(Debug, Clone)]
pub struct BootArgs {
    parts: Vec<String>,
}

impl Default for BootArgs {
    fn default() -> Self {
        Self::new()
    }
}

impl BootArgs {
    /// Base flags shared by every driver: serial console, no reboot, panic
    /// after 1s, no PCI (Firecracker has none).
    pub fn new() -> Self {
        Self {
            parts: vec![
                "console=ttyS0".into(),
                "reboot=k".into(),
                "panic=1".into(),
                "pci=off".into(),
            ],
        }
    }

    /// Trust the CPU RNG (`random.trust_cpu=on`) so the guest does not block
    /// on entropy during early boot.
    pub fn random_trust_cpu(mut self) -> Self {
        self.parts.push("random.trust_cpu=on".into());
        self
    }

    /// Mount `device` (usually `/dev/vda`) as read-write root.
    pub fn root_rw(mut self, device: &str) -> Self {
        self.parts.push(format!("root={device}"));
        self.parts.push("rw".into());
        self
    }

    /// Guest entrypoint binary (`init=<path>`).
    pub fn init(mut self, path: &str) -> Self {
        self.parts.push(format!("init={path}"));
        self
    }

    /// Append the trailing `vsock` token. The RFB1 guest runtime reads it
    /// from `/proc/cmdline` (it never appears in argv) to select vsock mode.
    pub fn vsock(mut self) -> Self {
        self.parts.push("vsock".into());
        self
    }

    /// Render the space-joined kernel command line.
    pub fn finish(self) -> String {
        self.parts.join(" ")
    }
}
