//! Path policy: workspace confinement and read-only host roots.
//!
//! ```
//! use rfb_runtime::policy::{PathPolicy, PolicyError};
//! let policy = PathPolicy::new("/workspace", vec!["/sources".into()]);
//! assert_eq!(policy.workspace_path("src/main.rs").unwrap(), std::path::PathBuf::from("/workspace/src/main.rs"));
//! assert!(matches!(policy.workspace_path("../etc"), Err(PolicyError::Escape)));
//! ```

use std::path::{Path, PathBuf};

/// Policy rejection modes.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// Path escapes the allowed root.
    #[error("path escapes allowed root")]
    Escape,
    /// A write was attempted under a read-only host root.
    #[error("write is not allowed for read-only host path")]
    ReadOnly,
    /// Path does not belong to an allowed root.
    #[error("path is not inside an allowed root")]
    NotAllowed,
}

/// Confines guest paths to a writable workspace root and (optionally) a set of
/// read-only host roots.
#[derive(Debug, Clone)]
pub struct PathPolicy {
    /// Absolute host roots that are readable but never writable.
    pub read_only_host_roots: Vec<PathBuf>,
    /// The writable workspace root all guest paths resolve beneath.
    pub workspace_root: PathBuf,
}

impl PathPolicy {
    /// Construct a policy with a workspace root and read-only host roots.
    pub fn new(workspace_root: impl Into<PathBuf>, read_only_host_roots: Vec<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            read_only_host_roots,
        }
    }

    /// Resolve a workspace-relative path, rejecting escapes.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn workspace_path(&self, relative: impl AsRef<Path>) -> Result<PathBuf, PolicyError> {
        self.safe_join(&self.workspace_root, relative.as_ref())
    }

    /// Resolve a path under the `root_index`-th read-only host root.
    ///
    /// ```
    /// use rfb_runtime::policy::{PathPolicy, PolicyError};
    /// let policy = PathPolicy::new("/workspace", vec!["/sources".into()]);
    /// assert_eq!(policy.host_read_path(0, "README").unwrap(), std::path::PathBuf::from("/sources/README"));
    /// assert!(matches!(policy.host_read_path(1, "README"), Err(PolicyError::NotAllowed)));
    /// ```
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn host_read_path(
        &self,
        root_index: usize,
        relative: impl AsRef<Path>,
    ) -> Result<PathBuf, PolicyError> {
        let root = self
            .read_only_host_roots
            .get(root_index)
            .ok_or(PolicyError::NotAllowed)?;
        self.safe_join(root, relative.as_ref())
    }

    /// Host paths are always read-only; any write is rejected.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn can_write_host(&self, _path: impl AsRef<Path>) -> Result<(), PolicyError> {
        Err(PolicyError::ReadOnly)
    }

    fn safe_join(&self, root: &Path, relative: &Path) -> Result<PathBuf, PolicyError> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(PolicyError::Escape);
        }
        let candidate = root.join(relative);
        if !candidate
            .components()
            .collect::<Vec<_>>()
            .starts_with(&root.components().collect::<Vec<_>>())
        {
            return Err(PolicyError::Escape);
        }
        // Symlink containment: a workspace-relative name may traverse a
        // symlink the guest created via exec, pointing outside the workspace.
        // Canonicalize the deepest existing ancestor and require the resolved
        // path to stay under the canonical root. When the root itself does
        // not exist (host-less tests) there is nothing to resolve and the
        // lexical check above is the only guarantee. Residual TOCTOU (a
        // symlink swapped between check and open) is accepted as intra-VM
        // robustness only: pooled connections into ONE guest run concurrent
        // exec turns on the same workspace, so one connection's exec can race
        // another's fs op — the threat model is a sandbox confusing itself,
        // never a host escape (guest code already runs inside the VM).
        let canonical_root = match std::fs::canonicalize(root) {
            Ok(canonical) => canonical,
            Err(_) => return Ok(candidate),
        };
        let mut existing = candidate.clone();
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        let mut resolved = loop {
            match std::fs::canonicalize(&existing) {
                Ok(resolved) => break resolved,
                Err(_) => match existing.file_name() {
                    Some(name) => {
                        missing.push(name.to_os_string());
                        if !existing.pop() {
                            return Err(PolicyError::Escape);
                        }
                    }
                    None => return Err(PolicyError::Escape),
                },
            }
        };
        for part in missing.into_iter().rev() {
            resolved.push(part);
        }
        if resolved.starts_with(&canonical_root) {
            Ok(resolved)
        } else {
            Err(PolicyError::Escape)
        }
    }
}
