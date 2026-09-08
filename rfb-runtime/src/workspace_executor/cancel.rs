//! The `GuestExecutor` implementation: serial turn pump and cancel lifecycle.

use super::WorkspaceGuestExecutor;
use crate::runtime_service::{GuestEvent, GuestExecutor};
use crate::session::{FileReadRequest, FileWriteRequest, SessionRequest};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

impl GuestExecutor for WorkspaceGuestExecutor {
    fn cancel_handle(&self) -> Option<Arc<AtomicBool>> {
        Some(self.cancel_requested.clone())
    }

    fn reset_cancel(&mut self) {
        self.cancel_requested.store(false, Ordering::SeqCst);
    }

    fn attach_event_sink(&mut self, sink: Option<Arc<dyn Fn(GuestEvent) + Send + Sync>>) {
        WorkspaceGuestExecutor::attach_event_sink(self, sink);
    }

    fn read_workspace_file(&mut self, request: &FileReadRequest) -> Result<Vec<u8>, String> {
        self.filesystem_read(request)
    }

    fn write_workspace_file(&mut self, request: &FileWriteRequest) -> Result<(), String> {
        self.filesystem_write(request)
    }

    fn filesystem_rpc(&mut self, op: u8, path: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        let args: Value =
            serde_json::from_slice(data).map_err(|e| format!("invalid Fs payload: {e}"))?;
        let op_name = match op {
            1 => "ls",
            2 => "find",
            3 => "grep",
            4 => "read",
            5 => "write",
            _ => return Err("unsupported Fs opcode".into()),
        };
        let mut value = serde_json::json!({"op": op_name, "args": args});
        if let Some(args) = value.get_mut("args").and_then(Value::as_object_mut) {
            // The frame's `path` field (already normalized by the ZBRT
            // connection layer) is authoritative; a path duplicated inside
            // the JSON args must not override it with an unnormalized value.
            args.insert("path".into(), Value::String(path.to_owned()));
        }
        let result = self.structured(
            &SessionRequest {
                session_id: "fs".into(),
                request_id: "fs".into(),
                prompt: String::new(),
            },
            value,
        )?;
        result
            .into_iter()
            .find(|e| e.kind == "turn.completed")
            .map(|e| e.payload)
            .ok_or("filesystem operation did not complete".into())
            .and_then(|payload| {
                // The workspace turn payload wraps the fs result in exec-shaped
                // envelope fields; strip them so the Fs result payload matches
                // the host's typed FsResult structs (bytes_written, entries, ...).
                let mut value: Value = serde_json::from_slice(&payload)
                    .map_err(|e| format!("invalid fs turn payload: {e}"))?;
                if let Some(object) = value.as_object_mut() {
                    for key in ["exit_code", "success", "stdout", "stderr", "timed_out"] {
                        object.remove(key);
                    }
                }
                serde_json::to_vec(&value).map_err(|e| format!("fs payload serialize: {e}"))
            })
    }

    fn start_turn(&mut self, request: &SessionRequest) -> Result<Vec<GuestEvent>, String> {
        if self.active.is_some() {
            return Err("request already active".into());
        }
        let v: Value = serde_json::from_str(&request.prompt)
            .map_err(|_| "prompt must be structured JSON".to_string())?;
        self.active = Some((request.session_id.clone(), request.request_id.clone()));
        let result = self.structured(request, v);
        if result.is_err()
            || result.as_ref().is_ok_and(|events| {
                events.iter().any(|event| {
                    matches!(
                        event.kind.as_str(),
                        "turn.completed" | "turn.failed" | "turn.cancelled"
                    )
                })
            })
        {
            self.active = None;
        }
        result
    }

    fn cancel(&mut self, session_id: &str, request_id: &str) -> Result<(), String> {
        match self.active.as_ref() {
            Some((s, r)) if s == session_id && r == request_id => {
                // Signal the running exec loop; it terminates the process
                // group on its next poll and returns a cancellation error.
                self.cancel_requested.store(true, Ordering::SeqCst);
                self.active = None;
                Ok(())
            }
            Some(_) => Err("active request identity mismatch".into()),
            None => Err("request is not active".into()),
        }
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.active = None;
        Ok(())
    }
}
