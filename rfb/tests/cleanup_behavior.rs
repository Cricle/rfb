#![cfg(feature = "cli")]

use rfb::cli::cleanup::{cleanup, discover, validate_target};
use tempfile::tempdir;

#[test]
fn cleanup_is_scoped_and_dry_run_does_not_delete() {
    let root = tempdir().unwrap();
    let runtime = root.path().join("rfb-runtime");
    std::fs::create_dir(&runtime).unwrap();
    let artifact = runtime.join("image.ext4");
    let checksum = runtime.join("image.sha256");
    let ignored = runtime.join("keep.txt");
    std::fs::write(&artifact, b"x").unwrap();
    std::fs::write(&checksum, b"x").unwrap();
    std::fs::write(&ignored, b"x").unwrap();

    let found = discover(&runtime).unwrap();
    assert_eq!(found.len(), 2);
    let result = cleanup(&runtime, true, false).unwrap();
    assert_eq!(result["mode"], "dry_run");
    assert_eq!(result["found"], 2);
    assert_eq!(result["deleted"], 0);
    assert!(artifact.exists());
    assert!(checksum.exists());
}

#[test]
fn cleanup_requires_confirmation_and_deletes_only_known_artifacts() {
    let root = tempdir().unwrap();
    let runtime = root.path().join("rfb-runtime");
    std::fs::create_dir(&runtime).unwrap();
    let artifact = runtime.join("image.manifest.json");
    let ignored = runtime.join("keep");
    std::fs::write(&artifact, b"x").unwrap();
    std::fs::write(&ignored, b"x").unwrap();

    assert!(cleanup(&runtime, false, false).is_err());
    let result = cleanup(&runtime, false, true).unwrap();
    assert_eq!(result["deleted"], 1);
    assert!(!artifact.exists());
    assert!(ignored.exists());
}

#[test]
fn cleanup_rejects_unscoped_targets() {
    let root = tempdir().unwrap();
    assert!(validate_target(root.path()).is_err());
}
