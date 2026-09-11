//! The capability surface advertised by the Rig adapter.

use rfb::{Capability, Sandbox};

/// A known capability exposed as a Rig tool. `core()` maps it to the RFB core
/// capability that gates registration; `Edit` is a compound of read+write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RigCapability {
    /// Stable `bash` tool, backed by the core execute capability.
    Bash,
    /// Stable structured text replacement, backed by core read/write.
    Edit,
    /// Legacy execute operation (not part of the default seven-tool surface).
    Execute,
    /// Stream a live command.
    Stream,
    /// Read a guest file.
    Read,
    /// Write a guest file.
    Write,
    /// Grep a guest directory.
    Grep,
    /// Find files in a guest directory.
    Find,
    /// List a guest directory.
    Ls,
    /// Evaluate code in the guest.
    Eval,
    /// Ping the guest for liveness.
    Ping,
    /// Cancel an in-flight guest operation.
    Cancel,
}

impl RigCapability {
    pub(super) fn core(self) -> Option<Capability> {
        match self {
            Self::Bash => Some(Capability::Execute),
            Self::Edit => None,
            Self::Execute => Some(Capability::Execute),
            Self::Stream => Some(Capability::Stream),
            Self::Read => Some(Capability::ReadFile),
            Self::Write => Some(Capability::WriteFile),
            Self::Grep => Some(Capability::Grep),
            Self::Find => Some(Capability::Find),
            Self::Ls => Some(Capability::Ls),
            Self::Eval => Some(Capability::Eval),
            Self::Ping => Some(Capability::Health),
            Self::Cancel => Some(Capability::Cancel),
        }
    }

    pub(super) fn available(self, sandbox: &dyn Sandbox) -> bool {
        match self {
            // Edit is a compound operation and is registered only when both
            // primitive structured operations are available.
            Self::Edit => {
                sandbox.capabilities().contains(&Capability::ReadFile)
                    && sandbox.capabilities().contains(&Capability::WriteFile)
            }
            _ => self
                .core()
                .is_some_and(|capability| sandbox.capabilities().contains(&capability)),
        }
    }
}
