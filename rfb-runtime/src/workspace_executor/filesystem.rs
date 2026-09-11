//! Workspace file read/write RPCs with byte and total-size limits.

use super::WorkspaceGuestExecutor;
use crate::session::{FileReadRequest, FileWriteRequest};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

impl WorkspaceGuestExecutor {
    /// Typed read (host control message): fails closed when the file exceeds
    /// `max_bytes`; truncation semantics only exist on the structured path.
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

    /// Structured `read` op: at most `max_bytes` starting at `offset`,
    /// truncating (instead of erroring) when more data remains. Returns
    /// `(data, truncated, total_file_bytes)` so callers can report whether
    /// bytes exist beyond `offset + data.len()`.
    pub(super) fn filesystem_read_offset(
        &self,
        path: &str,
        max_bytes: usize,
        offset: u64,
    ) -> Result<(Vec<u8>, bool, u64), String> {
        if max_bytes == 0 || max_bytes > self.limits.max_event_bytes {
            return Err("invalid max_bytes".into());
        }
        let path = self
            .policy
            .workspace_path(path)
            .map_err(|e| e.to_string())?;
        let mut file = fs::File::open(&path).map_err(|e| e.to_string())?;
        let total = file.metadata().map_err(|e| e.to_string())?.len();
        // A beyond-EOF offset simply yields no data.
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut data = Vec::with_capacity(max_bytes);
        file.take(max_bytes as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        let truncated = data.len() > max_bytes;
        data.truncate(max_bytes);
        Ok((data, truncated, total))
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
        let mut cache = self
            .workspace_size_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The recursive walk is O(workspace); cache it and patch by delta on
        // each write (full walks only after an invalidation, e.g. `exec`).
        let used = match *cache {
            Some(used) => used,
            None => {
                let used = workspace_size(&self.policy.workspace_root)?;
                *cache = Some(used);
                used
            }
        };
        let projected = used
            .saturating_sub(existing)
            .saturating_add(request.content.len() as u64);
        if projected > self.limits.max_workspace_bytes {
            return Err("workspace exceeds max_workspace_bytes".into());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Temp file + rename so a crash mid-write leaves the previous content
        // intact instead of a truncated file (same directory = one filesystem).
        write_atomic(&path, &request.content).map_err(|e| e.to_string())?;
        // The size cache is shared per workspace root and this guard is held
        // across the write, so no other executor (including one on another
        // pooled connection) could have mutated the workspace in between: the
        // projected total is the new total.
        *cache = Some(projected);
        Ok(())
    }

    /// Drop the cached workspace size so the next size check re-walks; used
    /// by `exec`, whose child can mutate the workspace arbitrarily.
    pub(super) fn invalidate_workspace_size(&self) {
        *self
            .workspace_size_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Write `content` to a uniquely named temp file beside `path`, then rename it
/// over `path` (atomic on the same filesystem). The temp file is removed when
/// any step fails.
fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?
        .to_os_string();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut tmp_name = name;
    tmp_name.push(format!(".{}.{}.tmp", nanos, std::process::id()));
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(tmp_name);
    let result = fs::File::create(&tmp)
        .and_then(|mut file| file.write_all(content))
        .and_then(|_| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
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
