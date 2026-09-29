//! Runtime resource limits shared by the codec and workspace executor.
//!
//! ```
//! use rfb_runtime::resources::RuntimeLimits;
//! let limits = RuntimeLimits::default();
//! assert!(limits.validate().is_ok());
//! ```

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use std::fs;

const MIB: u64 = 1024 * 1024;

/// Fallback `/workspace` tmpfs size (bytes) for environments where the guest's
/// total memory cannot be read (non-Linux builds, unusual `/proc` layouts).
/// The runtime size is derived from guest memory — see
/// [`derive_workspace_bytes`] and [`workspace_tmpfs_bytes`].
pub const WORKSPACE_TMPFS_BYTES: u64 = 256 * 1024 * 1024;

/// Floor for the derived `/workspace` tmpfs size: a sandbox below this much
/// workspace is useless, so tiny memory budgets still get a usable tmpfs.
pub const WORKSPACE_TMPFS_MIN_BYTES: u64 = 64 * MIB;
/// Ceiling for the derived `/workspace` tmpfs size: no guest needs more than
/// 1 GiB of investigation artifacts in RAM.
pub const WORKSPACE_TMPFS_MAX_BYTES: u64 = 1024 * MIB;

/// Derive the `/workspace` tmpfs size from the guest's total memory: 40% of
/// `mem_total_bytes`, floored to whole MiB and clamped to
/// 64 MiB..=1 GiB. The MiB flooring is deliberate: the tmpfs mount option
/// (`guest.rs::mount_workspace_tmpfs`) is also expressed in whole MiB, so the
/// executor-side limit can never exceed the kernel-enforced tmpfs cap (which
/// would turn policy errors into raw ENOSPC from the mount).
pub fn derive_workspace_bytes(mem_total_bytes: u64) -> u64 {
    // 40% = 2/5, computed in whole MiB so the result stays MiB-aligned.
    let share_mib = mem_total_bytes / MIB * 2 / 5;
    (share_mib * MIB).clamp(WORKSPACE_TMPFS_MIN_BYTES, WORKSPACE_TMPFS_MAX_BYTES)
}

/// Total guest memory in bytes parsed from `/proc/meminfo` (`MemTotal`, which
/// the kernel reports in KiB).
#[cfg(target_os = "linux")]
fn mem_total_bytes() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest.trim().strip_suffix(" kB")?.trim().parse().ok()?;
            return Some(kib.saturating_mul(1024));
        }
    }
    None
}

/// Non-Linux builds have no `/proc/meminfo`: fall back to the constant.
#[cfg(not(target_os = "linux"))]
fn mem_total_bytes() -> Option<u64> {
    None
}

/// The `/workspace` tmpfs size actually used by this process: derived from the
/// guest's total memory when available, else the [`WORKSPACE_TMPFS_BYTES`]
/// fallback. Cached after the first read (the value cannot change mid-run).
pub fn workspace_tmpfs_bytes() -> u64 {
    use std::sync::OnceLock;
    static CACHE: OnceLock<u64> = OnceLock::new();
    *CACHE.get_or_init(|| {
        mem_total_bytes()
            .map(derive_workspace_bytes)
            .unwrap_or(WORKSPACE_TMPFS_BYTES)
    })
}

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
            // Same source as the tmpfs actually mounted by the guest init:
            // 40% of MemTotal (clamped to 64 MiB..1 GiB), so the executor-side
            // limit can never exceed the kernel-enforced tmpfs cap (which
            // would turn policy errors into raw ENOSPC from the mount).
            max_workspace_bytes: workspace_tmpfs_bytes(),
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
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
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
