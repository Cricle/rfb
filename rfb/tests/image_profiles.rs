use rfb::{image_manifest_profile, ImageManifestProfile};

#[test]
fn profiles_expose_complete_public_metadata() {
    assert_eq!(ImageManifestProfile::all().len(), 2);
    assert_eq!(
        image_manifest_profile("rfb-runtime-rfb1-vsock"),
        Some(ImageManifestProfile::RfbRuntimeRfb1Vsock)
    );
    let profile = ImageManifestProfile::RfbRuntimeRfb1Vsock;
    assert_eq!(profile.name(), "rfb-runtime-rfb1-vsock");
    assert_eq!(profile.transport(), "vsock");
    assert_eq!(profile.protocol(), "rfb1");
    assert_eq!(profile.guest_port(), Some(5000));
    assert_eq!(profile.entrypoint(), "/sbin/rfb-runtime");
    assert_eq!(profile.capabilities(), &["execute"]);
}
