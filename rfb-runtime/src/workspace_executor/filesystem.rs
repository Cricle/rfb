//! Workspace file read/write RPCs with byte and total-size limits.

use super::WorkspaceGuestExecutor;
use crate::session::{FileReadRequest, FileWriteRequest};
use std::fs;
use std::io::Read;
use std::path::Path;

impl WorkspaceGuestExecutor {
    pub(super) fn filesystem_read(&self, request: &FileReadRequest) -> Result<Vec<u8>, String> {
        if request.max_bytes == 0 || request.max_bytes > self.limits.max_event_bytes {
            return Err("invalid max_bytes".into());
        }
        let path = self
            .policy
            .workspace_path(&request.path)
            .map_err(|e| e.to_string())?;
        let file = fs::File::open(path).map_err(|e| e.to_string())?;
        let mut data = Vec::with_capacity(request.max_bytes);
        file.take(request.max_bytes as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() > request.max_bytes {
            return Err("file exceeds max_bytes".into());
        }
        Ok(data)
    }

    pub(super) fn filesystem_write(&self, request: &FileWriteRequest) -> Result<(), String> {
        if request.content.len() > self.limits.max_event_bytes {
            return Err("file exceeds max_event_bytes".into());
        }
        let path = self
            .policy
            .workspace_path(&request.path)
            .map_err(|e| e.to_string())?;
        let existing = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let used = workspace_size(&self.policy.workspace_root)?;
        let projected = used
            .saturating_sub(existing)
            .saturating_add(request.content.len() as u64);
        if projected > self.limits.max_workspace_bytes {
            return Err("workspace exceeds max_workspace_bytes".into());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, &request.content).map_err(|e| e.to_string())
    }
}

fn workspace_size(root: &Path) -> Result<u64, String> {
    let mut total: u64 = 0;
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        if metadata.is_dir() {
            total = total.saturating_add(workspace_size(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}
