//! Unified sandbox backend selection for applications.
//!
//! ZeroBoot and forkd both implement [`SandboxProvider`], so switching between
//! them is a configuration change, not a code change. This module makes that
//! switch a one-line decision at the application layer:
//!
//! ```no_run
//! # async fn demo() -> Result<(), rfb::ProviderError> {
//! use rfb::{SandboxSpec, SandboxProvider, backend::SandboxBackendConfig};
//!
//! // Pick a backend (usually from configuration, here from RFB_SANDBOX_BACKEND).
//! let backend = SandboxBackendConfig::from_environment()?;
//! // Fail closed on unmet prerequisites before touching any VM.
//! for unmet in backend.unmet_prerequisites() {
//!     eprintln!("backend prerequisite unmet: {} — {}", unmet.name, unmet.detail);
//! }
//! // Same Sandbox trait surface for every backend.
//! let sandbox = backend.create(SandboxSpec::default()).await?;
//! # let _ = sandbox;
//! # Ok(())
//! # }
//! ```
//!
//! Selection semantics are fail-closed: an unknown backend name, or a backend
//! whose feature is not compiled in, is an error — never a silent fallback.

use crate::{Capability, ProviderError, Sandbox, SandboxProvider, SandboxSpec};
use std::future::Future;
use std::pin::Pin;

/// The environment variable that selects the sandbox backend.
pub const BACKEND_ENV: &str = "RFB_SANDBOX_BACKEND";

/// One backend prerequisite and whether it is currently met.
#[derive(Debug, Clone)]
pub struct Prerequisite {
    /// Stable machine-readable name, e.g. `kernel`, `snapshot_tag`.
    pub name: &'static str,
    /// `true` when the prerequisite is satisfied.
    pub met: bool,
    /// Human-readable detail for the unmet case.
    pub detail: String,
}

/// A selected sandbox backend, ready to create sandboxes.
///
/// Every backend implements [`SandboxProvider`], so callers hold one enum and
/// get the same [`Sandbox`] trait surface regardless of the wire protocol
/// (ZBRT over vsock vs NDJSON over TCP).
#[derive(Debug)]
pub enum SandboxBackendConfig {
    /// ZeroBoot: one Firecracker microVM per sandbox, ZBRT over vsock.
    #[cfg(feature = "zeroboot")]
    ZeroBoot(crate::zeroboot::Config),
    /// forkd: sandboxes spawned from a warm snapshot via the controller.
    #[cfg(feature = "forkd")]
    Forkd(crate::forkd::ForkdConfig),
}

/// Boxed future returned by [`SandboxBackendConfig::create`].
pub type CreateSandboxFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn Sandbox>, ProviderError>> + Send + 'a>>;

impl SandboxBackendConfig {
    /// Read [`BACKEND_ENV`] and load the selected backend's own environment
    /// configuration (`RFB_FIRECRACKER_*`-style paths for zeroboot, `FORKD_*`
    /// for forkd).
    pub fn from_environment() -> Result<Self, ProviderError> {
        let unavailable = |message: String| ProviderError::Unavailable(message);
        let name = std::env::var(BACKEND_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if name.is_empty() {
            return Err(unavailable(format!(
                "{BACKEND_ENV} is not set (expected: zeroboot | forkd)"
            )));
        }
        match name.as_str() {
            #[cfg(feature = "zeroboot")]
            "zeroboot" => Ok(Self::ZeroBoot(crate::zeroboot::Config::default())),
            #[cfg(feature = "forkd")]
            "forkd" => Ok(Self::Forkd(crate::forkd::ForkdConfig::from_env())),
            #[cfg(not(feature = "zeroboot"))]
            "zeroboot" => Err(unavailable(
                "zeroboot backend selected but the zeroboot feature is not compiled in",
            )),
            #[cfg(not(feature = "forkd"))]
            "forkd" => Err(unavailable(
                "forkd backend selected but the forkd feature is not compiled in",
            )),
            other => Err(unavailable(format!(
                "unknown {BACKEND_ENV} {other:?} (expected: zeroboot | forkd)"
            ))),
        }
    }

    /// Capabilities the selected backend advertises.
    pub fn capabilities(&self) -> Vec<Capability> {
        match self {
            #[cfg(feature = "zeroboot")]
            Self::ZeroBoot(config) => {
                let provider = crate::zeroboot::ZeroBootProvider::new(config.clone());
                provider.capabilities().to_vec()
            }
            #[cfg(feature = "forkd")]
            Self::Forkd(config) => config.guest_capabilities.clone(),
        }
    }

    /// Config-level prerequisites, without touching any VM or controller.
    ///
    /// Runtime reachability (Firecracker API, KVM, controller HTTP) is checked
    /// fail-closed inside [`Self::create`] with a specific error.
    pub fn prerequisites(&self) -> Vec<Prerequisite> {
        match self {
            #[cfg(feature = "zeroboot")]
            Self::ZeroBoot(config) => {
                let check = |name: &'static str, path: &Option<std::path::PathBuf>, label: &str| {
                    let (met, detail) = match path {
                        Some(path) if path.is_file() => (true, String::new()),
                        Some(path) => (
                            false,
                            format!("{label} is not a readable file: {}", path.display()),
                        ),
                        None => (false, format!("{label} path is required")),
                    };
                    Prerequisite { name, met, detail }
                };
                vec![
                    check("firecracker", &config.firecracker, "Firecracker binary"),
                    check("kernel", &config.kernel, "guest kernel"),
                    check("rootfs", &config.rootfs, "ZBRT rootfs image"),
                ]
            }
            #[cfg(feature = "forkd")]
            Self::Forkd(config) => {
                let tag_met = config.snapshot_tag.is_some();
                vec![Prerequisite {
                    name: "snapshot_tag",
                    met: tag_met,
                    detail: if tag_met {
                        String::new()
                    } else {
                        "FORKD_SNAPSHOT_TAG is not set".into()
                    },
                }]
            }
        }
    }

    /// List only the unmet prerequisites (convenience for startup logging).
    pub fn unmet_prerequisites(&self) -> Vec<Prerequisite> {
        self.prerequisites()
            .into_iter()
            .filter(|p| !p.met)
            .collect()
    }

    /// Create one sandbox with the selected backend.
    ///
    /// The same [`Sandbox`] trait surface is returned for every backend;
    /// capability requests are validated fail-closed by the provider itself.
    pub fn create<'a>(&'a self, spec: SandboxSpec) -> CreateSandboxFuture<'a> {
        match self {
            #[cfg(feature = "zeroboot")]
            Self::ZeroBoot(config) => Box::pin(async move {
                crate::zeroboot::ZeroBootProvider::new(config.clone())
                    .create_zero_boot(spec)
                    .await
                    .map(|sandbox| Box::new(sandbox) as Box<dyn Sandbox>)
            }),
            #[cfg(feature = "forkd")]
            Self::Forkd(config) => {
                let provider = match crate::forkd::ForkdClient::new(config.clone()) {
                    Ok(client) => client,
                    Err(error) => {
                        return Box::pin(async move {
                            Err(ProviderError::Unavailable(error.to_string()))
                        });
                    }
                };
                // Qualify the trait method explicitly: the inherent
                // `ForkdClient::create` takes a `CreateSandboxRequest`.
                Box::pin(async move {
                    <crate::forkd::ForkdClient as SandboxProvider>::create(&provider, spec).await
                })
            }
        }
    }
}
