use crate::ImageManifest;

/// Image profiles used by the CLI without enabling a runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageManifestProfile {
    /// rfb-runtime guest speaking RFB1 over vsock.
    RfbRuntimeRfb1Vsock,
    /// ZeroBoot guest speaking ZBRT over virtio-vsock.
    ZerobootZbrt,
}
impl ImageManifestProfile {
    /// All known profiles.
    pub const fn all() -> &'static [Self] {
        &[Self::RfbRuntimeRfb1Vsock, Self::ZerobootZbrt]
    }
    /// Canonical profile name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::RfbRuntimeRfb1Vsock => "rfb-runtime-rfb1-vsock",
            Self::ZerobootZbrt => "zeroboot-zbrt",
        }
    }
    /// Transport used by this profile.
    ///
    /// ```
    /// use rfb::ImageManifestProfile;
    /// assert_eq!(ImageManifestProfile::RfbRuntimeRfb1Vsock.transport(), "vsock");
    /// assert_eq!(ImageManifestProfile::ZerobootZbrt.protocol(), "zbrt");
    /// ```
    pub const fn transport(self) -> &'static str {
        match self {
            Self::RfbRuntimeRfb1Vsock => "vsock",
            Self::ZerobootZbrt => "virtio-vsock",
        }
    }
    /// Wire protocol used by this profile.
    pub const fn protocol(self) -> &'static str {
        match self {
            Self::RfbRuntimeRfb1Vsock => "rfb1",
            Self::ZerobootZbrt => "zbrt",
        }
    }
    /// Guest-facing vsock port used by this profile, if transport is vsock.
    pub const fn guest_port(self) -> Option<u16> {
        match self {
            Self::RfbRuntimeRfb1Vsock => Some(5000),
            Self::ZerobootZbrt => Some(5000),
        }
    }
    /// Entry point for this profile.
    pub const fn entrypoint(self) -> &'static str {
        match self {
            Self::RfbRuntimeRfb1Vsock => "/sbin/rfb-runtime",
            Self::ZerobootZbrt => "/init",
        }
    }
    /// Capabilities this profile advertises.
    pub const fn capabilities(self) -> &'static [&'static str] {
        &["execute"]
    }
}

/// Resolve a profile by its canonical name.
///
/// ```
/// # use rfb::image_manifest_profile;
/// assert_eq!(image_manifest_profile("rfb-runtime-rfb1-vsock").unwrap().transport(), "vsock");
/// assert!(image_manifest_profile("unknown").is_none());
/// ```
pub fn image_manifest_profile(profile: &str) -> Option<ImageManifestProfile> {
    ImageManifestProfile::all()
        .iter()
        .copied()
        .find(|p| p.name() == profile)
}

/// Validate that an image manifest matches a named profile.
///
/// ```
/// # use rfb::{validate_image_manifest_profile, ImageManifest};
/// let image = ImageManifest { image_ref: "guest".into(), transport: "vsock".into(), protocol: "rfb1".into(), entrypoint: vec!["/sbin/rfb-runtime".into()], arch: "x86_64".into(), ..Default::default() };
/// assert!(validate_image_manifest_profile(&image, "rfb-runtime-rfb1-vsock").is_ok());
/// assert!(validate_image_manifest_profile(&image, "unknown").is_err());
/// ```
pub fn validate_image_manifest_profile(image: &ImageManifest, name: &str) -> Result<(), String> {
    let profile =
        image_manifest_profile(name).ok_or_else(|| format!("unknown image profile: {name}"))?;
    if image.transport != profile.transport() {
        return Err(format!("profile transport must be {}", profile.transport()));
    }
    if image.protocol != profile.protocol() {
        return Err(format!("profile protocol must be {}", profile.protocol()));
    }
    let entrypoint = image.entrypoint.first().map(String::as_str);
    if entrypoint != Some(profile.entrypoint()) {
        return Err(format!(
            "profile entrypoint must be {}",
            profile.entrypoint()
        ));
    }
    Ok(())
}
