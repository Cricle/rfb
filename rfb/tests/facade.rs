use rfb::prelude::{ExecSpec, PixelFormat};

#[test]
fn prelude_exposes_core_contracts() {
    let spec = ExecSpec::new("true");
    assert!(spec.validate().is_ok());
    assert_eq!(PixelFormat::Rgba8.bytes_per_pixel(), 4);
}
