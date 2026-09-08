//! Firecracker API socket identity and HTTP response parsing.

use anyhow::{bail, Result};
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::time::Duration;

pub(super) const FC_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct EndpointIdentity {
    dev: u64,
    ino: u64,
}

pub(super) fn endpoint_identity(path: &Path) -> Option<EndpointIdentity> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata
        .file_type()
        .is_socket()
        .then_some(EndpointIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
}

pub(super) fn remove_stale_socket(path: &Path) {
    if endpoint_identity(path).is_some() {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn remove_owned_socket(path: &Path, identity: EndpointIdentity) {
    if endpoint_identity(path) == Some(identity) {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn snapshot_file_ready(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file() && m.len() > 0)
        .unwrap_or(false)
}

pub(super) fn remove_snapshot_file(path: &Path) {
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
pub fn read_http_response(stream: &mut impl Read) -> Result<Vec<u8>> {
    const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
    const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1024;
    let mut response = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            bail!("Firecracker API closed connection before response headers");
        }
        response.extend_from_slice(&chunk[..n]);
        if response.len() > MAX_RESPONSE_HEADER_BYTES
            && !response.windows(4).any(|w| w == b"\r\n\r\n")
        {
            bail!("Firecracker API response headers exceed limit");
        }
        if response.len() > MAX_RESPONSE_BYTES {
            bail!("Firecracker API response exceeds limit");
        }
        if let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let headers = std::str::from_utf8(&response[..header_end])
        .map_err(|_| anyhow::anyhow!("Firecracker API response headers are not UTF-8"))?;
    let status_line = headers.split("\r\n").next().unwrap_or_default();
    if !status_line.starts_with("HTTP/1.") {
        bail!("invalid Firecracker API response status line");
    }
    let mut content_length = None;
    for line in headers.split("\r\n").skip(1) {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("malformed Firecracker API response header"))?;
        if name.trim().is_empty() || name.trim() != name {
            bail!("malformed Firecracker API response header");
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            && !value.trim().is_empty()
            && !value.trim().eq_ignore_ascii_case("identity")
        {
            bail!("unsupported Firecracker API transfer encoding");
        }
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| anyhow::anyhow!("invalid Firecracker API Content-Length"))?;
            if let Some(previous) = content_length {
                if previous != parsed {
                    bail!("conflicting Firecracker API Content-Length headers");
                }
            } else {
                content_length = Some(parsed);
            }
        }
    }
    let response_end = match content_length {
        Some(length) => header_end
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("Firecracker API response exceeds limit"))?,
        None => MAX_RESPONSE_BYTES,
    };
    if response_end > MAX_RESPONSE_BYTES {
        bail!("Firecracker API response exceeds limit");
    }
    while response.len() < response_end {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            if content_length.is_some() {
                bail!("Firecracker API closed connection before response body");
            }
            break;
        }
        response.extend_from_slice(&chunk[..n]);
        if response.len() > MAX_RESPONSE_BYTES {
            bail!("Firecracker API response exceeds limit");
        }
    }
    if content_length.is_some() {
        response.truncate(response_end);
    }
    Ok(response)
}

/// Parse a Firecracker API response, failing on a non-2xx status line.
pub fn parse_api_response(response: &[u8], method: &str, path: &str) -> Result<String> {
    let resp = String::from_utf8_lossy(response);
    let status_line = resp.lines().next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid Firecracker API response: {status_line}"))?;
    if !(200..300).contains(&status) {
        bail!(
            "Firecracker API error on {} {}: {}",
            method,
            path,
            status_line
        );
    }
    Ok(resp.into_owned())
}
