//! Scoped artifact discovery/deletion for rfb-runtime trees.
//!
//! This replaces `cleanup-real.sh`: destructive cleanup is bound to an
//! `rfb-runtime` path, requires confirmation, and by default only lists what
//! would be deleted. It never touches anything outside the target directory's
//! first level of files.

use crate::cli::error::{usage, validation, CliError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Artifact suffixes considered safe to clean in an rfb-runtime tree.
pub const CLEANABLE_PATTERNS: [&str; 3] = [".ext4", ".sha256", ".manifest.json"];

/// Validate a cleanup target: absolute, under an `rfb-runtime` path, and not
/// the filesystem root or the user's home directory.
pub fn validate_target(target: &Path) -> Result<(), CliError> {
    if !target.is_absolute() {
        return Err(usage("cleanup target must be an absolute path"));
    }
    if target.as_os_str().is_empty() || target.parent().is_none() {
        return Err(usage("refusing unsafe cleanup target"));
    }
    if let Ok(home) = std::env::var("HOME") {
        if target.starts_with(&home) && target == Path::new(&home) {
            return Err(usage("refusing to clean the home directory"));
        }
    }
    // Target must be the rfb-runtime directory itself or a directory inside it.
    let text = target.to_string_lossy().replace('\\', "/");
    if !target.is_dir() {
        return Err(usage("cleanup target is not a directory"));
    }
    if !(text.ends_with("/rfb-runtime") || text.contains("/rfb-runtime/")) {
        return Err(usage(
            "cleanup target must be inside the rfb-runtime directory",
        ));
    }
    Ok(())
}

/// Discover cleanable artifacts (top-level files only) in a validated target.
pub fn discover(target: &Path) -> Result<Vec<PathBuf>, CliError> {
    validate_target(target)?;
    let mut found = Vec::new();
    for entry in fs_read_dir(target)? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.to_string_lossy().to_lowercase();
        if CLEANABLE_PATTERNS
            .iter()
            .any(|pattern| name.ends_with(pattern))
        {
            found.push(path);
        }
    }
    Ok(found)
}

fn fs_read_dir(path: &Path) -> Result<Vec<std::fs::DirEntry>, CliError> {
    let entries = std::fs::read_dir(path)
        .map_err(|error| validation(format!("read {}: {error}", path.display())))?;
    entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| validation(format!("read {}: {error}", path.display())))
}

/// Run a scoped cleanup. `dry_run` only lists; otherwise deletes discovered
/// artifacts. Returns a JSON summary of what was found/deleted.
pub fn cleanup(target: &Path, dry_run: bool, yes: bool) -> Result<Value, CliError> {
    let discovered = discover(target)?;
    if !dry_run && !yes {
        return Err(usage("destructive cleanup requires --yes (or --dry-run)"));
    }
    let mut deleted = Vec::new();
    if !dry_run {
        for path in &discovered {
            std::fs::remove_file(path)
                .map_err(|error| validation(format!("rm {}: {error}", path.display())))?;
            deleted.push(path.clone());
        }
    }
    Ok(json!({
        "mode": if dry_run { "dry_run" } else { "real" },
        "target": target,
        "found": discovered.len(),
        "artifacts": discovered.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        "deleted": deleted.len(),
    }))
}
