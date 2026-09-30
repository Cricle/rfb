//! Firecracker API socket identity and HTTP response parsing.

use super::FirecrackerError;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub(crate) const FC_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EndpointIdentity {
    dev: u64,
    ino: u64,
}

pub(crate) fn endpoint_identity(path: &Path) -> Option<EndpointIdentity> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata
        .file_type()
        .is_socket()
        .then_some(EndpointIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
}

pub(crate) fn remove_stale_socket(path: &Path) {
    if endpoint_identity(path).is_some() {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn remove_owned_socket(path: &Path, identity: EndpointIdentity) {
    if endpoint_identity(path) == Some(identity) {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn snapshot_file_ready(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file() && m.len() > 0)
        .unwrap_or(false)
}

pub(crate) fn remove_snapshot_file(path: &Path) {
    // Remove stale files and symlinks, but never recurse into a directory.
    if std::fs::symlink_metadata(path)
        .map(|m| !m.file_type().is_dir())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(path);
    }
}

/// Classify an API socket readiness failure: timeout or early process exit.
pub fn readiness_error(
    elapsed: Duration,
    child_exited: Option<std::process::ExitStatus>,
) -> Option<&'static str> {
    if elapsed > FC_SOCKET_TIMEOUT {
        Some("Firecracker API socket did not become ready")
    } else if child_exited.is_some() {
        Some("Firecracker exited before API became ready")
    } else {
        None
    }
}

/// Read a complete HTTP response (headers + content-length body) from the
/// Firecracker API socket with hard size limits.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn read_http_response(stream: &mut impl Read) -> Result<Vec<u8>, FirecrackerError> {
    const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
    const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1024;
    let mut response = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(FirecrackerError::Protocol(
                "Firecracker API closed connection before response headers".into(),
            ));
        }
        response.extend_from_slice(&chunk[..n]);
        if response.len() > MAX_RESPONSE_HEADER_BYTES
            && !response.windows(4).any(|w| w == b"\r\n\r\n")
        {
            return Err(FirecrackerError::Protocol(
                "Firecracker API response headers exceed limit".into(),
            ));
        }
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(FirecrackerError::Protocol(
                "Firecracker API response exceeds limit".into(),
            ));
        }
        if let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let headers = std::str::from_utf8(&response[..header_end]).map_err(|_| {
        FirecrackerError::Protocol("Firecracker API response headers are not UTF-8".into())
    })?;
    let status_line = headers.split("\r\n").next().unwrap_or_default();
    if !status_line.starts_with("HTTP/1.") {
        return Err(FirecrackerError::Protocol(
            "invalid Firecracker API response status line".into(),
        ));
    }
    let mut content_length = None;
    for line in headers.split("\r\n").skip(1) {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            FirecrackerError::Protocol("malformed Firecracker API response header".into())
        })?;
        if name.trim().is_empty() || name.trim() != name {
            return Err(FirecrackerError::Protocol(
                "malformed Firecracker API response header".into(),
            ));
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            && !value.trim().is_empty()
            && !value.trim().eq_ignore_ascii_case("identity")
        {
            return Err(FirecrackerError::Protocol(
                "unsupported Firecracker API transfer encoding".into(),
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value.trim().parse::<usize>().map_err(|_| {
                FirecrackerError::Protocol("invalid Firecracker API Content-Length".into())
            })?;
            if let Some(previous) = content_length {
                if previous != parsed {
                    return Err(FirecrackerError::Protocol(
                        "conflicting Firecracker API Content-Length headers".into(),
                    ));
                }
            } else {
                content_length = Some(parsed);
            }
        }
    }
    let response_end = match content_length {
        Some(length) => header_end.checked_add(length).ok_or_else(|| {
            FirecrackerError::Protocol("Firecracker API response exceeds limit".into())
        })?,
        None => MAX_RESPONSE_BYTES,
    };
    if response_end > MAX_RESPONSE_BYTES {
        return Err(FirecrackerError::Protocol(
            "Firecracker API response exceeds limit".into(),
        ));
    }
    while response.len() < response_end {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            if content_length.is_some() {
                return Err(FirecrackerError::Protocol(
                    "Firecracker API closed connection before response body".into(),
                ));
            }
            break;
        }
        response.extend_from_slice(&chunk[..n]);
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(FirecrackerError::Protocol(
                "Firecracker API response exceeds limit".into(),
            ));
        }
    }
    if content_length.is_some() {
        response.truncate(response_end);
    }
    Ok(response)
}

/// Parse a Firecracker API response, failing on a non-2xx status line.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn parse_api_response(
    response: &[u8],
    method: &str,
    path: &str,
) -> Result<String, FirecrackerError> {
    let resp = String::from_utf8_lossy(response);
    let status_line = resp.lines().next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            FirecrackerError::Protocol(format!("invalid Firecracker API response: {status_line}"))
        })?;
    if !(200..300).contains(&status) {
        return Err(FirecrackerError::Protocol(format!(
            "Firecracker API error on {} {}: {}",
            method, path, status_line
        )));
    }
    Ok(resp.into_owned())
}

/// Parse an HTTP/1.1 response head into `(status, content_length)`.
///
/// The status code is read from the status line only — never matched by
/// substring — so a `Content-Length: 2048` header cannot be mistaken for a
/// `204 OK` status.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn parse_response_head(headers: &str) -> Result<(u16, usize), FirecrackerError> {
    let status_line = headers.lines().next().ok_or_else(|| {
        FirecrackerError::Protocol("Firecracker returned an empty response".into())
    })?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(FirecrackerError::Protocol(format!(
            "Firecracker returned a non-HTTP/1.x status line: {status_line:?}"
        )));
    }
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| {
            FirecrackerError::Protocol(format!(
                "Firecracker returned an unparsable status line: {status_line:?}"
            ))
        })?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    Ok((status, content_length))
}

/// Read one complete HTTP/1.1 response from `stream` and return
/// `(status_code, full_response_text)`.
///
/// Built on [`read_http_response`]: the response is read with
/// `Content-Length` framing under hard size limits, then the status code is
/// parsed from the status line only — never substring-matched.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn read_response(stream: &mut UnixStream) -> Result<(u16, String), FirecrackerError> {
    let response = read_http_response(stream)?;
    let text = String::from_utf8_lossy(&response).into_owned();
    let (status, _content_length) = parse_response_head(&text)?;
    Ok((status, text))
}
