use rfb::prelude::ExecSpec;

#[test]
fn prelude_exposes_core_contracts() {
    let spec = ExecSpec::new("true");
    assert!(spec.validate().is_ok());
}
