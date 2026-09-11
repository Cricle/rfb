use rfb::{ImageManifest, ManifestError, Resources, SandboxSpec};

#[test]
fn manifest_validation_covers_ref_path_and_digest_boundaries() {
    let mut image = ImageManifest::new(" ");
    assert_eq!(image.validate(), Err(ManifestError::EmptyImageRef));

    image = ImageManifest::new("registry/app:tag");
    for digest in [
        "sha256:short".to_owned(),
        "sha256:".to_owned(),
        "md5:00000000000000000000000000000000".to_owned(),
        "g".repeat(64),
    ] {
        image.digest = Some(digest);
        assert_eq!(image.validate(), Err(ManifestError::InvalidDigest));
    }
    image.digest = Some(format!("sha256:{}", "a".repeat(64)));
    assert!(image.validate().is_ok());
    image.digest = Some("A".repeat(64));
    assert!(image.validate().is_ok());
}

#[test]
fn sandbox_spec_rejects_zero_nested_resources() {
    let spec = SandboxSpec {
        resources: Resources {
            cpus: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(spec.validate().is_err());
}
