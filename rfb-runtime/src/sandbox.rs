//! Sandbox backend classification used by the runtime.

/// The product backend a sandbox belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxBackend {
    /// forkd TCP/NDJSON guest.
    #[default]
    Forkd,
    /// An unsupported/unprovisioned backend.
    Unsupported,
}
