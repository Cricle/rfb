//! RFB1 framed-vsock client and acceptance gate.
//!
//! RFB1 is the runtime's framed protocol (outer little-endian u32 length,
//! then `RFB1` magic + type + flags + sequence + payload, postcard-coded).
//! It rides on Firecracker's host-side vsock UDS relay, which requires a
//! `CONNECT <port>\n` handshake before byte streaming.
//!
//! This module reuses the runtime's `codec`/`session` crate (via the `cli`
//! feature's `rfb-runtime` dependency) so the CLI never re-implements the
//! wire format. The acceptance gate boots a real Firecracker VM (KVM),
//! validates the rootfs contract, then performs Hello/Capabilities/ShutdownAck
//! over the vsock relay.

mod boot;
mod wire;

pub use boot::{
    boot_firecracker, boot_firecracker_with, firecracker_version_ok, validate_rootfs_contract,
    BootOptions, DEFAULT_CID, DEFAULT_KERNEL, DEFAULT_PORT, DEFAULT_ROOTFS, DEFAULT_VSOCK_UDS,
};
pub use wire::{connect_vsock_uds, exchange};

use crate::cli::error::{io, CliError};
use crate::cli::host::{detect, require_vm_host};
use crate::cli::tool::HostKind;
use serde_json::{json, Value};
use std::{fs, net::Shutdown, path::Path, time::Duration};

use rfb_runtime::codec::{FrameCodec, MessageType};
use rfb_runtime::session::{ControlMessage, RuntimeMessage};

/// Full RFB1 acceptance: boot a real VM, validate the rootfs contract, then
/// run HelloAck / Capabilities / ShutdownAck over the vsock relay.
pub fn acceptance(
    kernel: &Path,
    rootfs: &Path,
    firecracker: &str,
    require_vm: bool,
) -> Result<Value, CliError> {
    if !require_vm {
        // Mirror the script's skip semantics: report readiness, callers decide.
        let caps = detect();
        if !matches!(caps.kind, HostKind::Linux | HostKind::Wsl) {
            return Ok(json!({"status": "skipped", "reason": "Linux/WSL required"}));
        }
        if !caps.kvm {
            return Ok(json!({"status": "skipped", "reason": "/dev/kvm unavailable"}));
        }
    }
    require_vm_host()?;
    validate_rootfs_contract(rootfs)?;

    let work_dir = std::env::temp_dir().join(format!("rfb-cli-rfb1-{}", std::process::id()));
    let uds = work_dir.join("vsock.sock");
    let log = work_dir.join("firecracker.log");
    let _ = fs::remove_dir_all(&work_dir);
    fs::create_dir_all(&work_dir).map_err(|error| io(error.to_string()))?;

    let mut child = boot_firecracker(
        firecracker,
        kernel,
        rootfs,
        &work_dir,
        DEFAULT_CID,
        &uds,
        &log,
    )?;

    let result = (|| -> Result<Value, CliError> {
        let mut stream = connect_vsock_uds(&uds, DEFAULT_PORT, Duration::from_secs(15))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| io(error.to_string()))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| io(error.to_string()))?;
        let codec = FrameCodec::default();

        // Hello: postcard `ControlMessage::Hello { protocol_version: 1 }` ->
        // HelloAck { protocol_version: 1 }.
        exchange(
            &mut stream,
            &codec,
            0,
            MessageType::Hello,
            &ControlMessage::Hello {
                protocol_version: 1,
            },
            MessageType::HelloAck,
            &RuntimeMessage::HelloAck {
                protocol_version: 1,
            },
        )?;

        // Capabilities: session_per_vm + writable_workspace round trip.
        exchange(
            &mut stream,
            &codec,
            1,
            MessageType::Capabilities,
            &ControlMessage::Capabilities {
                session_per_vm: true,
                writable_workspace: true,
            },
            MessageType::Capabilities,
            &RuntimeMessage::Capabilities {
                session_per_vm: true,
                writable_workspace: true,
            },
        )?;

        // Shutdown -> ShutdownAck. The runtime writes ShutdownAck with the
        // Shutdown (9) message type on the wire.
        exchange(
            &mut stream,
            &codec,
            2,
            MessageType::Shutdown,
            &ControlMessage::Shutdown,
            MessageType::Shutdown,
            &RuntimeMessage::ShutdownAck,
        )?;

        let _ = stream.shutdown(Shutdown::Both);
        Ok(json!({
            "status": "passed",
            "checks": ["hello_ack", "capabilities", "shutdown_ack"],
            "note": "StartTurn/terminal not exercised (minimal guest has no shell/tool binaries)",
            "payload": "suppressed",
        }))
    })();

    // Always tear down the VM.
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&work_dir);
    result
}
