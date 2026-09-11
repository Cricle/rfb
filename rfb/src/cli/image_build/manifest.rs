//! Staging-manifest parsing, validation, and hashing for `rfb-cli image`.

use crate::cli::error::{io, validation, CliError};
use crate::core::{ImageManifest, Resources};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

/// Well-known `forkd-agent` integration profile name.
pub const FORKD_PROFILE: &str = "forkd-agent";
/// Transport required for forkd-agent images.
pub const FORKD_TRANSPORT: &str = "tcp";
/// Protocol required for forkd-agent images.
pub const FORKD_PROTOCOL: &str = "forkd";

/// The CLI staging manifest (kept wire-stable; converts to the core contract).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct StagingManifest {
    /// Manifest schema name, e.g. `rfb-cli-staging/v1`.
    pub format: String,
    /// Total image size in bytes (must be a multiple of `block_size`).
    pub image_size_bytes: u64,
    /// Filesystem block size in bytes used for the image layout.
    pub block_size: u64,
    /// Path to the staging directory holding the files to pack.
    pub staging: String,
    /// Staged files packed into the image, in order.
    #[serde(default)]
    pub files: Vec<StagedFile>,
    /// Optional embedded image manifest describing the boot contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageManifestWire>,
}

/// One file staged into the image, pinned by size and SHA-256.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct StagedFile {
    /// Relative path of the file inside the staging directory/image.
    pub path: String,
    /// Size of the file in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 digest of the file contents.
    pub sha256: String,
    /// Origin of the file (used for provenance and reporting).
    pub source: String,
}

/// A wire representation keeps the CLI format stable while converting to the
/// platform-neutral core contract in one place.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ImageManifestWire {
    /// Image reference, e.g. a tag or `path#tag`.
    pub image_ref: String,
    /// Optional image digest, when a concrete build was pinned.
    #[serde(default)]
    pub digest: Option<String>,
    /// Guest transport, e.g. `tcp` or `vsock`.
    pub transport: String,
    /// Command line used to launch the agent inside the guest.
    pub entrypoint: Vec<String>,
    /// Wire protocol spoken by the agent, e.g. `forkd` or `rfb1`.
    pub protocol: String,
    /// Target CPU architecture, e.g. `x86_64`.
    pub arch: String,
    /// Declared VM resource requirements.
    #[serde(default)]
    pub resources: Resources,
    /// Optional named integration profile; profile validation is explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Declared guest-facing capabilities (diagnostic metadata only).
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl ImageManifestWire {
    /// Convert to the core contract, applying profile validation.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn into_core(self) -> Result<ImageManifest, CliError> {
        let profile = self.profile.clone();
        if self.transport.trim().is_empty() {
            return Err(validation("image transport must not be empty"));
        }
        if !matches!(
            self.transport.as_str(),
            "tcp" | "vsock" | "virtio-vsock" | "oci"
        ) {
            return Err(validation(format!(
                "unsupported image transport: {}",
                self.transport
            )));
        }
        let image = ImageManifest {
            image_ref: self.image_ref,
            digest: self.digest,
            transport: self.transport,
            entrypoint: self.entrypoint,
            protocol: self.protocol,
            arch: self.arch,
            resources: self.resources,
        };
        image
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        if let Some(profile) = profile.as_deref() {
            if profile == FORKD_PROFILE {
                validate_forkd_agent_manifest(&image, &self.capabilities)?;
            } else {
                crate::image_profiles::validate_image_manifest_profile(&image, profile)
                    .map_err(validation)?;
            }
        }
        Ok(image)
    }
}

fn validate_forkd_agent_manifest(
    image: &ImageManifest,
    capabilities: &[String],
) -> Result<(), CliError> {
    if image.image_ref.contains("rfb-runtime")
        || image
            .entrypoint
            .iter()
            .any(|entry| entry.contains("rfb-runtime"))
        || (image.transport == "vsock" && image.protocol == "rfb1")
    {
        return Err(validation(
            "rfb-runtime vsock image cannot be used as forkd-agent",
        ));
    }
    if image.transport != FORKD_TRANSPORT {
        return Err(validation(format!(
            "forkd-agent transport must be {FORKD_TRANSPORT}, got {}",
            image.transport
        )));
    }
    if image.protocol != FORKD_PROTOCOL {
        return Err(validation(format!(
            "forkd-agent protocol must be {FORKD_PROTOCOL}, got {}",
            image.protocol
        )));
    }
    if image.entrypoint.is_empty() || image.entrypoint[0].trim().is_empty() {
        return Err(validation("forkd-agent requires a non-empty entrypoint"));
    }
    if capabilities.is_empty() {
        return Err(validation("forkd-agent requires explicit capabilities"));
    }
    let mut seen = HashSet::new();
    for capability in capabilities {
        let capability = capability.trim();
        if capability.is_empty()
            || !matches!(
                capability,
                "execute"
                    | "health"
                    | "stream"
                    | "ls"
                    | "find"
                    | "grep"
                    | "read_file"
                    | "write_file"
            )
        {
            return Err(validation(format!(
                "forkd-agent has unsupported capability: {capability:?}"
            )));
        }
        if !seen.insert(capability) {
            return Err(validation(format!(
                "forkd-agent capability is duplicated: {capability}"
            )));
        }
    }
    Ok(())
}

impl StagingManifest {
    /// Convert the embedded image manifest (if present).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn core_image(&self) -> Result<Option<ImageManifest>, CliError> {
        self.image
            .clone()
            .map(ImageManifestWire::into_core)
            .transpose()
    }
}

/// Load and structurally validate a staging manifest.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn load(path: &Path) -> Result<StagingManifest, CliError> {
    let contents = fs::read_to_string(path)
        .map_err(|error| io(format!("read {}: {error}", path.display())))?;
    serde_json::from_str(&contents)
        .map_err(|error| validation(format!("invalid staging manifest: {error}")))
}

/// Validate a staging manifest in-place (files, paths, hashes).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn validate(path: &Path) -> Result<StagingManifest, CliError> {
    let manifest = load(path)?;
    if manifest.format != "rfb-manifest/v1" && manifest.format != "rfb-cli-staging/v1" {
        return Err(validation("unsupported staging manifest format"));
    }
    if manifest.image_size_bytes == 0
        || manifest.block_size == 0
        || !manifest
            .image_size_bytes
            .is_multiple_of(manifest.block_size)
    {
        return Err(validation(
            "image_size_bytes must be a non-zero multiple of block_size",
        ));
    }
    let _ = manifest.core_image()?;
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    let staging = safe_join(root, &manifest.staging)?;
    if !staging.is_dir() {
        return Err(validation(format!(
            "staging directory missing: {}",
            staging.display()
        )));
    }
    let staging_root = fs::canonicalize(&staging)
        .map_err(|error| io(format!("canonicalize staging directory: {error}")))?;
    let mut paths = HashSet::new();
    for file in &manifest.files {
        let normalized = normalize_relative_path(&file.path)?;
        if !paths.insert(normalized.clone()) {
            return Err(validation(format!("duplicate file path: {}", file.path)));
        }
        let staged = safe_join(&staging, &normalized)?;
        let canonical = fs::canonicalize(&staged)
            .map_err(|_| validation(format!("staged file missing: {}", file.path)))?;
        if !canonical.starts_with(&staging_root) || !canonical.is_file() {
            return Err(validation(format!("unsafe file path: {}", file.path)));
        }
        let metadata =
            fs::metadata(&canonical).map_err(|error| io(format!("stat {}: {error}", file.path)))?;
        if metadata.len() != file.size {
            return Err(validation(format!("size mismatch: {}", file.path)));
        }
        if !is_sha256(&file.sha256) {
            return Err(validation(format!("invalid sha256: {}", file.path)));
        }
        if sha256(&canonical).map_err(|error| io(error.to_string()))?
            != file.sha256.to_ascii_lowercase()
        {
            return Err(validation(format!("sha256 mismatch: {}", file.path)));
        }
    }
    Ok(manifest)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize_relative_path(relative: &str) -> Result<String, CliError> {
    let candidate = Path::new(relative);
    if relative.is_empty()
        || relative.as_bytes().get(1) == Some(&b':')
        || relative.contains('\\')
        || candidate.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(validation(format!("unsafe file path: {relative}")));
    }
    let normalized = candidate
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if normalized != relative {
        return Err(validation(format!("non-canonical file path: {relative}")));
    }
    Ok(normalized)
}

/// Join a root with a relative path, rejecting any traversal.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, CliError> {
    let candidate = Path::new(relative);
    if relative.is_empty()
        || relative.contains('\0')
        || relative.contains('\\')
        || candidate.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(validation(format!("unsafe file path: {relative}")));
    }
    Ok(root.join(candidate))
}

/// Compute the SHA-256 of a file.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn sha256(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(hasher.finalize()))
}

/// Compute the SHA-256 of a byte slice.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(hasher.finalize())
}

/// Lowercase hex encoding; digest 0.11 outputs no longer implement `LowerHex`.
pub(crate) fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}
