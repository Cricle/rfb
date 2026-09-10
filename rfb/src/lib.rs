//! RFB contracts and optional backend integrations.

pub mod client;
/// Platform-neutral sandbox contract types and traits.
pub mod core;
/// Typed guest filesystem, streaming, and evaluation request contracts.
pub mod guest;

#[cfg(feature = "cli")]
/// Command-line integration for image and sandbox operations.
pub mod cli;

pub use core::{
    BackendKind, BoxFuture, Capability, ContractError, ExecResult, ExecSpec, ImageManifest,
    ManifestError, PixelFormat, ProviderError, Resources, Sandbox, SandboxError, SandboxProvider,
    SandboxSpec, TransportKind,
};

/// Compatibility namespace for consumers migrating from the former rfb-core crate.
pub mod prelude {
    pub use crate::core::*;
    pub use crate::guest;
}

mod image_profiles;
pub use image_profiles::{
    image_manifest_profile, validate_image_manifest_profile, ImageManifestProfile,
};

/// Unified sandbox backend selection: zeroboot / forkd behind one factory.
#[cfg(any(feature = "forkd", feature = "zeroboot"))]
pub mod backend;

#[cfg(feature = "forkd")]
#[path = "forkd/controller.rs"]
mod controller;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
#[path = "zeroboot/firecracker.rs"]
mod firecracker;
#[cfg(feature = "forkd")]
pub mod forkd;
#[cfg(feature = "forkd")]
#[path = "forkd/guest.rs"]
/// Forkd guest TCP/NDJSON transport client and typed RPC helpers.
pub mod forkd_guest;
#[cfg(feature = "zeroboot")]
/// ZBRT binary frame types and codec used by the ZeroBoot provider.
pub mod protocol;
#[cfg(all(feature = "zeroboot", target_os = "linux"))]
pub mod vsock;
#[cfg(feature = "zeroboot")]
pub mod zeroboot;
