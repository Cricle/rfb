//! Strict localhost URL validation shared by every command that talks to the
//! forkd controller or a local web service.

use crate::cli::error::{validation, CliError};

/// Validate that a base URL is an HTTP(S) loopback target
/// (`127.0.0.1` or `localhost`) and return it trimmed.
///
/// Parses with `Url` so userinfo tricks like
/// `http://127.0.0.1:8889@attacker.com/` resolve to their real host.
pub fn require_localhost(base_url: &str) -> Result<String, CliError> {
    let trimmed = base_url.trim().trim_end_matches('/').to_owned();
    let parsed: url::Url = url::Url::parse(&trimmed).map_err(|e| {
        validation(format!(
            "URL must use http:// or https://: {base_url} ({e})"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(validation(format!(
            "URL must use http:// or https://: {base_url}"
        )));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if host != "127.0.0.1" && host != "localhost" {
        return Err(validation(format!(
            "URL must target localhost only: {base_url}"
        )));
    }
    // A base URL with a path would be concatenated into every request path
    // ("http://127.0.0.1:8889/x" + "/v1/snapshots") and silently 404.
    if !parsed.path().is_empty() && parsed.path() != "/" {
        return Err(validation(format!(
            "URL must not include a path: {base_url}"
        )));
    }
    Ok(trimmed)
}

/// Validate a snapshot tag against the forkd naming constraint. `.` and `..`
/// are rejected: tags are joined into the snapshots root as directory names,
/// so the dot entries would escape it.
pub fn require_snapshot_tag(tag: &str) -> Result<&str, CliError> {
    let ok = !tag.is_empty()
        && tag.len() <= 128
        && tag != "."
        && tag != ".."
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(tag)
    } else {
        Err(validation("invalid snapshot tag"))
    }
}
