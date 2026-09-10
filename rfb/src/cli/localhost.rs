use crate::cli::error::{validation, CliError};

/// Validate that a base URL is an HTTP(S) loopback target
/// (`127.0.0.1`, `localhost`, or `::1`) and return it trimmed.
///
/// Parses with `Url` to defeat userinfo tricks like
/// `http://127.0.0.1:8889@attacker.com/`.
pub fn require_localhost(base_url: &str) -> Result<String, CliError> {
    let trimmed = base_url.trim().trim_end_matches('/').to_owned();
    let parsed: url::Url = url::Url::parse(&trimmed).map_err(|e| {
        validation(format!(
            "URL must use http:// or https://: {base_url} ({e})"
        ))
    })?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
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
