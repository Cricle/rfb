//! Strict localhost URL validation shared by every command that talks to the
//! forkd controller or a local web service.
//!
//! All production harnesses must reject anything but a loopback target so an
//! operator cannot accidentally point a destructive or benchmark command at a
//! remote host.

use crate::cli::error::{validation, CliError};

/// Validate that a base URL is an HTTP(S) loopback target
/// (`127.0.0.1`, `localhost`, or `::1`) and return it trimmed.
pub fn require_localhost(base_url: &str) -> Result<String, CliError> {
    let trimmed = base_url.trim().trim_end_matches('/').to_owned();
    let lower = trimmed.to_ascii_lowercase();
    let valid = lower.starts_with("http://") || lower.starts_with("https://");
    if !valid {
        return Err(validation(format!(
            "URL must use http:// or https://: {base_url}"
        )));
    }
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .unwrap_or_default();
    let host = rest
        .split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    if host != "127.0.0.1" && host != "localhost" && host != "[::1]" && host != "::1" {
        return Err(validation(format!(
            "URL must target localhost only: {base_url}"
        )));
    }
    Ok(trimmed)
}

/// Validate a snapshot tag against the forkd naming constraint.
pub fn require_snapshot_tag(tag: &str) -> Result<&str, CliError> {
    let ok = !tag.is_empty()
        && tag.len() <= 128
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(tag)
    } else {
        Err(validation("invalid snapshot tag"))
    }
}
