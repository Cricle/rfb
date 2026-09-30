//! Fail-closed verification of the Firecracker binary against a neighboring
//! `SHA256SUMS` manifest.
//!
//! The provider refuses to spawn a Firecracker binary whose digest is listed
//! in a `SHA256SUMS` manifest and does not match. A binary with no adjacent
//! manifest is accepted (`Ok(None)`): absence of a manifest is the normal
//! case for a system-installed Firecracker and is not an error, while a hash
//! mismatch is always fail-closed.
//!
//! The lookup walks at most three directory levels above the binary (so the
//! repository's `resx/SHA256SUMS` covers `resx/firecracker/<binary>`), and a
//! manifest line matches when its relative path's final component equals the
//! binary's basename.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// How many ancestor directories (starting at the binary's parent) are
/// searched for a `SHA256SUMS` manifest.
const MANIFEST_SEARCH_LEVELS: usize = 3;

/// Verify the SHA-256 of `path` against the nearest applicable `SHA256SUMS`
/// manifest.
///
/// Returns `Ok(Some(digest))` when a manifest lists this binary and the digest
/// matched, `Ok(None)` when no manifest that could apply lists the basename,
/// and `Err` when the binary cannot be read/hashed or a listed digest does
/// not match. Results are cached per process, keyed by (path, length, mtime).
pub fn verify_firecracker_binary(path: &Path) -> Result<Option<String>, String> {
    type Cache = Mutex<HashMap<PathBuf, (u64, u128, Result<Option<String>, String>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();

    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "firecracker binary {} is not readable: {error}",
            path.display()
        )
    })?;
    let stamp = (metadata.len(), mtime_nanos(&metadata));
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((len, mtime, result)) = guard.get(path) {
            if (*len, *mtime) == stamp {
                return result.clone();
            }
        }
    }
    let result = verify_uncached(path);
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.to_path_buf(), (stamp.0, stamp.1, result.clone()));
    result
}

fn verify_uncached(path: &Path) -> Result<Option<String>, String> {
    let Some(basename) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(format!(
            "firecracker path {} has no file name to match a manifest",
            path.display()
        ));
    };
    let mut dir = path.parent();
    for _ in 0..MANIFEST_SEARCH_LEVELS {
        let Some(current) = dir else { break };
        let manifest = current.join("SHA256SUMS");
        if manifest.is_file() {
            if let Some(expected) = manifest_entry(&manifest, basename)? {
                let actual = sha256_file(path)?;
                if actual != expected {
                    return Err(format!(
                        "firecracker sha256 mismatch for {}: manifest expects {expected}, binary hashes to {actual}",
                        path.display()
                    ));
                }
                return Ok(Some(expected));
            }
        }
        dir = current.parent();
    }
    Ok(None)
}

/// Read one manifest and return the digest listed for `basename`, if any.
fn manifest_entry(manifest: &Path, basename: &str) -> Result<Option<String>, String> {
    let content = std::fs::read_to_string(manifest)
        .map_err(|error| format!("read {}: {error}", manifest.display()))?;
    Ok(parse_manifest(&content, basename))
}

/// Parse `<64hex><whitespace>*<relpath>` lines (coreutils `sha256sum`
/// format, with the optional binary-mode `*`). Only a well-formed 64-hex
/// digest counts; the line matches when the relative path's final component
/// equals `basename`.
fn parse_manifest(content: &str, basename: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(digest) = parts.next() else { continue };
        let Some(relpath) = parts.next() else {
            continue;
        };
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let relpath = relpath.trim_start_matches('*');
        let entry_name = relpath.rsplit(['/', '\\']).next().unwrap_or(relpath);
        if entry_name == basename {
            return Some(digest.to_ascii_lowercase());
        }
    }
    None
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match std::io::Read::read(&mut file, &mut chunk) {
            Ok(0) => break,
            Ok(n) => hasher.update(&chunk[..n]),
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Nanosecond modification time, 0 when unavailable (cache stamp only).
fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or(0)
}
