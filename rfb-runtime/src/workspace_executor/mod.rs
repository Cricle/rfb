//! Serial, shell-free workspace executor with a cancellable active child.

mod cancel;
mod dispatch;
mod filesystem;
mod process;

use crate::policy::PathPolicy;
use crate::resources::RuntimeLimits;
use crate::runtime_service::GuestEvent;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Serial, shell-free workspace executor with a cancellable active child.
pub struct WorkspaceGuestExecutor {
    policy: PathPolicy,
    limits: RuntimeLimits,
    active: Option<(String, String)>,
    cancel_requested: Arc<AtomicBool>,
    /// Live event sink used by streaming transports (ZeroBoot V1). When set,
    /// `exec` forwards `terminal.output` chunks to the sink as they are read
    /// instead of only reporting them in the aggregated result; RFB1 turns
    /// leave this `None` and keep the buffered event contract unchanged.
    event_sink: Option<Arc<dyn Fn(GuestEvent) + Send + Sync>>,
    /// Cached total workspace size in bytes; `None` forces a fresh recursive
    /// walk. Writes patch it by delta and `exec` invalidates it (its child can
    /// mutate the workspace arbitrarily), so the size limit never costs a full
    /// O(workspace) walk per write.
    workspace_size_cache: Mutex<Option<u64>>,
}

impl WorkspaceGuestExecutor {
    /// Create an executor rooted at `root` (created if missing), with limits.
    pub fn new(root: impl Into<PathBuf>, limits: RuntimeLimits) -> Result<Self, String> {
        limits.validate().map_err(str::to_owned)?;
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        Ok(Self {
            policy: PathPolicy::new(root, Vec::new()),
            limits,
            active: None,
            cancel_requested: Arc::new(AtomicBool::new(false)),
            event_sink: None,
            workspace_size_cache: Mutex::new(None),
        })
    }

    /// Shared cancellation signal observed by a running child loop. A running
    /// request can be interrupted from another thread/connection by calling
    /// the returned flag (or by setting it directly) without
    /// holding the executor.
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel_requested.clone()
    }

    /// Attach (or detach) a live event sink. Streaming transports set it before
    /// a turn so `terminal.output` chunks are forwarded while the child runs.
    pub fn attach_event_sink(&mut self, sink: Option<Arc<dyn Fn(GuestEvent) + Send + Sync>>) {
        self.event_sink = sink;
    }
}
