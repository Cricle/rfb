#![cfg(feature = "cli")]

//! Offline unit-style coverage for the `rfb-cli image_build` helpers:
//! staging-manifest parsing/validation (`manifest.rs`), artifact manifest
//! construction and serialization (`artifact.rs`), verification gates
//! (`verify.rs`), and the build orchestration (`build.rs`).
//!
//! Every test is self-contained: it only touches files under the system
//! temporary directory and never starts a VM, Firecracker, KVM, or a real
//! `mke2fs`/`debugfs` image build. Branches that require external binaries
//! (the actual ext4 build behind `--execute`, `build_rootfs`, `readelf`,
//! `cargo`/`rustup`) are exercised only up to their pure rejection gates and
//! documented here so the remaining lines are known to be tool-gated.

use rfb::cli::error::{EXIT_IO, EXIT_VALIDATION};
use rfb::cli::image_build::{
    build, build_static_runtime, check_kernel, image_diagnostics, init, inspect_rootfs, load,
    profile_summary, run_debugfs, safe_join, sha256, sha256_bytes, validate, ArtifactFile,
    ArtifactManifest, ArtifactMismatch, ArtifactSnapshot, ArtifactTool, ArtifactVm,
    ImageManifestWire, StagedFile, StagingManifest, FORKD_PROFILE, FORKD_PROTOCOL, FORKD_TRANSPORT,
};
use rfb::Resources;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A valid 64-hex SHA-256 digest (content-independent; for schema-only tests).
fn digest() -> String {
    sha256_bytes(b"artifact-digest")
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).expect("serialize manifest"))
        .expect("write manifest");
}

/// Write a staging manifest JSON document at `dir/manifest.json`.
fn write_staging_manifest(dir: &Path, files: Value, image: Value) -> PathBuf {
    let manifest_path = dir.join("manifest.json");
    let value = json!({
        "format": "rfb-cli-staging/v1",
        "image_size_bytes": 4096,
        "block_size": 4096,
        "staging": "staging",
        "files": files,
        "image": image,
    });
    write_json(&manifest_path, &value);
    manifest_path
}

/// A staging directory containing a real `init` file plus a manifest whose
/// `files` entry matches the file's bytes and digest. Returns the temp dir
/// (kept alive) and the manifest path.
fn valid_staging_tree() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temporary directory");
    fs::create_dir_all(dir.path().join("staging")).expect("staging directory");
    let bytes = b"#!/bin/sh\nexit 0\n";
    fs::write(dir.path().join("staging/init"), bytes).expect("write staging init");
    let manifest = write_staging_manifest(
        dir.path(),
        json!([{
            "path": "init",
            "size": bytes.len(),
            "sha256": sha256_bytes(bytes),
            "source": "init",
        }]),
        Value::Null,
    );
    (dir, manifest)
}

// ---------------------------------------------------------------------------
// manifest.rs: parsing and validation
// ---------------------------------------------------------------------------

#[test]
fn staging_manifest_loads_a_minimal_legal_document() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = write_staging_manifest(dir.path(), json!([]), Value::Null);

    let parsed = load(&manifest).expect("valid manifest loads");
    assert_eq!(parsed.format, "rfb-cli-staging/v1");
    assert_eq!(parsed.image_size_bytes, 4096);
    assert_eq!(parsed.block_size, 4096);
    assert_eq!(parsed.staging, "staging");
    assert!(parsed.files.is_empty());
    assert!(parsed.image.is_none());
}

#[test]
fn staging_manifest_load_rejects_a_missing_format_field() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = dir.path().join("manifest.json");
    write_json(
        &manifest,
        &json!({
            "image_size_bytes": 4096,
            "block_size": 4096,
            "staging": "staging",
            "files": [],
        }),
    );
    let err = load(&manifest).expect_err("missing format is rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("invalid staging manifest"));
}

#[test]
fn staging_manifest_load_rejects_unknown_fields_everywhere() {
    let dir = tempfile::tempdir().expect("temporary directory");

    // Unknown top-level field.
    let manifest = dir.path().join("m1.json");
    let value = json!({
        "format": "rfb-cli-staging/v1", "image_size_bytes": 4096, "block_size": 4096,
        "staging": "staging", "files": [], "future_field": true,
    });
    write_json(&manifest, &value);
    let err = load(&manifest).expect_err("unknown top-level field rejected");
    assert!(err.message.contains("unknown field"), "{}", err.message);

    // Unknown field inside a staged file.
    let manifest = write_staging_manifest(
        dir.path(),
        json!([{"path": "a", "size": 0, "sha256": "", "source": "s", "extra": 1}]),
        Value::Null,
    );
    let err = load(&manifest).expect_err("unknown StagedFile field rejected");
    assert!(err.message.contains("unknown field"), "{}", err.message);

    // Unknown field inside the embedded image manifest.
    let manifest = write_staging_manifest(
        dir.path(),
        json!([]),
        json!({
            "image_ref": "x", "transport": "tcp", "entrypoint": [], "protocol": "forkd",
            "arch": "x86_64", "resources": {}, "bogus": 1,
        }),
    );
    let err = load(&manifest).expect_err("unknown ImageManifestWire field rejected");
    assert!(err.message.contains("unknown field"), "{}", err.message);
}

#[test]
fn staging_manifest_load_rejects_a_non_array_files_field() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = dir.path().join("manifest.json");
    let value = json!({
        "format": "rfb-cli-staging/v1", "image_size_bytes": 4096, "block_size": 4096,
        "staging": "staging", "files": "not-an-array",
    });
    write_json(&manifest, &value);
    let err = load(&manifest).expect_err("wrong files type rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("invalid staging manifest"));
}

#[test]
fn staging_manifest_serde_round_trip_preserves_every_field() {
    let wire = ImageManifestWire {
        image_ref: "registry.example/app:latest".into(),
        digest: Some(format!("sha256:{}", digest())),
        transport: "oci".into(),
        entrypoint: vec!["/init".into()],
        protocol: "rfb".into(),
        arch: "x86_64".into(),
        resources: Resources {
            cpus: Some(2),
            memory_bytes: Some(1073741824),
            ..Resources::default()
        },
        profile: None,
        capabilities: vec!["execute".into()],
    };
    let manifest = StagingManifest {
        format: "rfb-cli-staging/v1".into(),
        image_size_bytes: 8192,
        block_size: 4096,
        staging: "staging".into(),
        files: vec![StagedFile {
            path: "init".into(),
            size: 7,
            sha256: digest(),
            source: "init".into(),
        }],
        image: Some(wire.clone()),
    };

    let text = serde_json::to_string(&manifest).expect("serialize staging manifest");
    let back: StagingManifest = serde_json::from_str(&text).expect("deserialize staging manifest");
    assert_eq!(back.format, "rfb-cli-staging/v1");
    assert_eq!(back.image_size_bytes, 8192);
    assert_eq!(back.block_size, 4096);
    assert_eq!(back.staging, "staging");
    assert_eq!(back.files.len(), 1);
    assert_eq!(back.files[0].path, "init");
    assert_eq!(back.files[0].size, 7);
    assert_eq!(back.files[0].sha256, manifest.files[0].sha256);
    assert_eq!(back.files[0].source, "init");
    let image = back.image.expect("image preserved");
    assert_eq!(image.image_ref, wire.image_ref);
    assert_eq!(image.digest, wire.digest);
    assert_eq!(image.entrypoint, vec!["/init".to_string()]);
    assert_eq!(image.protocol, "rfb");
    assert_eq!(image.capabilities, vec!["execute".to_string()]);
    assert_eq!(image.resources.cpus, Some(2));
    assert_eq!(image.resources.memory_bytes, Some(1073741824));
}

#[test]
fn staging_manifest_validate_rejects_unsupported_format() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = dir.path().join("manifest.json");
    let value = json!({
        "format": "nope/v9", "image_size_bytes": 4096, "block_size": 4096,
        "staging": "staging", "files": [],
    });
    write_json(&manifest, &value);
    let err = validate(&manifest).expect_err("unsupported format rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("unsupported staging manifest format"));
}

#[test]
fn staging_manifest_validate_rejects_zero_and_non_multiple_sizes() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for (size, block) in [(0u64, 4096u64), (5000, 4096), (4096, 0)] {
        let manifest = dir.path().join(format!("m-{size}-{block}.json"));
        let value = json!({
            "format": "rfb-cli-staging/v1", "image_size_bytes": size, "block_size": block,
            "staging": "staging", "files": [],
        });
        write_json(&manifest, &value);
        let err = validate(&manifest).expect_err("invalid size rejected");
        assert_eq!(err.code, EXIT_VALIDATION);
        assert!(
            err.message.contains("non-zero multiple of block_size"),
            "{}",
            err.message
        );
    }
}

#[test]
fn staging_manifest_validate_rejects_a_missing_staging_directory() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = write_staging_manifest(dir.path(), json!([]), Value::Null);
    let err = validate(&manifest).expect_err("missing staging dir rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("staging directory missing"));
}

#[test]
fn staging_manifest_validate_accepts_a_real_staging_tree() {
    let (_dir, manifest) = valid_staging_tree();
    let validated = validate(&manifest).expect("valid staging tree validates");
    assert_eq!(validated.files.len(), 1);
    assert_eq!(validated.files[0].path, "init");
    assert_eq!(validated.files[0].source, "init");
}

#[test]
fn staging_manifest_validate_rejects_staged_file_problems() {
    // Missing staged file.
    let dir = tempfile::tempdir().expect("temporary directory");
    fs::create_dir_all(dir.path().join("staging")).expect("staging directory");
    let manifest = write_staging_manifest(
        dir.path(),
        json!([{"path": "missing", "size": 1, "sha256": digest(), "source": "s"}]),
        Value::Null,
    );
    let err = validate(&manifest).expect_err("missing staged file rejected");
    assert!(err.message.contains("staged file missing"));

    // Size mismatch.
    fs::write(dir.path().join("staging/init"), b"#!/bin/sh\nexit 0\n").expect("write init");
    let manifest = write_staging_manifest(
        dir.path(),
        json!([{"path": "init", "size": 1, "sha256": sha256_bytes(b"#!/bin/sh\nexit 0\n"), "source": "s"}]),
        Value::Null,
    );
    let err = validate(&manifest).expect_err("size mismatch rejected");
    assert!(err.message.contains("size mismatch"));

    // Malformed sha256.
    let manifest = write_staging_manifest(
        dir.path(),
        json!([{"path": "init", "size": 16, "sha256": "not-a-digest", "source": "s"}]),
        Value::Null,
    );
    let err = validate(&manifest).expect_err("invalid sha256 rejected");
    assert_eq!(err.code, EXIT_VALIDATION);

    // sha256 that does not match the file contents.
    let manifest = write_staging_manifest(
        dir.path(),
        json!([{"path": "init", "size": 16, "sha256": sha256_bytes(b"different bytes!!"), "source": "s"}]),
        Value::Null,
    );
    let err = validate(&manifest).expect_err("sha256 mismatch rejected");
    assert_eq!(err.code, EXIT_VALIDATION);

    // Duplicate file paths (detected before existence checks).
    let file = json!({"path": "init", "size": 16, "sha256": sha256_bytes(b"#!/bin/sh\nexit 0\n"), "source": "s"});
    let manifest = write_staging_manifest(dir.path(), json!([file, file]), Value::Null);
    let err = validate(&manifest).expect_err("duplicate path rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
}

#[test]
fn staging_manifest_validate_rejects_unsafe_and_non_canonical_paths() {
    let dir = tempfile::tempdir().expect("temporary directory");
    fs::create_dir_all(dir.path().join("staging")).expect("staging directory");

    for path in ["../outside", "sub/../../escape", "C:/abs"] {
        let manifest = write_staging_manifest(
            dir.path(),
            json!([{"path": path, "size": 0, "sha256": digest(), "source": "s"}]),
            Value::Null,
        );
        let err = validate(&manifest).expect_err("unsafe path rejected");
        assert!(err.message.contains("unsafe file path"), "{}", err.message);
    }

    let manifest = write_staging_manifest(
        dir.path(),
        json!([{"path": "init/", "size": 0, "sha256": digest(), "source": "s"}]),
        Value::Null,
    );
    let err = validate(&manifest).expect_err("non-canonical path rejected");
    assert!(err.message.contains("non-canonical file path"));
}

#[test]
fn safe_join_rejects_traversal_absolute_and_backslash_paths() {
    let root = Path::new("C:/some/root");
    for relative in ["", "..", "../escape", "/abs", "a\\b", "a\0b"] {
        assert!(
            safe_join(root, relative).is_err(),
            "safe_join should reject {relative:?}"
        );
    }
    let joined = safe_join(root, "a/b").expect("clean relative path joins");
    assert_eq!(joined, root.join("a/b"));
}

#[test]
fn sha256_helpers_produce_known_digests_and_agree_on_files() {
    assert_eq!(
        sha256_bytes(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let dir = tempfile::tempdir().expect("temporary directory");
    let file = dir.path().join("blob");
    fs::write(&file, b"hello world").expect("write blob");
    assert_eq!(
        sha256(&file).expect("file hashed"),
        sha256_bytes(b"hello world")
    );
}

#[test]
fn forkd_constants_are_stable() {
    assert_eq!(FORKD_PROFILE, "forkd-agent");
    assert_eq!(FORKD_TRANSPORT, "tcp");
    assert_eq!(FORKD_PROTOCOL, "forkd");
}

#[test]
fn staging_manifest_core_image_converts_and_validates() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = write_staging_manifest(
        dir.path(),
        json!([]),
        json!({
            "image_ref": "registry.example/app:latest", "transport": "oci",
            "entrypoint": ["/init"], "protocol": "rfb", "arch": "x86_64", "resources": {},
        }),
    );
    let parsed = load(&manifest).expect("loads");
    let core = parsed
        .core_image()
        .expect("conversion succeeds")
        .expect("image present");
    assert_eq!(core.image_ref, "registry.example/app:latest");
    assert_eq!(core.transport, "oci");
    assert_eq!(core.entrypoint, vec!["/init".to_string()]);

    // An embedded image with an empty reference fails conversion.
    let manifest = write_staging_manifest(
        dir.path(),
        json!([]),
        json!({
            "image_ref": "", "transport": "oci", "entrypoint": [], "protocol": "rfb",
            "arch": "x86_64", "resources": {},
        }),
    );
    let parsed = load(&manifest).expect("loads");
    let err = parsed.core_image().expect_err("invalid image rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("image_ref must not be empty"));

    // An image without an image manifest has no core image.
    let manifest = write_staging_manifest(dir.path(), json!([]), Value::Null);
    let parsed = load(&manifest).expect("loads");
    assert!(parsed.core_image().expect("ok").is_none());
}

#[test]
fn image_manifest_wire_rejects_an_empty_transport() {
    let wire = ImageManifestWire {
        image_ref: "guest".into(),
        digest: None,
        transport: "".into(),
        entrypoint: vec!["/init".into()],
        protocol: "rfb".into(),
        arch: "x86_64".into(),
        resources: Resources::default(),
        profile: None,
        capabilities: vec![],
    };
    let err = wire.into_core().expect_err("empty transport rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("image transport must not be empty"));
}

#[test]
fn forkd_agent_profile_validation_gates_the_manifest() {
    let base = ImageManifestWire {
        image_ref: "registry.example/agent:latest".into(),
        digest: None,
        transport: "tcp".into(),
        entrypoint: vec!["/forkd-init.sh".into(), "--serve".into()],
        protocol: "forkd".into(),
        arch: "x86_64".into(),
        resources: Resources::default(),
        profile: Some(FORKD_PROFILE.into()),
        capabilities: vec!["execute".into(), "health".into(), "stream".into()],
    };
    assert!(
        base.clone().into_core().is_ok(),
        "valid forkd-agent accepts"
    );

    let mut wire = base.clone();
    wire.transport = "vsock".into();
    let err = wire.into_core().expect_err("wrong transport rejected");
    assert!(err.message.contains("forkd-agent transport must be tcp"));

    wire = base.clone();
    wire.protocol = "rfb1".into();
    let err = wire.into_core().expect_err("wrong protocol rejected");
    assert!(err.message.contains("forkd-agent protocol must be forkd"));

    wire = base.clone();
    wire.entrypoint = vec![];
    let err = wire.into_core().expect_err("missing entrypoint rejected");
    assert!(err.message.contains("requires a non-empty entrypoint"));

    wire = base.clone();
    wire.capabilities = vec![];
    let err = wire.into_core().expect_err("missing capabilities rejected");
    assert!(err.message.contains("requires explicit capabilities"));

    wire = base.clone();
    wire.capabilities = vec!["execute".into(), "evil".into()];
    let err = wire
        .into_core()
        .expect_err("unsupported capability rejected");
    assert!(err.message.contains("unsupported capability"));

    wire = base.clone();
    wire.capabilities = vec!["execute".into(), "execute".into()];
    let err = wire.into_core().expect_err("duplicate capability rejected");
    assert!(err.message.contains("capability is duplicated"));

    // An rfb-runtime vsock image can never be a forkd-agent.
    wire = base.clone();
    wire.image_ref = "registry.example/rfb-runtime:v1".into();
    wire.transport = "vsock".into();
    wire.protocol = "rfb1".into();
    let err = wire.into_core().expect_err("rfb-runtime vsock rejected");
    assert!(err
        .message
        .contains("rfb-runtime vsock image cannot be used as forkd-agent"));
}

#[test]
fn named_profile_validation_runs_for_other_profiles() {
    let base = ImageManifestWire {
        image_ref: "guest".into(),
        digest: None,
        transport: "vsock".into(),
        entrypoint: vec!["/sbin/rfb-runtime".into()],
        protocol: "rfb1".into(),
        arch: "x86_64".into(),
        resources: Resources::default(),
        profile: Some("rfb-runtime-rfb1-vsock".into()),
        capabilities: vec![],
    };
    assert!(base.clone().into_core().is_ok(), "matching profile accepts");

    let mut wire = base;
    wire.transport = "tcp".into();
    let err = wire
        .into_core()
        .expect_err("profile transport mismatch rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("profile transport must be vsock"));
}

// ---------------------------------------------------------------------------
// artifact.rs: construction, serialization, validation
// ---------------------------------------------------------------------------

#[test]
fn artifact_manifest_serde_round_trip_preserves_every_field() {
    let manifest = ArtifactManifest {
        schema: "rfb-artifact/v1".into(),
        backend: "forkd".into(),
        profile: "forkd-agent".into(),
        arch: "x86_64".into(),
        transport: "tcp".into(),
        protocol: "forkd".into(),
        entrypoint: "/forkd-init.sh".into(),
        guest_port: 8888,
        kernel: Some(ArtifactFile {
            path: "/boot/vmlinux".into(),
            sha256: digest(),
        }),
        rootfs: Some(ArtifactFile {
            path: "/srv/rootfs.ext4".into(),
            sha256: digest(),
        }),
        firecracker: Some(ArtifactTool {
            path: "/usr/bin/firecracker".into(),
            version: "1.4.0".into(),
            sha256: digest(),
        }),
        vm: ArtifactVm {
            cpus: 2,
            memory_bytes: 1073741824,
        },
        snapshot: Some(ArtifactSnapshot {
            tag: "rfb".into(),
            sha256: digest(),
            memory_sha256: None,
            vmstate_sha256: None,
            network: true,
            batch_id: None,
            version: None,
            vm_identity: None,
            network_identity: None,
        }),
    };

    let text = serde_json::to_string(&manifest).expect("serialize artifact manifest");
    let back: ArtifactManifest =
        serde_json::from_str(&text).expect("deserialize artifact manifest");
    assert_eq!(back.schema, "rfb-artifact/v1");
    assert_eq!(back.backend, "forkd");
    assert_eq!(back.profile, "forkd-agent");
    assert_eq!(back.arch, "x86_64");
    assert_eq!(back.transport, "tcp");
    assert_eq!(back.protocol, "forkd");
    assert_eq!(back.entrypoint, "/forkd-init.sh");
    assert_eq!(back.guest_port, 8888);
    assert_eq!(back.kernel.as_ref().expect("kernel").path, "/boot/vmlinux");
    assert_eq!(
        back.rootfs.as_ref().expect("rootfs").sha256,
        manifest.rootfs.as_ref().expect("rootfs").sha256
    );
    assert_eq!(
        back.firecracker.as_ref().expect("firecracker").version,
        "1.4.0"
    );
    assert_eq!(back.vm.cpus, 2);
    assert_eq!(back.vm.memory_bytes, 1073741824);
    assert_eq!(back.snapshot.as_ref().expect("snapshot").tag, "rfb");
    assert!(back.snapshot.as_ref().expect("snapshot").network);
}

#[test]
fn artifact_manifest_serde_rejects_unknown_fields() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("artifact.json");
    write_json(
        &path,
        &json!({
            "schema": "rfb-artifact/v1", "backend": "rfb1", "profile": "p", "arch": "x",
            "transport": "vsock", "protocol": "rfb1", "entrypoint": "/init", "guest_port": 5000,
            "vm": {"cpus": 1, "memory_bytes": 0}, "mystery": true,
        }),
    );
    let err = ArtifactManifest::load(&path).expect_err("unknown field rejected");
    assert!(err.message.contains("unknown field"), "{}", err.message);
}

#[test]
fn artifact_digests_are_checked_by_schema_validation() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("out.ext4");
    fs::write(&path, b"x").expect("write artifact");
    let good = ArtifactManifest::for_kernel(&path, &sha256_bytes(b"x"));
    assert!(good.is_ok());
    assert!(good.unwrap().validate_schema().is_ok());
    let bad = ArtifactManifest::for_kernel(&path, "short");
    assert!(bad.is_ok());
    assert!(bad.unwrap().validate_schema().is_err());
}

#[test]
fn artifact_for_kernel_builds_a_valid_kernel_manifest() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let kernel_path = dir.path().join("vmlinux");
    fs::write(&kernel_path, b"kernel-bytes").expect("write kernel");
    let kernel_digest = sha256_bytes(b"kernel-bytes");

    let artifact =
        ArtifactManifest::for_kernel(&kernel_path, &kernel_digest).expect("build kernel artifact");
    assert_eq!(artifact.schema, "rfb-artifact/v1");
    assert_eq!(artifact.backend, "rfb1");
    assert_eq!(artifact.profile, "rfb1-kernel");
    assert_eq!(artifact.arch, "x86_64");
    assert_eq!(artifact.transport, "vsock");
    assert_eq!(artifact.protocol, "rfb1");
    assert_eq!(artifact.entrypoint, "/sbin/rfb-runtime");
    assert_eq!(artifact.guest_port, 5000);
    assert_eq!(artifact.kind(), "kernel");
    assert_eq!(
        artifact.kernel.as_ref().expect("kernel").path,
        kernel_path.to_string_lossy().as_ref()
    );
    assert!(artifact.validate().is_ok(), "local kernel file validates");
}

#[test]
fn artifact_for_static_runtime_selects_package_properties() {
    let for_pkg = |package: &str| {
        ArtifactManifest::for_static_runtime(
            Path::new("/nonexistent/rootfs.ext4"),
            &digest(),
            package,
            "x86_64-unknown-linux-musl",
        )
        .expect("static runtime artifact builds")
    };

    let forkd = for_pkg("forkd-agent");
    assert_eq!(forkd.backend, "forkd");
    assert_eq!(forkd.profile, "forkd-agent-x86_64-unknown-linux-musl");
    assert_eq!(forkd.transport, "tcp");
    assert_eq!(forkd.protocol, "forkd");
    assert_eq!(forkd.entrypoint, "/forkd-init.sh");
    assert_eq!(forkd.guest_port, 8888);
    assert!(forkd.validate().is_ok());

    let zeroboot = for_pkg("zeroboot-zbrt");
    assert_eq!(zeroboot.backend, "zeroboot");
    assert_eq!(zeroboot.profile, "zeroboot-zbrt-x86_64-unknown-linux-musl");
    assert_eq!(zeroboot.transport, "virtio-vsock");
    assert_eq!(zeroboot.protocol, "zbrt");
    assert_eq!(zeroboot.entrypoint, "/init");
    assert_eq!(zeroboot.guest_port, 5000);
    assert!(zeroboot.validate().is_ok());

    let custom = for_pkg("custom-package");
    assert_eq!(custom.backend, "rfb1");
    assert_eq!(custom.profile, "rfb-runtime-x86_64-unknown-linux-musl");
    assert_eq!(custom.transport, "vsock");
    assert_eq!(custom.protocol, "rfb1");
    assert_eq!(custom.entrypoint, "/sbin/rfb-runtime");
    assert_eq!(custom.guest_port, 5000);
    assert!(custom.validate().is_ok());
}

#[test]
fn artifact_for_rootfs_selects_modes_and_rejects_unknown_ones() {
    let rootfs = ArtifactManifest::for_rootfs(
        Path::new("/out.ext4"),
        &digest(),
        "forkd-agent",
        "/forkd-init.sh",
        "forkd",
    )
    .expect("forkd-agent rootfs builds");
    assert_eq!(rootfs.backend, "forkd");
    assert_eq!(rootfs.profile, "forkd-agent-tcp");
    assert_eq!(rootfs.transport, "tcp");
    assert_eq!(rootfs.guest_port, 8888);
    // Rootfs artifacts declare no VM memory (memory_bytes = 0): the
    // provenance identity skips the memory comparison for them.
    assert_eq!(rootfs.vm.memory_bytes, 0);
    assert!(rootfs.validate().is_ok());

    let vsock = ArtifactManifest::for_rootfs(
        Path::new("/out.ext4"),
        &digest(),
        "rfb-vsock",
        "/sbin/rfb-runtime",
        "rfb1",
    )
    .expect("rfb-vsock rootfs builds");
    assert_eq!(vsock.backend, "rfb1");
    assert_eq!(vsock.profile, "rfb-runtime-rfb1-vsock");
    assert_eq!(vsock.transport, "vsock");
    assert_eq!(vsock.guest_port, 5000);
    assert!(vsock.validate().is_ok());

    let zbrt = ArtifactManifest::for_rootfs(
        Path::new("/out.ext4"),
        &digest(),
        "zeroboot-zbrt",
        "/init",
        "zbrt",
    )
    .expect("zbrt rootfs builds");
    assert_eq!(zbrt.backend, "zeroboot");
    assert_eq!(zbrt.profile, "zeroboot-zbrt");
    assert_eq!(zbrt.transport, "virtio-vsock");
    assert_eq!(zbrt.guest_port, 5000);
    assert!(zbrt.validate().is_ok());

    let err =
        ArtifactManifest::for_rootfs(Path::new("/out.ext4"), &digest(), "bogus", "/init", "rfb1")
            .expect_err("unknown mode rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("unsupported rootfs artifact mode"));
}

#[test]
fn artifact_for_image_maps_transport_and_protocol() {
    let for_wire = |transport: &str, protocol: &str| {
        let wire = ImageManifestWire {
            image_ref: "custom-image".into(),
            digest: None,
            transport: transport.into(),
            entrypoint: vec!["/entry".into()],
            protocol: protocol.into(),
            arch: "aarch64".into(),
            resources: Resources {
                cpus: Some(4),
                memory_bytes: Some(1073741824),
                ..Resources::default()
            },
            profile: None,
            capabilities: vec![],
        };
        ArtifactManifest::for_image(Path::new("/out.ext4"), &digest(), &wire)
            .expect("image artifact builds")
    };

    let forkd = for_wire("tcp", "forkd");
    assert_eq!(forkd.backend, "forkd");
    assert_eq!(forkd.transport, "tcp");
    assert_eq!(forkd.protocol, "forkd");
    assert_eq!(forkd.guest_port, 8888);
    assert_eq!(forkd.profile, "custom");
    assert_eq!(forkd.arch, "aarch64");
    assert_eq!(forkd.vm.cpus, 4);
    assert_eq!(forkd.vm.memory_bytes, 1073741824);

    let rfb1 = for_wire("vsock", "rfb1");
    assert_eq!(rfb1.backend, "rfb1");
    assert_eq!(rfb1.guest_port, 5000);

    let zbrt = for_wire("virtio-vsock", "zbrt");
    assert_eq!(zbrt.backend, "zeroboot");
    assert_eq!(zbrt.guest_port, 5000);

    // Unknown combos fall back to the rfb1 backend.
    let fallback = for_wire("oci", "custom");
    assert_eq!(fallback.backend, "rfb1");
    assert_eq!(fallback.transport, "oci");
    assert_eq!(fallback.protocol, "custom");
    assert_eq!(fallback.guest_port, 5000);
    assert!(fallback.validate_schema().is_ok());
}

#[test]
fn artifact_kind_reports_the_most_specific_category() {
    let base = ArtifactManifest {
        schema: "rfb-artifact/v1".into(),
        backend: "rfb1".into(),
        profile: "p".into(),
        arch: "x86_64".into(),
        transport: "vsock".into(),
        protocol: "rfb1".into(),
        entrypoint: "/init".into(),
        guest_port: 5000,
        kernel: None,
        rootfs: None,
        firecracker: None,
        vm: ArtifactVm {
            cpus: 1,
            memory_bytes: 0,
        },
        snapshot: None,
    };
    assert_eq!(base.kind(), "unknown");

    let mut m = base.clone();
    m.rootfs = Some(ArtifactFile {
        path: "/r".into(),
        sha256: digest(),
    });
    assert_eq!(m.kind(), "rootfs");

    let mut m = base.clone();
    m.kernel = Some(ArtifactFile {
        path: "/k".into(),
        sha256: digest(),
    });
    assert_eq!(m.kind(), "kernel");

    let mut m = base.clone();
    m.firecracker = Some(ArtifactTool {
        path: "/fc".into(),
        version: "1".into(),
        sha256: digest(),
    });
    assert_eq!(m.kind(), "firecracker");

    let mut m = base;
    m.snapshot = Some(ArtifactSnapshot {
        tag: "t".into(),
        sha256: digest(),
        memory_sha256: None,
        vmstate_sha256: None,
        network: false,
        batch_id: None,
        version: None,
        vm_identity: None,
        network_identity: None,
    });
    assert_eq!(m.kind(), "snapshot");
}

#[test]
fn artifact_validate_schema_rejects_bad_structure() {
    let mut m = ArtifactManifest {
        schema: "rfb-artifact/v1".into(),
        backend: "rfb1".into(),
        profile: "p".into(),
        arch: "x86_64".into(),
        transport: "vsock".into(),
        protocol: "rfb1".into(),
        entrypoint: "/init".into(),
        guest_port: 5000,
        kernel: None,
        rootfs: None,
        firecracker: None,
        vm: ArtifactVm {
            cpus: 1,
            memory_bytes: 0,
        },
        snapshot: None,
    };

    m.schema = "rfb-artifact/v2".into();
    let err = m.validate_schema().expect_err("unknown schema rejected");
    assert!(err.message.contains("unsupported artifact schema"));

    m.schema = "rfb-artifact/v1".into();
    m.backend = "bogus".into();
    let err = m.validate_schema().expect_err("unknown backend rejected");
    assert!(err.message.contains("unsupported artifact backend"));

    m.backend = "rfb1".into();
    m.guest_port = 0;
    let err = m.validate_schema().expect_err("zero guest_port rejected");
    assert!(err.message.contains("guest_port must be non-zero"));

    m.guest_port = 5000;
    m.rootfs = Some(ArtifactFile {
        path: "".into(),
        sha256: digest(),
    });
    let err = m.validate_schema().expect_err("empty path rejected");
    assert!(err.message.contains("rootfs path must not be empty"));

    m.rootfs = Some(ArtifactFile {
        path: "/r".into(),
        sha256: "not-a-digest".into(),
    });
    let err = m.validate_schema().expect_err("bad sha256 rejected");
    assert!(err.message.contains("invalid sha256 for rootfs"));

    m.rootfs = None;
    m.snapshot = Some(ArtifactSnapshot {
        tag: "t".into(),
        sha256: "zz".into(),
        memory_sha256: None,
        vmstate_sha256: None,
        network: false,
        batch_id: None,
        version: None,
        vm_identity: None,
        network_identity: None,
    });
    let err = m
        .validate_schema()
        .expect_err("bad snapshot sha256 rejected");
    assert!(err.message.contains("invalid snapshot sha256"));
}

#[test]
fn artifact_validate_enforces_backend_transport_contract() {
    let m = ArtifactManifest {
        schema: "rfb-artifact/v1".into(),
        backend: "forkd".into(),
        profile: "p".into(),
        arch: "x86_64".into(),
        transport: "vsock".into(),
        protocol: "forkd".into(),
        entrypoint: "/forkd-init.sh".into(),
        guest_port: 8888,
        kernel: None,
        rootfs: None,
        firecracker: None,
        vm: ArtifactVm {
            cpus: 1,
            memory_bytes: 0,
        },
        snapshot: None,
    };
    let err = m.validate().expect_err("forkd transport mismatch rejected");
    assert!(err.message.contains("forkd requires tcp/forkd"));

    let mut rfb1 = m.clone();
    rfb1.backend = "rfb1".into();
    rfb1.transport = "tcp".into();
    rfb1.protocol = "rfb1".into();
    rfb1.guest_port = 5000;
    let err = rfb1
        .validate()
        .expect_err("rfb1 transport mismatch rejected");
    assert!(err.message.contains("rfb1 requires vsock/rfb1"));

    let mut zbrt = m;
    zbrt.backend = "zeroboot".into();
    zbrt.transport = "virtio-vsock".into();
    zbrt.protocol = "rfb1".into();
    zbrt.guest_port = 5000;
    let err = zbrt
        .validate()
        .expect_err("zeroboot protocol mismatch rejected");
    assert!(err.message.contains("zeroboot requires virtio-vsock/zbrt"));
}

#[test]
fn artifact_validate_rejects_wrong_guest_port_and_empty_entrypoint() {
    let mut m = ArtifactManifest {
        schema: "rfb-artifact/v1".into(),
        backend: "rfb1".into(),
        profile: "p".into(),
        arch: "x86_64".into(),
        transport: "vsock".into(),
        protocol: "rfb1".into(),
        entrypoint: "/init".into(),
        guest_port: 5000,
        kernel: None,
        rootfs: None,
        firecracker: None,
        vm: ArtifactVm {
            cpus: 1,
            memory_bytes: 0,
        },
        snapshot: None,
    };

    m.guest_port = 1234;
    let err = m.validate().expect_err("wrong port rejected");
    assert!(err.message.contains("rfb1 guest_port must be 5000"));

    m.guest_port = 5000;
    m.entrypoint = "   ".into();
    let err = m.validate().expect_err("empty entrypoint rejected");
    assert!(err.message.contains("entrypoint must not be empty"));
}

#[test]
fn artifact_local_file_checks_cover_missing_files_and_digests() {
    let dir = tempfile::tempdir().expect("temporary directory");

    // Schema validation tolerates a missing local file; local verification does not.
    let missing = ArtifactManifest::for_kernel(&dir.path().join("missing-kernel"), &digest())
        .expect("kernel artifact builds");
    assert!(missing.validate_schema().is_ok());
    let err = missing
        .verify_local_files()
        .expect_err("missing kernel rejected");
    assert!(err.message.contains("kernel local file is missing"));

    // Full validate() requires the kernel file to exist.
    let err = missing
        .validate()
        .expect_err("missing kernel fails validate");
    assert!(err.message.contains("local file is missing"));

    // A present file whose digest does not match is rejected.
    let kernel_path = dir.path().join("vmlinux");
    fs::write(&kernel_path, b"kernel-bytes").expect("write kernel");
    let wrong = sha256_bytes(b"other-bytes");
    let mismatched =
        ArtifactManifest::for_kernel(&kernel_path, &wrong).expect("kernel artifact builds");
    let err = mismatched.validate().expect_err("digest mismatch rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("artifact mismatch: kernel digest"));

    // Rootfs files are portable: a matching manifest validates without a local file.
    let rootfs = ArtifactManifest::for_rootfs(
        &dir.path().join("missing-rootfs"),
        &digest(),
        "rfb-vsock",
        "/sbin/rfb-runtime",
        "rfb1",
    )
    .expect("rootfs artifact builds");
    assert!(rootfs.validate_schema().is_ok());
    assert!(rootfs.verify_local_files().is_ok());
    assert!(rootfs.validate().is_ok());
}

#[test]
fn artifact_manifest_load_and_sidecar_round_trip() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let artifact_path = dir.path().join("image.ext4");
    let artifact = ArtifactManifest::for_rootfs(
        &artifact_path,
        &digest(),
        "rfb-vsock",
        "/sbin/rfb-runtime",
        "rfb1",
    )
    .expect("rootfs artifact builds");

    let sidecar = artifact
        .write_sidecar(&artifact_path)
        .expect("write sidecar");
    assert_eq!(sidecar, dir.path().join("image.ext4.artifact.json"));
    assert!(sidecar.is_file());

    let loaded = ArtifactManifest::load(&sidecar).expect("load sidecar");
    assert_eq!(loaded.backend, "rfb1");
    assert_eq!(loaded.profile, "rfb-runtime-rfb1-vsock");
    assert_eq!(loaded.guest_port, 5000);

    let loaded_local = ArtifactManifest::load_local(&sidecar).expect("load_local sidecar");
    assert_eq!(
        loaded_local.rootfs.as_ref().expect("rootfs").path,
        artifact_path.to_string_lossy().as_ref()
    );

    // A sidecar whose schema is wrong is rejected.
    let bad = dir.path().join("bad.json");
    write_json(
        &bad,
        &json!({
            "schema": "v9", "backend": "rfb1", "profile": "p", "arch": "x",
            "transport": "vsock", "protocol": "rfb1", "entrypoint": "/init",
            "guest_port": 5000, "vm": {"cpus": 1, "memory_bytes": 0},
        }),
    );
    let err = ArtifactManifest::load(&bad).expect_err("wrong schema rejected");
    assert!(err.message.contains("unsupported artifact schema"));

    // Garbage JSON is rejected with a clear validation message.
    let garbage = dir.path().join("garbage.json");
    fs::write(&garbage, "{ not json").expect("write garbage");
    let err = ArtifactManifest::load(&garbage).expect_err("garbage json rejected");
    assert!(err.message.contains("invalid artifact manifest"));
}

#[test]
fn artifact_mismatch_error_renders_all_context() {
    let err = ArtifactMismatch::Mismatch {
        expected: "abc".into(),
        observed: "def".into(),
        path: "/srv/rootfs.ext4".into(),
        kind: "digest".into(),
        next: "rebuild".into(),
    };
    let text = err.to_string();
    assert!(text.contains("artifact mismatch"));
    assert!(text.contains("/srv/rootfs.ext4"));
    assert!(text.contains("(digest)"));
    assert!(text.contains("expected abc"));
    assert!(text.contains("observed def"));
    assert!(text.contains("next: rebuild"));
}

// ---------------------------------------------------------------------------
// verify.rs: verification gates (offline portions)
// ---------------------------------------------------------------------------

#[test]
fn inspect_rootfs_reports_unknown_on_an_unreadable_image() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let image = dir.path().join("image.ext4");
    fs::write(&image, b"this is not an ext4 filesystem").expect("write garbage image");

    // `inspect_rootfs` never fails: when debugfs is absent (or fails on a
    // non-ext4 image) it returns the "unknown" defaults deterministically.
    let value = inspect_rootfs(&image).expect("inspect defaults instead of failing");
    assert_eq!(value["protocol_version"], "unknown");
    assert_eq!(value["entrypoint_regular"], false);
    assert_eq!(value["entrypoint_executable"], false);
}

#[test]
fn run_debugfs_errors_on_a_non_ext4_image() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let image = dir.path().join("image.ext4");
    fs::write(&image, b"garbage").expect("write garbage image");

    // Either debugfs is missing (external error) or it fails on a non-ext4
    // image (external error). Both are errors; never a hang.
    assert!(run_debugfs(&image, "cat /etc/missing", true).is_err());
}

#[test]
fn check_kernel_rejects_non_regular_files_before_readelf() {
    let dir = tempfile::tempdir().expect("temporary directory");

    let err = check_kernel(&dir.path().join("missing-kernel")).expect_err("missing rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err
        .message
        .contains("kernel is not a readable regular file"));

    let err = check_kernel(dir.path()).expect_err("directory rejected");
    assert!(err
        .message
        .contains("kernel is not a readable regular file"));
}

#[test]
fn check_kernel_never_succeeds_on_garbage_bytes() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let kernel = dir.path().join("vmlinux");
    fs::write(&kernel, b"not an elf binary").expect("write garbage kernel");

    // Deterministic rejection regardless of environment: with readelf absent we
    // get the external "readelf is required" error; with readelf present the
    // header parse of garbage fails with a validation error. Either way: Err.
    let result = check_kernel(&kernel);
    assert!(result.is_err(), "garbage kernel must not validate");
}

#[test]
fn build_static_runtime_rejects_empty_arguments_before_cargo() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for (target, package) in [("", "forkd-agent"), ("x86_64-unknown-linux-musl", "")] {
        let err = build_static_runtime(dir.path(), target, package, "cli")
            .expect_err("empty target/package rejected");
        assert_eq!(err.code, EXIT_VALIDATION);
        assert!(err.message.contains("target and package must not be empty"));
    }
    let err = build_static_runtime(dir.path(), "x86_64-unknown-linux-musl", "rfb-runtime", "  ")
        .expect_err("empty features rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("features must not be empty"));
}

// ---------------------------------------------------------------------------
// build.rs: init, dry-run build, execute gates, diagnostics
// ---------------------------------------------------------------------------

#[test]
fn image_init_creates_manifest_and_staging_directory() {
    let parent = tempfile::tempdir().expect("temporary directory");
    let directory = parent.path().join("new-image");
    let value = init(&directory, 8192, false).expect("init succeeds");
    assert_eq!(value["ok"], true);

    let manifest_path = directory.join("manifest.json");
    assert!(manifest_path.is_file());
    assert!(directory.join("staging").is_dir());

    let manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest")).expect("json");
    assert_eq!(manifest["format"], "rfb-cli-staging/v1");
    assert_eq!(manifest["image_size_bytes"], 8192);
    assert_eq!(manifest["block_size"], 4096);
    assert_eq!(manifest["staging"], "staging");
}

#[test]
fn image_init_rejects_bad_sizes_and_existing_directories() {
    let parent = tempfile::tempdir().expect("temporary directory");
    let directory = parent.path().join("img");

    let err = init(&directory, 0, false).expect_err("zero size rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err
        .message
        .contains("--size must be a non-zero multiple of 4096"));

    let err = init(&directory, 1000, false).expect_err("non-multiple size rejected");
    assert!(err
        .message
        .contains("--size must be a non-zero multiple of 4096"));

    // Existing directory is rejected without --force ...
    let existing = tempfile::tempdir().expect("temporary directory");
    let err = init(existing.path(), 4096, false).expect_err("existing dir rejected");
    assert_eq!(err.code, EXIT_IO);
    assert!(err.message.contains("directory exists"));

    // ... and allowed with --force.
    assert!(init(existing.path(), 4096, true).is_ok());
    assert!(existing.path().join("staging").is_dir());
}

#[test]
fn image_build_dry_run_reports_readiness_without_tools() {
    let (_dir, manifest) = valid_staging_tree();
    let value = build(&manifest, None, false).expect("dry-run build succeeds");

    assert_eq!(value["ok"], true);
    assert_eq!(value["dry_run"], true);
    let status = value["status"].as_str().expect("status present");
    assert!(status == "READY" || status == "SKIP", "status: {status}");
    assert_eq!(
        value["output"].as_str().expect("output present"),
        manifest.with_extension("img").to_string_lossy().as_ref()
    );
    assert_eq!(value["image_size_bytes"], 4096);
    assert_eq!(value["delegation"], json!(["mke2fs", "debugfs"]));
    assert!(value["tools"]["mke2fs"].is_boolean());
    assert!(value["tools"]["debugfs"].is_boolean());

    // A caller-supplied output path is honored in the dry run.
    let custom = _dir.path().join("custom.img");
    let value = build(&manifest, Some(&custom), false).expect("dry-run with output");
    assert_eq!(
        value["output"].as_str().expect("output present"),
        custom.to_string_lossy().as_ref()
    );
}

#[test]
fn image_build_dry_run_rejects_a_bad_manifest() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = dir.path().join("manifest.json");
    write_json(
        &manifest,
        &json!({
            "format": "rfb-cli-staging/v1", "image_size_bytes": 5000, "block_size": 4096,
            "staging": "staging", "files": [],
        }),
    );
    let err = build(&manifest, None, false).expect_err("bad manifest rejected");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err.message.contains("non-zero multiple of block_size"));
}

#[test]
fn image_build_execute_requires_an_image_manifest() {
    let (_dir, manifest) = valid_staging_tree();
    let out = _dir.path().join("out.img");
    let err = build(&manifest, Some(&out), true).expect_err("image required");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err
        .message
        .contains("requires an image manifest with an entrypoint"));
}

#[test]
fn image_build_execute_requires_a_non_empty_entrypoint() {
    let dir = tempfile::tempdir().expect("temporary directory");
    fs::create_dir_all(dir.path().join("staging")).expect("staging directory");
    let bytes = b"#!/bin/sh\nexit 0\n";
    fs::write(dir.path().join("staging/init"), bytes).expect("write staging init");
    let manifest = write_staging_manifest(
        dir.path(),
        json!([{"path": "init", "size": bytes.len(), "sha256": sha256_bytes(bytes), "source": "init"}]),
        json!({
            "image_ref": "rfb-runtime", "transport": "vsock", "entrypoint": [],
            "protocol": "rfb1", "arch": "x86_64", "resources": {},
        }),
    );
    let out = dir.path().join("out.img");
    let err = build(&manifest, Some(&out), true).expect_err("empty entrypoint rejected");
    assert!(err
        .message
        .contains("requires a non-empty image entrypoint"));
}

#[test]
fn image_build_execute_rejects_an_entrypoint_absent_from_files() {
    let dir = tempfile::tempdir().expect("temporary directory");
    fs::create_dir_all(dir.path().join("staging")).expect("staging directory");
    let manifest = write_staging_manifest(
        dir.path(),
        json!([]),
        json!({
            "image_ref": "rfb-runtime", "transport": "vsock", "entrypoint": ["/init"],
            "protocol": "rfb1", "arch": "x86_64", "resources": {},
        }),
    );
    let out = dir.path().join("out.img");
    let err = build(&manifest, Some(&out), true)
        .expect_err("absent entrypoint rejected before external tools");
    assert_eq!(err.code, EXIT_VALIDATION);
    assert!(err
        .message
        .contains("entrypoint is not present in staging files"));
}

#[test]
fn image_diagnostics_reports_unknown_without_an_image() {
    let (_dir, manifest) = valid_staging_tree();
    let parsed = load(&manifest).expect("loads");
    let (transport, protocol, capabilities, profile) = image_diagnostics(&parsed);
    assert_eq!(transport, "unknown");
    assert_eq!(protocol, "unknown");
    assert!(capabilities.is_empty());
    assert_eq!(profile, "none");
}

#[test]
fn image_diagnostics_reports_profile_and_capabilities() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let manifest = write_staging_manifest(
        dir.path(),
        json!([]),
        json!({
            "image_ref": "x", "transport": "vsock", "entrypoint": ["/init"],
            "protocol": "rfb1", "arch": "x86_64", "profile": "forkd-agent",
            "capabilities": ["execute", "health"], "resources": {},
        }),
    );
    let parsed = load(&manifest).expect("loads");
    let (transport, protocol, capabilities, profile) = image_diagnostics(&parsed);
    assert_eq!(transport, "vsock");
    assert_eq!(protocol, "rfb1");
    assert_eq!(
        capabilities,
        vec!["execute".to_string(), "health".to_string()]
    );
    assert_eq!(profile, "forkd-agent");

    // Without an explicit profile or capabilities the defaults are used.
    let manifest = write_staging_manifest(
        dir.path(),
        json!([]),
        json!({
            "image_ref": "x", "transport": "tcp", "entrypoint": ["/forkd-init.sh"],
            "protocol": "forkd", "arch": "x86_64", "resources": {},
        }),
    );
    let parsed = load(&manifest).expect("loads");
    let (_transport, _protocol, capabilities, profile) = image_diagnostics(&parsed);
    assert_eq!(capabilities, vec!["execute".to_string()]);
    assert_eq!(profile, "custom");
}

#[test]
fn profile_summary_lists_all_named_profiles() {
    let value = profile_summary();
    let profiles = value.as_array().expect("profile array");
    assert_eq!(profiles.len(), 2);
    assert_eq!(profiles[0]["profile"], "rfb-runtime-rfb1-vsock");
    assert_eq!(profiles[0]["transport"], "vsock");
    assert_eq!(profiles[0]["protocol"], "rfb1");
    assert_eq!(profiles[0]["status"], "supported");
    assert_eq!(profiles[1]["profile"], "zeroboot-zbrt");
    assert_eq!(profiles[1]["transport"], "virtio-vsock");
    assert_eq!(profiles[1]["protocol"], "zbrt");
    assert_eq!(profiles[1]["guest_port"], 5000);
    assert_eq!(profiles[1]["capabilities"], json!(["execute"]));
}
