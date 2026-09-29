use crate::cli::error::{io, validation, CliError};
use crate::cli::image_build::manifest::{sha256, ImageManifestWire};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

/// Metadata and file references describing a built artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    /// Artifact manifest schema identifier.
    pub schema: String,
    /// Backend that runs the artifact.
    pub backend: String,
    /// Backend profile name.
    pub profile: String,
    /// Artifact CPU architecture.
    pub arch: String,
    /// Artifact transport.
    pub transport: String,
    /// Artifact protocol.
    pub protocol: String,
    /// Guest entrypoint.
    pub entrypoint: String,
    /// Guest port used by the backend.
    pub guest_port: u16,
    /// Optional kernel file reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<ArtifactFile>,
    /// Optional root filesystem file reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<ArtifactFile>,
    /// Optional Firecracker tool reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firecracker: Option<ArtifactTool>,
    /// Virtual machine resource configuration.
    pub vm: ArtifactVm,
    /// Optional snapshot reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ArtifactSnapshot>,
}
/// A file path and its SHA-256 digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFile {
    /// Path to the artifact file.
    pub path: String,
    /// SHA-256 digest, optionally prefixed with `sha256:`.
    pub sha256: String,
}
/// A tool path, version, and SHA-256 digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTool {
    /// Path to the tool binary.
    pub path: String,
    /// Tool version.
    pub version: String,
    /// SHA-256 digest of the tool.
    pub sha256: String,
}
/// Virtual machine resource configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactVm {
    /// Number of virtual CPUs.
    pub cpus: u16,
    /// Guest memory size in bytes.
    pub memory_bytes: u64,
}
/// A snapshot tag, digest, and network setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSnapshot {
    /// Snapshot tag.
    pub tag: String,
    /// SHA-256 digest of the snapshot descriptor/manifest.
    pub sha256: String,
    /// Independent SHA-256 digest of the guest memory image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_sha256: Option<String>,
    /// Independent SHA-256 digest of the VM state image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmstate_sha256: Option<String>,
    /// Whether networking is enabled.
    pub network: bool,
    /// Capture batch identity, when supplied by the snapshot producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// Controller/runtime version used for capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Stable VM configuration identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_identity: Option<String>,
    /// Stable network configuration identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_identity: Option<String>,
}

/// Errors describing an artifact mismatch.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactMismatch {
    /// A declared artifact value differs from the observed value.
    #[error("artifact mismatch: {path} ({kind}), expected {expected}, observed {observed}; next: {next}")]
    Mismatch {
        /// Expected value.
        expected: String,
        /// Observed value.
        observed: String,
        /// Artifact path.
        path: String,
        /// Mismatch category.
        kind: String,
        /// Suggested next action.
        next: String,
    },
}

/// Return whether `value` is a valid 64-digit hexadecimal SHA-256 digest.
pub fn valid_sha(value: &str) -> bool {
    let v = value.strip_prefix("sha256:").unwrap_or(value);
    v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())
}
fn check_file(path: &str, digest: &str, label: &str, require_local: bool) -> Result<(), CliError> {
    if path.trim().is_empty() {
        return Err(validation(format!("{label} path must not be empty")));
    }
    if !valid_sha(digest) {
        return Err(validation(format!("invalid sha256 for {label}")));
    }
    let p = Path::new(path);
    if !p.exists() {
        if require_local {
            return Err(validation(format!(
                "{label} local file is missing: {}",
                p.display()
            )));
        }
        return Ok(());
    }
    if !p.is_file() {
        return Err(validation(format!("{label} is not a regular file")));
    }
    let got = sha256(p).map_err(|e| io(e.to_string()))?;
    let expected = digest.strip_prefix("sha256:").unwrap_or(digest);
    if got != expected.to_ascii_lowercase() {
        return Err(validation(format!("artifact mismatch: {label} digest")));
    }
    Ok(())
}
impl ArtifactManifest {
    /// Construct a manifest for a kernel artifact.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn for_kernel(path: &Path, digest: &str) -> Result<Self, CliError> {
        Ok(Self {
            schema: "rfb-artifact/v1".into(),
            backend: "rfb1".into(),
            profile: "rfb1-kernel".into(),
            arch: "x86_64".into(),
            transport: "vsock".into(),
            protocol: "rfb1".into(),
            entrypoint: "/sbin/rfb-runtime".into(),
            guest_port: 5000,
            kernel: Some(ArtifactFile {
                path: path.to_string_lossy().into(),
                sha256: digest.into(),
            }),
            rootfs: None,
            firecracker: None,
            vm: ArtifactVm {
                cpus: 1,
                memory_bytes: 0,
            },
            snapshot: None,
        })
    }

    /// Construct a manifest for a statically packaged runtime.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn for_static_runtime(
        path: &Path,
        digest: &str,
        package: &str,
        target: &str,
    ) -> Result<Self, CliError> {
        let (backend, profile, transport, protocol, entrypoint, guest_port) = match package {
            "forkd-agent" => (
                "forkd",
                "forkd-agent",
                "tcp",
                "forkd",
                "/forkd-init.sh",
                8888,
            ),
            "zeroboot-runtime" | "zeroboot-zbrt" => (
                "zeroboot",
                "zeroboot-zbrt",
                "virtio-vsock",
                "zbrt",
                "/init",
                5000,
            ),
            _ => (
                "rfb1",
                "rfb-runtime",
                "vsock",
                "rfb1",
                "/sbin/rfb-runtime",
                5000,
            ),
        };
        Ok(Self {
            schema: "rfb-artifact/v1".into(),
            backend: backend.into(),
            profile: format!("{profile}-{target}"),
            arch: "x86_64".into(),
            transport: transport.into(),
            protocol: protocol.into(),
            entrypoint: entrypoint.into(),
            guest_port,
            kernel: None,
            rootfs: Some(ArtifactFile {
                path: path.to_string_lossy().into(),
                sha256: digest.into(),
            }),
            firecracker: None,
            vm: ArtifactVm {
                cpus: 1,
                memory_bytes: 0,
            },
            snapshot: None,
        })
    }

    /// Construct a manifest for a root filesystem artifact.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn for_rootfs(
        output: &Path,
        digest: &str,
        mode: &str,
        entrypoint: &str,
        protocol: &str,
    ) -> Result<Self, CliError> {
        let (backend, profile, transport) = match mode {
            "forkd-agent" => ("forkd", "forkd-agent-tcp", "tcp"),
            "rfb-vsock" => ("rfb1", "rfb-runtime-rfb1-vsock", "vsock"),
            "zeroboot-zbrt" => ("zeroboot", "zeroboot-zbrt", "virtio-vsock"),
            _ => return Err(validation("unsupported rootfs artifact mode")),
        };
        Ok(Self {
            schema: "rfb-artifact/v1".into(),
            backend: backend.into(),
            profile: profile.into(),
            arch: "x86_64".into(),
            transport: transport.into(),
            protocol: protocol.into(),
            entrypoint: entrypoint.into(),
            guest_port: if backend == "forkd" { 8888 } else { 5000 },
            kernel: None,
            rootfs: Some(ArtifactFile {
                path: output.to_string_lossy().into(),
                sha256: digest.into(),
            }),
            firecracker: None,
            vm: ArtifactVm {
                cpus: 1,
                // A rootfs artifact has no VM memory to report. Setting
                // memory_bytes to 0 makes the provenance identity skip the
                // memory comparison, so --require-provenance does not
                // permanently reject CLI-built rootfs artifacts.
                memory_bytes: 0,
            },
            snapshot: None,
        })
    }

    /// Schema-level validation: structure, digest format, and non-empty
    /// required fields. Suitable for custom/opaque image manifests.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn for_image(
        path: &Path,
        digest: &str,
        image: &ImageManifestWire,
    ) -> Result<Self, CliError> {
        let (backend, transport, protocol, port) =
            match (image.transport.as_str(), image.protocol.as_str()) {
                ("tcp", "forkd") => ("forkd", "tcp", "forkd", 8888),
                ("vsock", "rfb1") => ("rfb1", "vsock", "rfb1", 5000),
                ("virtio-vsock", "zbrt") => ("zeroboot", "virtio-vsock", "zbrt", 5000),
                _ => (
                    "rfb1",
                    image.transport.as_str(),
                    image.protocol.as_str(),
                    5000,
                ),
            };
        Ok(Self {
            schema: "rfb-artifact/v1".into(),
            backend: backend.into(),
            profile: image.profile.clone().unwrap_or_else(|| "custom".into()),
            arch: image.arch.clone(),
            transport: transport.into(),
            protocol: protocol.into(),
            entrypoint: image.entrypoint.first().cloned().unwrap_or_default(),
            guest_port: port,
            kernel: None,
            rootfs: Some(ArtifactFile {
                path: path.to_string_lossy().into(),
                sha256: digest.into(),
            }),
            firecracker: None,
            vm: ArtifactVm {
                cpus: image.resources.cpus.unwrap_or(1) as u16,
                memory_bytes: image.resources.memory_bytes.unwrap_or(0),
            },
            snapshot: None,
        })
    }

    /// Return the stable, human-readable artifact kind for this manifest.
    pub fn kind(&self) -> &'static str {
        if self.snapshot.is_some() {
            "snapshot"
        } else if self.firecracker.is_some() {
            "firecracker"
        } else if self.kernel.is_some() {
            "kernel"
        } else if self.rootfs.is_some() {
            "rootfs"
        } else {
            "unknown"
        }
    }

    /// Validate only portable manifest structure and contract metadata.
    /// Local paths are intentionally optional for manifests moved between hosts.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate_schema(&self) -> Result<(), CliError> {
        if self.schema != "rfb-artifact/v1" {
            return Err(validation("unsupported artifact schema"));
        }
        if !["forkd", "rfb1", "zeroboot"].contains(&self.backend.as_str()) {
            return Err(validation("unsupported artifact backend"));
        }
        if self.guest_port == 0 {
            return Err(validation("guest_port must be non-zero"));
        }
        if let Some(kernel) = &self.kernel {
            check_file(&kernel.path, &kernel.sha256, "kernel", false)?;
        }
        if let Some(rootfs) = &self.rootfs {
            check_file(&rootfs.path, &rootfs.sha256, "rootfs", false)?;
        }
        if let Some(firecracker) = &self.firecracker {
            check_file(&firecracker.path, &firecracker.sha256, "firecracker", false)?;
        }
        if let Some(s) = &self.snapshot {
            if s.tag.trim().is_empty() {
                return Err(validation("snapshot tag must not be empty"));
            }
            if !valid_sha(&s.sha256) {
                return Err(validation("invalid snapshot sha256"));
            }
            for (name, value) in [
                ("snapshot memory", s.memory_sha256.as_deref()),
                ("snapshot vmstate", s.vmstate_sha256.as_deref()),
            ] {
                if let Some(value) = value {
                    if !valid_sha(value) {
                        return Err(validation(format!("invalid {name} sha256")));
                    }
                }
            }
            for (name, value) in [
                ("snapshot batch_id", s.batch_id.as_deref()),
                ("snapshot version", s.version.as_deref()),
                ("snapshot vm_identity", s.vm_identity.as_deref()),
                ("snapshot network_identity", s.network_identity.as_deref()),
            ] {
                if value.is_some_and(|v| v.trim().is_empty()) {
                    return Err(validation(format!("{name} must not be empty")));
                }
            }
        }
        Ok(())
    }

    /// Verify every declared local file and reject missing paths.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn verify_local_files(&self) -> Result<(), CliError> {
        if let Some(kernel) = &self.kernel {
            check_file(&kernel.path, &kernel.sha256, "kernel", true)?;
        }
        if let Some(rootfs) = &self.rootfs {
            check_file(&rootfs.path, &rootfs.sha256, "rootfs", false)?;
        }
        if let Some(firecracker) = &self.firecracker {
            check_file(&firecracker.path, &firecracker.sha256, "firecracker", true)?;
        }
        Ok(())
    }

    /// Full contract validation on top of `validate_schema`: the backend must
    /// match its declared transport/protocol/guest port.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn validate(&self) -> Result<(), CliError> {
        self.validate_schema()?;
        if !matches!(
            self.transport.as_str(),
            "tcp" | "vsock" | "virtio-vsock" | "oci"
        ) {
            return Err(validation(format!(
                "unsupported artifact transport: {}",
                self.transport
            )));
        }
        if self.backend == "forkd" && (self.transport != "tcp" || self.protocol != "forkd") {
            return Err(validation("forkd requires tcp/forkd"));
        }
        if self.backend == "rfb1" && (self.transport != "vsock" || self.protocol != "rfb1") {
            return Err(validation("rfb1 requires vsock/rfb1"));
        }
        if self.profile == "rfb-runtime-rfb1-vsock"
            && (self.transport != "vsock"
                || self.protocol != "rfb1"
                || self.entrypoint != "/sbin/rfb-runtime"
                || self.guest_port != 5000)
        {
            return Err(validation(
                "rfb-runtime-rfb1-vsock requires vsock/rfb1, /sbin/rfb-runtime, guest_port 5000",
            ));
        }
        if self.backend == "zeroboot"
            && (self.transport != "virtio-vsock" || self.protocol != "zbrt")
        {
            return Err(validation("zeroboot requires virtio-vsock/zbrt"));
        }
        let expected_port = if self.backend == "forkd" { 8888 } else { 5000 };
        if self.guest_port != expected_port {
            return Err(validation(format!(
                "{} guest_port must be {expected_port}",
                self.backend
            )));
        }
        if self.entrypoint.trim().is_empty() {
            return Err(validation("entrypoint must not be empty"));
        }
        if let Some(kernel) = &self.kernel {
            check_file(&kernel.path, &kernel.sha256, "kernel", true)?;
        }
        if let Some(rootfs) = &self.rootfs {
            check_file(&rootfs.path, &rootfs.sha256, "rootfs", false)?;
        }
        if let Some(firecracker) = &self.firecracker {
            check_file(&firecracker.path, &firecracker.sha256, "firecracker", false)?;
        }
        if let Some(s) = &self.snapshot {
            if s.tag.trim().is_empty() {
                return Err(validation("snapshot tag must not be empty"));
            }
            if !valid_sha(&s.sha256) {
                return Err(validation("invalid snapshot sha256"));
            }
            for (name, value) in [
                ("snapshot memory", s.memory_sha256.as_deref()),
                ("snapshot vmstate", s.vmstate_sha256.as_deref()),
            ] {
                if let Some(value) = value {
                    if !valid_sha(value) {
                        return Err(validation(format!("invalid {name} sha256")));
                    }
                }
            }
            for (name, value) in [
                ("snapshot batch_id", s.batch_id.as_deref()),
                ("snapshot version", s.version.as_deref()),
                ("snapshot vm_identity", s.vm_identity.as_deref()),
                ("snapshot network_identity", s.network_identity.as_deref()),
            ] {
                if value.is_some_and(|v| v.trim().is_empty()) {
                    return Err(validation(format!("{name} must not be empty")));
                }
            }
        }
        Ok(())
    }
    /// Load and schema-validate a manifest from a JSON file.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn load(path: &Path) -> Result<Self, CliError> {
        let text = fs::read_to_string(path).map_err(|e| io(e.to_string()))?;
        let m: Self = serde_json::from_str(&text)
            .map_err(|e| validation(format!("invalid artifact manifest: {e}")))?;
        m.validate_schema()?;
        Ok(m)
    }

    /// Load a manifest and perform full local validation.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn load_local(path: &Path) -> Result<Self, CliError> {
        let manifest = Self::load(path)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Write this manifest beside `artifact` and return the sidecar path.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn write_sidecar(&self, artifact: &Path) -> Result<PathBuf, CliError> {
        let p = PathBuf::from(format!("{}.artifact.json", artifact.display()));
        let data = serde_json::to_string_pretty(self).map_err(|e| io(e.to_string()))? + "\n";
        let parent = p
            .parent()
            .filter(|directory| !directory.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temp = NamedTempFile::new_in(parent).map_err(|e| io(e.to_string()))?;
        temp.write_all(data.as_bytes())
            .map_err(|e| io(e.to_string()))?;
        temp.as_file().sync_all().map_err(|e| io(e.to_string()))?;
        temp.persist(&p).map_err(|e| io(e.error.to_string()))?;
        Ok(p)
    }
}
