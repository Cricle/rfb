//! Runtime resource limits shared by the codec and workspace executor.
//!
//! ```
//! use rfb_runtime::resources::RuntimeLimits;
//! let limits = RuntimeLimits::default();
//! assert!(limits.validate().is_ok());
//! ```

use serde::{Deserialize, Serialize};

/// Size of the guest `/workspace` tmpfs, mounted by the PID-1 guest init
/// (`guest.rs::mount_workspace_tmpfs`).
///
/// This is the single source of truth: `RuntimeLimits::default()`
/// derives `max_workspace_bytes` from it so the executor-side limit can never
/// exceed the kernel-enforced tmpfs cap (which would turn policy errors into
/// raw ENOSPC from the mount).
pub const WORKSPACE_TMPFS_BYTES: u64 = 256 * 1024 * 1024;

/// Hard limits the runtime enforces on frames, events, workspace, and turn time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLimits {
    /// Maximum encoded frame payload (whole frame minus header).
    pub max_frame_bytes: usize,
    /// Maximum size of a single event payload.
    pub max_event_bytes: usize,
    /// Channel capacity for forwarded events.
    pub channel_capacity: usize,
    /// Maximum total workspace bytes.
    pub max_workspace_bytes: u64,
    /// Default maximum runtime of a single turn in seconds.
    pub max_runtime_seconds: u64,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 4 * 1024 * 1024,
            max_event_bytes: 512 * 1024,
            channel_capacity: 64,
            max_workspace_bytes: WORKSPACE_TMPFS_BYTES,
            max_runtime_seconds: 1800,
        }
    }
}

impl RuntimeLimits {
    /// Validate the limits are internally consistent and bounded.
    ///
    /// ```
    /// use rfb_runtime::resources::RuntimeLimits;
    /// let mut limits = RuntimeLimits::default();
    /// limits.channel_capacity = 0;
    /// assert_eq!(limits.validate(), Err("invalid channel_capacity"));
    /// ```
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_frame_bytes == 0 || self.max_frame_bytes > 64 * 1024 * 1024 {
            return Err("invalid max_frame_bytes");
        }
        if self.max_event_bytes == 0 || self.max_event_bytes > self.max_frame_bytes {
            return Err("invalid max_event_bytes");
        }
        if self.channel_capacity == 0 || self.channel_capacity > 4096 {
            return Err("invalid channel_capacity");
        }
        if self.max_runtime_seconds == 0 {
            return Err("invalid max_runtime_seconds");
        }
        Ok(())
    }
}
