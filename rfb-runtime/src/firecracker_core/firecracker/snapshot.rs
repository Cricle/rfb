//! Pause-and-snapshot lifecycle for a running Firecracker VM.

use super::socket;
use super::FirecrackerVm;
use anyhow::{bail, Result};
use serde::Serialize;
use socket::{remove_snapshot_file, snapshot_file_ready, FC_SOCKET_TIMEOUT};
use std::path::Path;
use std::time::Instant;

#[derive(Serialize)]
struct SnapshotCreate {
    snapshot_type: String,
    snapshot_path: String,
    mem_file_path: String,
}

impl FirecrackerVm {
    /// Pause the VM and create a snapshot, polling until both files are stable
    /// and non-empty (partial snapshots fail).
    pub fn snapshot(&mut self) -> Result<(String, String)> {
        let snapshot_path = Path::new(&self.snapshot_dir).join("vmstate");
        let mem_path = Path::new(&self.snapshot_dir).join("mem");
        let snapshot_path = snapshot_path.to_string_lossy().into_owned();
        let mem_path = mem_path.to_string_lossy().into_owned();

        // Pause the VM
        eprintln!("Pausing VM...");
        self.api_patch("/vm", &serde_json::json!({"state": "Paused"}))?;

        // Remove outputs from an earlier attempt. Otherwise a failed API call could
        // leave old, non-empty files that look like a newly completed snapshot.
        remove_snapshot_file(Path::new(&snapshot_path));
        remove_snapshot_file(Path::new(&mem_path));

        // Create snapshot
        eprintln!("Creating snapshot...");
        self.api_put(
            "/snapshot/create",
            &SnapshotCreate {
                snapshot_type: "Full".to_string(),
                snapshot_path: snapshot_path.clone(),
                mem_file_path: mem_path.clone(),
            },
        )?;

        // Firecracker writes both files asynchronously. Poll instead of using a
        // fixed sleep, and require non-empty files so partial snapshots fail.
        let deadline = Instant::now() + FC_SOCKET_TIMEOUT;
        let mut previous_sizes = None;
        let mut stable_polls = 0;
        while Instant::now() < deadline {
            let sizes = match (
                std::fs::symlink_metadata(&snapshot_path),
                std::fs::symlink_metadata(&mem_path),
            ) {
                (Ok(state), Ok(mem))
                    if state.file_type().is_file()
                        && mem.file_type().is_file()
                        && state.len() > 0
                        && mem.len() > 0 =>
                {
                    Some((state.len(), mem.len()))
                }
                _ => None,
            };
            if sizes.is_some() && sizes == previous_sizes {
                stable_polls += 1;
                if stable_polls >= 2 {
                    break;
                }
            } else {
                stable_polls = 0;
            }
            previous_sizes = sizes;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if stable_polls < 2 {
            remove_snapshot_file(Path::new(&snapshot_path));
            remove_snapshot_file(Path::new(&mem_path));
            bail!("Snapshot files did not become stable before timeout");
        }
        if !snapshot_file_ready(Path::new(&snapshot_path)) {
            remove_snapshot_file(Path::new(&snapshot_path));
            remove_snapshot_file(Path::new(&mem_path));
            bail!("Snapshot state file not created");
        }
        if !snapshot_file_ready(Path::new(&mem_path)) {
            remove_snapshot_file(Path::new(&snapshot_path));
            remove_snapshot_file(Path::new(&mem_path));
            bail!("Snapshot memory file not created");
        }

        let mem_size = std::fs::metadata(&mem_path)?.len();
        eprintln!(
            "Snapshot created: state={}B, mem={}MB",
            std::fs::metadata(&snapshot_path)?.len(),
            mem_size / 1024 / 1024
        );

        Ok((snapshot_path, mem_path))
    }
}
