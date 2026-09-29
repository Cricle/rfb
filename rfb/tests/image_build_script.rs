#![cfg(feature = "cli")]

//! `build.rfb` declarative build-script contract tests: full parsing, option
//! defaults, fail-closed schema/mode/unknown-field rejection, and the guest
//! path validation shared with the manifest builder.

use rfb::cli::image_build::{features_for, parse_script, validate_extra_guest_path};
use std::path::{Path, PathBuf};

const FULL: &str = r#"
schema = "rfb-build/v1"
mode = "zeroboot-zbrt"
output = "out/my.ext4"
force = true
size-mb = 64

[interpreters]
python = true
lua = true

[packages]
py-site = "sites/py"
lua-lib = "sites/lua"

[rust]
apps = ["apps/hello"]

[files]
"assets/banner.txt" = "/etc/motd"
"#;

#[test]
fn parses_full_script() {
    let script = parse_script(FULL).unwrap();
    assert_eq!(script.schema, "rfb-build/v1");
    assert_eq!(script.mode, "zeroboot-zbrt");
    assert_eq!(script.output, PathBuf::from("out/my.ext4"));
    assert!(script.force);
    assert_eq!(script.size_mb, Some(64));
    assert!(script.interpreters.python && script.interpreters.lua);
    assert_eq!(
        script.packages.py_site.as_deref(),
        Some(Path::new("sites/py"))
    );
    assert_eq!(script.rust.apps, vec!["apps/hello"]);
    assert_eq!(
        script.files.get("assets/banner.txt").map(String::as_str),
        Some("/etc/motd")
    );
    assert_eq!(features_for(&script), "cli,rustpython,mlua");
}

#[test]
fn defaults_are_minimal() {
    let script =
        parse_script("schema = \"rfb-build/v1\"\nmode = \"forkd-agent\"\noutput = \"a.ext4\"\n")
            .unwrap();
    assert!(!script.interpreters.python);
    assert!(!script.force);
    assert_eq!(features_for(&script), "cli");
}

#[test]
fn rejects_wrong_schema_and_mode() {
    let bad = "schema = \"nope\"\nmode = \"zeroboot-zbrt\"\noutput = \"a.ext4\"\n";
    assert!(parse_script(bad).is_err());
    let bad = "schema = \"rfb-build/v1\"\nmode = \"docker\"\noutput = \"a.ext4\"\n";
    assert!(parse_script(bad).is_err());
}

#[test]
fn rejects_unknown_fields() {
    let bad =
        "schema = \"rfb-build/v1\"\nmode = \"forkd-agent\"\noutput = \"a.ext4\"\nnonsense = 1\n";
    assert!(parse_script(bad).is_err());
    let bad = "schema = \"rfb-build/v1\"\nmode = \"forkd-agent\"\noutput = \"a.ext4\"\n[rust]\neval = true\n";
    assert!(parse_script(bad).is_err());
}

#[test]
fn extra_guest_paths_validate() {
    assert!(validate_extra_guest_path("/etc/motd").is_ok());
    assert!(validate_extra_guest_path("relative").is_err());
    assert!(validate_extra_guest_path("/a/../b").is_err());
    assert!(validate_extra_guest_path("/dir/").is_err());
}
