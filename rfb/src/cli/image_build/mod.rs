//! Image building and verification helpers used by `rfb-cli image`.
//!
//! This is the single place that owns staging-manifest parsing, ext4
//! creation, debugfs injection/verification, and rootfs asset validation. The
//! CLI is a thin router over these functions so every path gets identical
//! validation and error codes.

mod artifact;
mod build;
mod manifest;
mod verify;

pub use artifact::{
    valid_sha, ArtifactFile, ArtifactManifest, ArtifactMismatch, ArtifactSnapshot, ArtifactTool,
    ArtifactVm,
};
pub use build::{build, build_rootfs, image_diagnostics, init, profile_summary};
pub use manifest::{
    load, safe_join, sha256, sha256_bytes, validate, ImageManifestWire, StagedFile,
    StagingManifest, FORKD_PROFILE, FORKD_PROTOCOL, FORKD_TRANSPORT,
};
pub use verify::{build_static_runtime, check_kernel, inspect_rootfs, run_debugfs};
