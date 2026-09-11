use rfb::prelude::*;
use std::time::Duration;

#[test]
fn pixel_formats_report_storage_widths() {
    assert_eq!(PixelFormat::Rgb8.bytes_per_pixel(), 3);
    assert_eq!(PixelFormat::Bgr8.bytes_per_pixel(), 3);
    assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
    assert_eq!(PixelFormat::Bgra8.bytes_per_pixel(), 4);
    assert_eq!(PixelFormat::Gray8.bytes_per_pixel(), 1);
    assert_eq!(PixelFormat::default(), PixelFormat::Rgba8);
}

#[test]
fn image_manifest_defaults_and_digest_boundaries_validate() {
    let image = ImageManifest::new("example:latest");
    assert_eq!(image.transport, "oci");
    assert_eq!(image.protocol, "rfb");
    assert_eq!(image.arch, "unknown");
    assert!(image.validate().is_ok());

    let mut valid = image.clone();
    valid.digest = Some(format!("sha256:{}", "a".repeat(64)));
    assert!(valid.validate().is_ok());

    for digest in [
        "sha256:",
        "sha256:abc",
        "sha256:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let mut invalid = image.clone();
        invalid.digest = Some(digest.to_owned());
        assert_eq!(invalid.validate(), Err(ManifestError::InvalidDigest));
    }
}

#[test]
fn manifest_and_spec_reject_empty_fields_and_zero_resources() {
    let mut image = ImageManifest::new(" ");
    assert_eq!(image.validate(), Err(ManifestError::EmptyImageRef));
    image.image_ref = "image".into();
    image.transport.clear();
    assert_eq!(image.validate(), Err(ManifestError::EmptyTransport));

    let mut spec = SandboxSpec::default();
    spec.resources.cpus = Some(0);
    assert_eq!(
        spec.validate(),
        Err(ContractError::InvalidResource(
            "resource limits must be non-zero"
        ))
    );

    let mut exec = ExecSpec::new(" ");
    assert_eq!(exec.validate(), Err(ContractError::EmptyCommand));
    exec.command = "true".into();
    exec.timeout = Some(Duration::ZERO);
    assert_eq!(exec.validate(), Err(ContractError::InvalidTimeout));
}

#[test]
fn exec_cwd_rejects_escape_and_host_paths() {
    for cwd in ["", "../tmp", "tmp/..", "\\tmp", "tmp\\file", "C:/tmp"] {
        let mut spec = ExecSpec::new("true");
        spec.cwd = Some(cwd.into());
        assert_eq!(
            spec.validate(),
            Err(ContractError::InvalidCwd),
            "cwd={cwd:?}"
        );
    }
    let mut valid = ExecSpec::new("true");
    valid.cwd = Some("workspace/tmp".into());
    assert!(valid.validate().is_ok());
}

struct MockSandbox;

impl Sandbox for MockSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        &[]
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                status: Some(0),
                stdout: vec![],
                stderr: vec![],
                timed_out: false,
            })
        })
    }
}

#[tokio::test]
async fn sandbox_default_operations_report_missing_capabilities() {
    let sandbox = MockSandbox;
    assert_eq!(sandbox.backend(), BackendKind::InMemory);
    assert_eq!(sandbox.transport(), TransportKind::InProcess);
    assert_eq!(
        sandbox.exec(ExecSpec::new("true")).await.unwrap().status,
        Some(0)
    );
    assert_eq!(
        sandbox.health().await,
        Err(SandboxError::UnsupportedCapability(Capability::Health))
    );
    assert_eq!(
        sandbox.framebuffer().await,
        Err(SandboxError::UnsupportedCapability(
            Capability::ReadFramebuffer
        ))
    );
    assert_eq!(
        sandbox.ping().await,
        Err(SandboxError::UnsupportedCapability(Capability::Health))
    );
}
