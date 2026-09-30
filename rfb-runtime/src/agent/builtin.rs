//! Shell-free builtins (echo/true/false/netprobe) for the forkd agent. The
//! guest runs images without a shell, so these are handled in-process. The
//! definitions are shared with the ZBRT executor; this module adds the agent's
//! own workspace path check for a request that names a working directory.

use super::transport::guest_path;
use serde_json::Value;
use std::io;

pub use crate::builtin::{builtin, builtin_result};

/// Validate the argument, working-directory, and environment fields of a builtin request.
pub fn validate_builtin_request(request: &Value) -> io::Result<()> {
    crate::builtin::validate_builtin_request(request)?;
    if let Some(cwd) = request.get("cwd") {
        // Apply the same path policy when a cwd is explicitly supplied.
        let _ = guest_path(Some(cwd), true)?;
    }
    Ok(())
}
