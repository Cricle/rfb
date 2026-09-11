//! Structured filesystem RPCs: `ls`, `find`, `grep`, and the dispatch that
//! turns a structured prompt into terminal guest events.

use super::WorkspaceGuestExecutor;
use crate::runtime_service::GuestEvent;
use crate::session::SessionRequest;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

impl WorkspaceGuestExecutor {
    /// Dispatch one structured prompt into the typed workspace operations.
    pub(super) fn structured(
        &mut self,
        _request: &SessionRequest,
        value: Value,
    ) -> Result<Vec<GuestEvent>, String> {
        let op = value
            .get("op")
            .and_then(Value::as_str)
            .ok_or("request op is required")?;
        if value.as_object().is_none_or(|object| {
            object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "op" | "args" | "cwd" | "stdin" | "timeout_secs" | "timeout_ms"
                )
            })
        }) {
            return Err("unsupported request fields".into());
        }
        let result = match op {
            "exec" => self.exec(&value, &self.cancel_requested)?,
            "ls" => self.list(&value)?,
            "find" => self.find(&value)?,
            "grep" => self.grep(&value)?,
            "read" => self.read(&value)?,
            "write" => self.write(&value)?,
            _ => return Err("unsupported operation".into()),
        };
        let mut events = vec![GuestEvent::new("turn.started", Vec::new())];
        if op == "exec" {
            if let Some(data) = result.get("stdout").and_then(Value::as_str) {
                if !data.is_empty() {
                    events.push(GuestEvent::new(
                        "terminal.output",
                        serde_json::to_vec(&crate::session::TerminalEvent {
                            stream: crate::session::TerminalStream::Stdout,
                            data: data.to_owned(),
                        })
                        .map_err(|e| e.to_string())?,
                    ));
                }
            }
            if let Some(data) = result.get("stderr").and_then(Value::as_str) {
                if !data.is_empty() {
                    events.push(GuestEvent::new(
                        "terminal.output",
                        serde_json::to_vec(&crate::session::TerminalEvent {
                            stream: crate::session::TerminalStream::Stderr,
                            data: data.to_owned(),
                        })
                        .map_err(|e| e.to_string())?,
                    ));
                }
            }
        }
        let result = if op == "exec" {
            result
        } else {
            let mut object = result
                .as_object()
                .cloned()
                .ok_or("operation result must be an object")?;
            object.insert("exit_code".into(), Value::Null);
            object.insert("success".into(), Value::Bool(true));
            object.insert("stdout".into(), Value::String(String::new()));
            object.insert("stderr".into(), Value::String(String::new()));
            Value::Object(object)
        };
        events.push(GuestEvent::new(
            "turn.completed",
            serde_json::to_vec(&result).map_err(|e| e.to_string())?,
        ));
        Ok(events)
    }

    fn read(&self, value: &Value) -> Result<Value, String> {
        let a = value.get("args").ok_or("args is required")?;
        let path = a
            .get("path")
            .and_then(Value::as_str)
            .ok_or("path is required")?;
        let max = a
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(self.limits.max_event_bytes as u64) as usize;
        let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let (data, truncated, total) = self.filesystem_read_offset(path, max, offset)?;
        // total_bytes is the whole-file size, so hosts can detect truncation
        // as `offset + data.len() < total_bytes`.
        Ok(json!({"data": data, "truncated": truncated, "total_bytes": total}))
    }

    fn write(&self, value: &Value) -> Result<Value, String> {
        let a = value.get("args").ok_or("args is required")?;
        let path = a
            .get("path")
            .and_then(Value::as_str)
            .ok_or("path is required")?;
        let data = a
            .get("data")
            .and_then(Value::as_array)
            .ok_or("data is required")?
            .iter()
            .map(|v| {
                let n = v.as_u64().ok_or("data must be bytes in 0..=255")?;
                if n > 255 {
                    return Err("data must be bytes in 0..=255".into());
                }
                Ok(n as u8)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let n = data.len();
        self.filesystem_write(&crate::session::FileWriteRequest {
            request_id: "fs".into(),
            path: path.into(),
            content: data,
        })?;
        Ok(json!({"bytes_written": n}))
    }

    fn list(&self, value: &Value) -> Result<Value, String> {
        let a = value.get("args").ok_or("args is required")?;
        let path = self
            .policy
            .workspace_path(a.get("path").and_then(Value::as_str).unwrap_or("."))
            .map_err(|e| e.to_string())?;
        let max = a.get("max_results").and_then(Value::as_u64).unwrap_or(100) as usize;
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entries.len() >= max {
                truncated = true;
                break;
            }
            // Host `LsResult` expects structured DirEntry objects
            // ({name, is_dir, size}), not bare file-name strings.
            let metadata = entry.metadata().map_err(|e| e.to_string());
            let (is_dir, size) = match metadata {
                Ok(meta) => (meta.is_dir(), Some(meta.len())),
                Err(_) => (false, None),
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy().into_owned(),
                "is_dir": is_dir,
                "size": size,
            }));
        }
        Ok(json!({"entries": entries, "truncated": truncated}))
    }

    fn find(&self, value: &Value) -> Result<Value, String> {
        let a = value.get("args").ok_or("args is required")?;
        let root = self
            .policy
            .workspace_path(a.get("path").and_then(Value::as_str).unwrap_or("."))
            .map_err(|e| e.to_string())?;
        let pattern = a
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or("pattern is required")?;
        let max = a.get("max_results").and_then(Value::as_u64).unwrap_or(100) as usize;
        let mut out = Vec::new();
        walk(&root, &root, pattern, max, &mut out)?;
        // Host `FindResult` expects {matches, truncated}.
        Ok(json!({"matches": out, "truncated": false}))
    }

    fn grep(&self, value: &Value) -> Result<Value, String> {
        let a = value.get("args").ok_or("args is required")?;
        let root = self
            .policy
            .workspace_path(a.get("path").and_then(Value::as_str).unwrap_or("."))
            .map_err(|e| e.to_string())?;
        let pattern = a
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or("pattern is required")?;
        let max = a.get("max_results").and_then(Value::as_u64).unwrap_or(100) as usize;
        let max_bytes = a
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(u32::MAX)) as usize;
        // Grep over a file or recursively over a directory of regular files,
        // matching the host GrepMatch contract ({path, line, text}).
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        let mut stack = vec![root.clone()];
        let mut visited = std::collections::HashSet::new();
        'outer: while let Some(current) = stack.pop() {
            // Use symlink_metadata to detect symlinks without following them,
            // preventing symlink-cycle hangs.
            let meta = match fs::symlink_metadata(&current) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let ft = meta.file_type();
            if !ft.is_file() {
                if visited.insert(current.clone()) {
                    let entries = match fs::read_dir(&current) {
                        Ok(entries) => entries,
                        Err(_) => continue,
                    };
                    for entry in entries {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(_) => continue,
                        };
                        stack.push(entry.path());
                    }
                }
                continue;
            }
            if scanned >= max_bytes {
                truncated = true;
                break 'outer;
            }
            let data = match fs::read(&current) {
                Ok(data) => data,
                Err(_) => continue,
            };
            scanned += data.len();
            if scanned > max_bytes {
                truncated = true;
            }
            let text = String::from_utf8_lossy(&data);
            let relative = current
                .strip_prefix(&root)
                .unwrap_or(&current)
                .to_string_lossy()
                .into_owned();
            let display = if relative.is_empty() {
                a.get("path")
                    .and_then(Value::as_str)
                    .unwrap_or(".")
                    .to_owned()
            } else {
                relative
            };
            for (index, line) in text.lines().enumerate() {
                if line.contains(pattern) {
                    matches.push(json!({
                        "path": display,
                        "line": index as u64 + 1,
                        "text": line,
                    }));
                    if matches.len() >= max {
                        break 'outer;
                    }
                }
            }
        }
        Ok(json!({"matches": matches, "truncated": truncated}))
    }
}

fn walk(
    root: &Path,
    base: &Path,
    pattern: &str,
    max: usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    if out.len() >= max {
        return Ok(());
    }
    for e in fs::read_dir(root).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let p = e.path();
        // Skip symlinks to prevent cycle hangs
        if e.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
            continue;
        }
        if e.file_name().to_string_lossy().contains(pattern) {
            out.push(
                p.strip_prefix(base)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned(),
            );
            if out.len() >= max {
                return Ok(());
            }
        }
        if p.is_dir() {
            walk(&p, base, pattern, max, out)?;
        }
    }
    Ok(())
}
