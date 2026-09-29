//! Serial, shell-free workspace executor with a cancellable active child.

mod cancel;
mod dispatch;
mod filesystem;
mod process;

use crate::policy::PathPolicy;
use crate::resources::RuntimeLimits;
use crate::runtime_service::GuestEvent;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

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
    ///
    /// Shared by every executor on the same root: the ZeroBoot provider pools
    /// ZBRT connections into one guest, so concurrent executors write the same
    /// workspace, and a per-connection cache would let each of them project
    /// against a stale total — making `max_workspace_bytes` a per-connection
    /// bound instead of a workspace bound.
    workspace_size_cache: SizeCache,
}

/// Cached workspace byte total: `None` means "walk the tree on next use".
type SizeCache = Arc<Mutex<Option<u64>>>;

/// Process-wide registry of workspace size caches, keyed by workspace root.
fn shared_size_cache(root: &Path) -> SizeCache {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, SizeCache>>> = OnceLock::new();
    let registry = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(registry.entry(root.to_path_buf()).or_default())
}

impl WorkspaceGuestExecutor {
    /// Create an executor rooted at `root` (created if missing), with limits.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn new(root: impl Into<PathBuf>, limits: RuntimeLimits) -> Result<Self, String> {
        limits.validate().map_err(str::to_owned)?;
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let workspace_size_cache = shared_size_cache(&root);
        Ok(Self {
            policy: PathPolicy::new(root, Vec::new()),
            limits,
            active: None,
            cancel_requested: Arc::new(AtomicBool::new(false)),
            event_sink: None,
            workspace_size_cache,
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
