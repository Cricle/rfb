//! Public value types of the unified facade (`sdk/UNIFIED_API.md` §1, §5, §6).

use std::time::Duration;

/// Unified result of `exec` and `eval` across both guest transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// Process exit code (`-1` when the turn was cancelled).
    pub exit_code: i32,
    /// Collected stdout bytes.
    pub stdout: Vec<u8>,
    /// Collected stderr bytes.
    pub stderr: Vec<u8>,
    /// Whether the operation ended because its deadline elapsed.
    pub timed_out: bool,
}

impl ExecResult {
    /// Stdout decoded as UTF-8 (lossy).
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr decoded as UTF-8 (lossy).
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Kind of a [`StreamEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEventKind {
    /// The stream session was accepted by the guest.
    Started,
    /// Stdout chunk.
    Stdout,
    /// Stderr chunk.
    Stderr,
    /// Terminal exit of the stream session.
    Exit,
}

/// One event of an interactive guest stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    /// Event kind.
    pub kind: StreamEventKind,
    /// Payload bytes (empty for `Started`).
    pub data: Vec<u8>,
    /// Exit code, present only for the terminal `Exit` event.
    pub code: Option<i32>,
}

/// Guest transport used by a [`Sandbox`](super::Sandbox). Both transports
/// expose identical facade method shapes; `Ndjson` is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GuestTransport {
    /// forkd guest newline-delimited JSON over TCP.
    #[default]
    Ndjson,
    /// ZBRT v1 binary frames over TCP (requires the `zeroboot` feature).
    #[cfg(feature = "zeroboot")]
    Zbrt,
}

/// Options for `RfbClient::create_sandbox` (`sdk/UNIFIED_API.md` §3).
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Number of sandboxes to create.
    pub n: usize,
    /// Give each created sandbox its own network namespace.
    pub per_child_netns: bool,
    /// Optional memory limit in MiB.
    pub memory_limit_mib: Option<u64>,
    /// Prewarm the sandbox before returning.
    pub prewarm: bool,
    /// Branch the sandbox from a live guest.
    pub live_fork: bool,
    /// Use huge pages for sandbox memory.
    pub hugepages: bool,
    /// Guest transport attached to the returned sandboxes.
    pub transport: GuestTransport,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            n: 1,
            per_child_netns: false,
            memory_limit_mib: None,
            prewarm: false,
            live_fork: false,
            hugepages: false,
            transport: GuestTransport::default(),
        }
    }
}

/// Convenience alias: a guest-op timeout of ten seconds, the protocol default.
pub(super) fn default_guest_timeout() -> Duration {
    Duration::from_secs(10)
}
