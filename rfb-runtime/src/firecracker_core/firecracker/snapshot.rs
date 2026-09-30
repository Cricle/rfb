//! Pause-and-snapshot lifecycle for a running Firecracker VM.

use super::{FirecrackerError, FirecrackerVm};
use std::path::Path;

impl FirecrackerVm {
    /// Pause the VM and create a full snapshot in the VM's snapshot directory,
    /// polling until both files are stable and non-empty (partial snapshots fail).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn snapshot(&self) -> Result<(String, String), FirecrackerError> {
        let snapshot_dir = self.snapshot_dir.as_deref().ok_or_else(|| {
            FirecrackerError::Protocol("Firecracker VM has no snapshot directory".into())
        })?;
        let snapshot_path = Path::new(snapshot_dir).join("vmstate");
        let mem_path = Path::new(snapshot_dir).join("mem");
        let snapshot_path = snapshot_path.to_string_lossy().into_owned();
        let mem_path = mem_path.to_string_lossy().into_owned();

        self.snapshot_create(&snapshot_path, &mem_path, Some("Full"), true)?;

        let mem_size = std::fs::metadata(&mem_path)?.len();
        eprintln!(
            "Snapshot created: state={}B, mem={}MB",
            std::fs::metadata(&snapshot_path)?.len(),
            mem_size / 1024 / 1024
        );

        Ok((snapshot_path, mem_path))
    }
}
