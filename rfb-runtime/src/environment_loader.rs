//! Image-declared environment loading from `/etc/rfb-runtime/environment`.

use crate::config::RuntimeConfig;
use std::io;

/// Load the image-declared runtime environment from the configured path
/// (`/etc/rfb-runtime/environment` by default, e.g.
/// `RFB_RUNTIME_EXECUTOR=workspace`, `RFB_RUNTIME_WORKSPACE=/workspace`).
/// Existing process environment variables take precedence, so an operator can
/// still override the image contract. Missing/empty file is not an error:
/// forkd and stdio modes do not require it.
pub fn load_image_environment() -> io::Result<()> {
    load_image_environment_from(&RuntimeConfig::from_environment().environment_path)
}

/// Load the image-declared environment from an explicit path.
pub fn load_image_environment_from(path: &std::path::Path) -> io::Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() && std::env::var_os(key).is_none() {
                std::env::set_var(key, value);
            }
        }
    }
    Ok(())
}
