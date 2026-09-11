use rfb_runtime::runtime_service::{parse_runtime_backend, RuntimeBackend};
use rfb_runtime::sandbox::SandboxBackend;

#[test]
fn backend_defaults_to_forkd() {
    assert_eq!(SandboxBackend::default(), SandboxBackend::Forkd);
}

#[test]
fn runtime_backend_parser_defaults_to_forkd_without_environment_mutation() {
    assert_eq!(parse_runtime_backend(None), RuntimeBackend::Forkd);
    assert_eq!(parse_runtime_backend(Some("  ")), RuntimeBackend::Forkd);
}

#[test]
fn runtime_backend_parser_accepts_only_forkd() {
    assert_eq!(parse_runtime_backend(Some("forkd")), RuntimeBackend::Forkd);
    assert_eq!(
        parse_runtime_backend(Some(" FORKD ")),
        RuntimeBackend::Forkd
    );
}

#[test]
fn runtime_backend_parser_fails_closed_for_legacy_and_unknown_values() {
    assert_eq!(
        parse_runtime_backend(Some("legacy")),
        RuntimeBackend::Unsupported("legacy".into())
    );
    assert_eq!(
        parse_runtime_backend(Some("zeroboot")),
        RuntimeBackend::Unsupported("zeroboot".into())
    );
    assert_eq!(
        parse_runtime_backend(Some("unknown")),
        RuntimeBackend::Unsupported("unknown".into())
    );
}
