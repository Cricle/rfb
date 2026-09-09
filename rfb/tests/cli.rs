#![cfg(feature = "cli")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rfb-cli"))
}

fn run(args: &[&str]) -> Output {
    cli().args(args).output().expect("run rfb-cli")
}

fn write_manifest(path: &Path, image: Option<serde_json::Value>, files: serde_json::Value) {
    let manifest = serde_json::json!({
        "format": "rfb-cli-staging/v1",
        "image_size_bytes": 4096,
        "block_size": 4096,
        "staging": "staging",
        "files": files,
        "image": image,
    });
    fs::write(
        path,
        serde_json::to_vec(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");
}

#[test]
fn help_and_version_are_stable_binary_contracts() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("RFB image staging workflow"));
    assert!(help.contains("image"));
    assert!(help.contains("Exit codes:"));

    let output = run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "rfb-cli 0.1.0"
    );
}

#[test]
fn usage_errors_use_exit_two_and_json_errors_are_machine_readable() {
    let output = run(&["image"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage:"));

    let output = run(&["--json", "image", "validate", "missing.json"]);
    assert_eq!(output.status.code(), Some(4));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json error");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], 4);
    assert!(value["error"]["message"].as_str().is_some());
}

#[test]
fn bench_list_honors_json_contract() {
    let output = run(&["--json", "bench", "list"]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json output");
    assert_eq!(value["ok"], true);
    assert_eq!(value["benchmarks"][0], "rfb-runtime/codec");
}

#[test]
fn image_init_rejects_an_existing_directory_without_force() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let output = run(&["image", "init", directory.path().to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("directory exists"));
}

#[test]
fn image_validate_rejects_missing_and_unsafe_paths() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("missing.json");
    let output = run(&["image", "validate", missing.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(4));

    let manifest = directory.path().join("manifest.json");
    fs::create_dir(directory.path().join("staging")).expect("staging directory");
    write_manifest(
        &manifest,
        None,
        serde_json::json!([{"path":"../outside","size":0,"sha256":"","source":"outside"}]),
    );
    let output = run(&["image", "validate", manifest.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe file path"));
}

#[test]
fn image_validate_rejects_unknown_manifest_fields() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("staging")).expect("staging directory");
    let manifest = directory.path().join("manifest.json");
    let value = serde_json::json!({
        "format": "rfb-cli-staging/v1",
        "image_size_bytes": 4096,
        "block_size": 4096,
        "staging": "staging",
        "files": [],
        "future_field": true
    });
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize manifest"),
    )
    .expect("write manifest");

    let output = run(&["image", "validate", manifest.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field"));
}

#[test]
fn image_validate_accepts_and_validates_the_core_manifest() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("staging")).expect("staging directory");
    let manifest = directory.path().join("manifest.json");
    let image = serde_json::json!({
        "image_ref": "registry.example/app:latest",
        "transport": "oci",
        "entrypoint": ["/init"],
        "protocol": "rfb",
        "arch": "x86_64",
        "resources": {}
    });
    write_manifest(&manifest, Some(image), serde_json::json!([]));

    let output = run(&["image", "validate", manifest.to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "valid");

    let invalid_manifest = directory.path().join("invalid.json");
    write_manifest(
        &invalid_manifest,
        Some(serde_json::json!({
            "image_ref": "",
            "transport": "oci",
            "entrypoint": [],
            "protocol": "rfb",
            "arch": "x86_64",
            "resources": {}
        })),
        serde_json::json!([]),
    );
    let output = run(&["image", "validate", invalid_manifest.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("image_ref must not be empty"),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn doctor_reports_transport_protocol_and_capabilities() {
    let output = run(&["--json", "doctor"]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert!(value["transport"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "vsock"));
    assert!(value["protocol"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "rfb1"));
    assert!(value["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "execute"));
    assert_eq!(
        value["transport"],
        serde_json::json!(["vsock", "virtio-vsock"])
    );
    assert_eq!(value["protocol"], serde_json::json!(["rfb1", "zbrt"]));
    assert_eq!(value["profiles"][1]["profile"], "zeroboot-zbrt");
}

#[test]
fn image_inspect_reports_profile_diagnostics() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("staging")).expect("staging directory");
    let manifest = directory.path().join("manifest.json");
    write_manifest(
        &manifest,
        Some(serde_json::json!({
            "image_ref": "rfb-runtime", "transport": "vsock", "entrypoint": ["/sbin/rfb-runtime"],
            "protocol": "rfb1", "arch": "x86_64", "profile": "rfb-runtime-rfb1-vsock",
            "capabilities": ["execute"], "resources": {}
        })),
        serde_json::json!([]),
    );
    let output = run(&["--json", "image", "inspect", manifest.to_str().unwrap()]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["transport"], "vsock");
    assert_eq!(value["protocol"], "rfb1");
    assert_eq!(value["profile"], "rfb-runtime-rfb1-vsock");
}

#[test]
fn image_build_execute_rejects_missing_entrypoint_before_external_tools() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("staging")).expect("staging directory");
    let manifest = directory.path().join("manifest.json");
    write_manifest(
        &manifest,
        Some(serde_json::json!({
            "image_ref": "rfb-runtime", "transport": "vsock", "entrypoint": ["/sbin/rfb-runtime"],
            "protocol": "rfb1", "arch": "x86_64", "profile": "rfb-runtime-rfb1-vsock",
            "resources": {}
        })),
        serde_json::json!([]),
    );

    let output = run(&["image", "build", manifest.to_str().unwrap(), "--execute"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("entrypoint is not present"));
}

/// Positive end-to-end: with a real staging file and the e2fsprogs tools
/// available, `image build --execute` creates a valid ext4 image, writes the
/// entrypoint, and reports a digest. Skipped where the tools are absent.
#[test]
#[cfg(unix)]
fn image_build_execute_creates_ext4_and_reports_digest() {
    fn available(cmd: &str) -> bool {
        Command::new(cmd)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    if !available("mke2fs") || !available("debugfs") {
        eprintln!("skipping: mke2fs/debugfs unavailable");
        return;
    }

    let directory = tempfile::tempdir().expect("temporary directory");
    let staging = directory.path().join("staging");
    fs::create_dir_all(&staging).expect("staging directory");
    // A tiny executable entrypoint the image will ship at /init.
    let init_bytes = b"#!/bin/sh\nexit 0\n";
    fs::write(staging.join("init"), init_bytes).expect("write staging init");
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(init_bytes);
        format!("{:x}", hasher.finalize())
    };
    let manifest = directory.path().join("manifest.json");
    write_manifest(
        &manifest,
        Some(serde_json::json!({
            "image_ref": "rfb-runtime", "transport": "vsock", "entrypoint": ["/sbin/rfb-runtime"],
            "protocol": "rfb1", "arch": "x86_64", "profile": "rfb-runtime-rfb1-vsock",
            "resources": {}
        })),
        serde_json::json!([{
            "path": "init",
            "size": init_bytes.len(),
            "sha256": digest,
            "source": "init"
        }]),
    );

    let output_img = directory.path().join("built.img");
    let output = run(&[
        "image",
        "build",
        manifest.to_str().unwrap(),
        "--execute",
        "--output",
        output_img.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "image build --execute failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("digest: sha256:"), "missing digest: {text}");
    assert!(output_img.exists(), "output image was not created");

    // debugfs can read the entrypoint back from the built image.
    let readback = Command::new("debugfs")
        .args(["-R", "cat /init"])
        .arg(&output_img)
        .output()
        .expect("run debugfs");
    assert!(readback.status.success(), "debugfs cat /init failed");
    assert_eq!(readback.stdout, init_bytes, "entrypoint bytes mismatch");
}

#[test]
fn custom_forkd_agent_manifest_preserves_entrypoint_protocol_and_capabilities() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("staging")).expect("staging directory");
    let manifest = directory.path().join("manifest.json");
    write_manifest(
        &manifest,
        Some(serde_json::json!({
            "image_ref": "registry.example/custom-agent:latest",
            "transport": "tcp", "entrypoint": ["/forkd-init.sh", "--serve"],
            "protocol": "forkd", "arch": "x86_64", "profile": "forkd-agent",
            "capabilities": ["execute", "health", "stream"], "resources": {}
        })),
        serde_json::json!([]),
    );
    let output = run(&["--json", "image", "inspect", manifest.to_str().unwrap()]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["entrypoint"][0], "/forkd-init.sh");
    assert_eq!(value["protocol"], "forkd");
    assert_eq!(value["capabilities"][2], "stream");

    let output = run(&["image", "validate", manifest.to_str().unwrap()]);
    assert!(output.status.success());
}

#[test]
fn forkd_profile_rejects_rfb1_vsock_image() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("staging")).expect("staging directory");
    let manifest = directory.path().join("manifest.json");
    write_manifest(
        &manifest,
        Some(serde_json::json!({
            "image_ref": "custom-image", "transport": "vsock", "entrypoint": ["/sbin/agent"],
            "protocol": "rfb1", "arch": "x86_64", "profile": "forkd-agent",
            "capabilities": ["execute"], "resources": {}
        })),
        serde_json::json!([]),
    );
    let output = run(&["image", "validate", manifest.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("rfb-runtime vsock"));
}

#[test]
fn image_init_creates_manifest_and_staging_for_a_new_path() {
    let parent = tempfile::tempdir().expect("temporary directory");
    let directory: PathBuf = parent.path().join("new-image");
    let output = run(&["image", "init", directory.to_str().unwrap()]);

    assert!(output.status.success());
    assert!(directory.join("manifest.json").is_file());
    assert!(directory.join("staging").is_dir());
}

#[test]
fn forkd_preflight_contract_blocked_and_require_vm_exit() {
    // Mirrors the former tests/scripts.sh preflight contract:
    // text mode reports BLOCKED, JSON mode reports blocked with the standard
    // check names, and --require-vm exits 12 when prerequisites are missing.
    let output = run(&["forkd", "preflight", "--url", "http://127.0.0.1:1"]);
    assert!(
        output.status.success(),
        "preflight must not fail without --require-vm"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("RFB preflight: BLOCKED"), "text: {text}");
    assert!(text.contains("platform"), "text: {text}");

    let output = run(&[
        "--json",
        "forkd",
        "preflight",
        "--url",
        "http://127.0.0.1:1",
    ]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["status"], "blocked");
    assert_eq!(value["snapshot_tag"], "rfb");
    let names: Vec<&str> = value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    for expected in [
        "platform",
        "architecture",
        "kvm",
        "forkd_binary",
        "forkd_rootfs",
        "forkd_controller",
        "forkd_snapshot",
    ] {
        assert!(
            names.contains(&expected),
            "missing check {expected}: {names:?}"
        );
    }

    let output = run(&[
        "forkd",
        "preflight",
        "--require-vm",
        "--url",
        "http://127.0.0.1:1",
    ]);
    assert_eq!(output.status.code(), Some(12));
}

#[test]
fn forkd_snapshot_commands_are_documented_in_help() {
    for sub in ["snapshot-create", "snapshot-info", "snapshot-delete"] {
        let output = run(&["forkd", sub, "--help"]);
        assert!(
            output.status.success(),
            "{sub} --help failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("--tag"), "{sub} help missing --tag");
        assert!(
            help.contains("--forkd-bin"),
            "{sub} help missing --forkd-bin"
        );
    }
}

#[test]
fn forkd_snapshot_commands_validate_inputs_before_delegation() {
    // A non-loopback controller URL is rejected without invoking forkd.
    let output = run(&[
        "forkd",
        "snapshot-info",
        "--url",
        "http://evil.example.com",
        "--tag",
        "snap",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("localhost"));

    // A malformed snapshot tag is rejected before delegation.
    let output = run(&["forkd", "snapshot-info", "--tag", "bad tag"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid snapshot tag"));

    // A missing forkd binary is a validation error, not a spawn attempt.
    let output = run(&[
        "forkd",
        "snapshot-info",
        "--tag",
        "snap",
        "--forkd-bin",
        "/no/such/forkd",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("forkd binary"));

    // snapshot-create validates numeric options before asset resolution.
    let output = run(&[
        "forkd",
        "snapshot-create",
        "--tag",
        "snap",
        "--boot-wait-secs",
        "0",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("boot-wait-secs"));
}

#[test]
fn forkd_snapshot_create_rejects_missing_assets_before_delegation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fake = directory.path().join("forkd");
    fs::write(&fake, b"#!/bin/sh\nexit 0\n").expect("write fake forkd");
    let output = run(&[
        "forkd",
        "snapshot-create",
        "--tag",
        "snap",
        "--forkd-bin",
        fake.to_str().unwrap(),
        "--kernel",
        "/no/such/kernel",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("kernel"));

    let kernel = directory.path().join("vmlinux");
    fs::write(&kernel, b"ELF").expect("write kernel");
    let output = run(&[
        "forkd",
        "snapshot-create",
        "--tag",
        "snap",
        "--forkd-bin",
        fake.to_str().unwrap(),
        "--kernel",
        kernel.to_str().unwrap(),
        "--rootfs",
        "/no/such/rootfs",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("rootfs"));
}

#[test]
fn forkd_snapshot_delete_validates_flags_and_tag() {
    // A missing --tag is a clap usage error.
    let output = run(&["forkd", "snapshot-delete"]);
    assert_eq!(output.status.code(), Some(2));

    // --force and --cascade are mutually exclusive.
    let output = run(&[
        "forkd",
        "snapshot-delete",
        "--tag",
        "snap",
        "--force",
        "--cascade",
    ]);
    assert_eq!(output.status.code(), Some(2));

    // A non-loopback URL is rejected before delegation.
    let output = run(&[
        "forkd",
        "snapshot-delete",
        "--tag",
        "snap",
        "--url",
        "http://evil.example.com",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("localhost"));
}

#[cfg(unix)]
mod forkd_snapshot_delegation {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `forkd` binary that answers the snapshot subcommands our wrapper
    /// delegates to. The snapshot-info payload carries extra fields on purpose:
    /// the wrapper must sanitize them out of the report.
    fn fake_forkd(directory: &Path) -> PathBuf {
        let path = directory.join("forkd");
        let script = r#"#!/bin/sh
case "$1" in
  snapshot-info)
    echo '{"tag":"snap","status":"ready","bootable":true,"digest":"sha256:abc","dir":"/home/user/.local/share/forkd/snapshots/snap","provenance":{"status":"complete","kernel_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","memory_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","vmstate_sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}}'
    ;;
  snapshot)
    echo "snapshot created"
    ;;
  rmi)
    echo "removed"
    ;;
  *)
    echo "unexpected args: $*" >&2
    exit 1
    ;;
esac
"#;
        fs::write(&path, script).expect("write fake forkd");
        let mut perms = fs::metadata(&path).expect("stat fake forkd").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod fake forkd");
        path
    }

    #[test]
    fn snapshot_info_delegates_and_sanitizes_output() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fake = fake_forkd(directory.path());
        let output = run(&[
            "--json",
            "forkd",
            "snapshot-info",
            "--tag",
            "snap",
            "--forkd-bin",
            fake.to_str().unwrap(),
        ]);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(value["tag"], "snap");
        assert_eq!(value["provenance"], "complete");
        assert!(
            value.get("dir").is_none(),
            "snapshot directory must not leak"
        );
        assert!(
            value.get("kernel_sha256").is_none(),
            "provenance digests must not leak verbatim"
        );
    }

    #[test]
    fn snapshot_info_require_provenance_fails_closed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fake = directory.path().join("forkd");
        fs::write(
            &fake,
            "#!/bin/sh\ncase \"$1\" in\n  snapshot-info)\n    echo '{\"tag\":\"snap\",\"status\":\"ready\",\"bootable\":true,\"provenance\":{\"status\":\"partial\"}}'\n    ;;\n  *) exit 1 ;;\nesac\n",
        )
        .expect("write fake forkd");
        let mut perms = fs::metadata(&fake).expect("stat fake forkd").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake, perms).expect("chmod fake forkd");

        let output = run(&[
            "forkd",
            "snapshot-info",
            "--tag",
            "snap",
            "--forkd-bin",
            fake.to_str().unwrap(),
            "--require-provenance",
        ]);
        assert_eq!(output.status.code(), Some(3));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("fails closed"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn snapshot_create_delegates_and_reports_info() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fake = fake_forkd(directory.path());
        let kernel = directory.path().join("vmlinux");
        fs::write(&kernel, b"ELF").expect("write kernel");
        let rootfs = directory.path().join("forkd-agent.ext4");
        fs::write(&rootfs, b"ext4").expect("write rootfs");

        let output = run(&[
            "--json",
            "forkd",
            "snapshot-create",
            "--tag",
            "snap",
            "--forkd-bin",
            fake.to_str().unwrap(),
            "--kernel",
            kernel.to_str().unwrap(),
            "--rootfs",
            rootfs.to_str().unwrap(),
        ]);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(value["ok"], true);
        assert_eq!(value["created"], true);
        assert_eq!(value["snapshot_info"]["tag"], "snap");
        assert_eq!(value["snapshot_info"]["provenance"], "complete");
    }

    #[test]
    fn snapshot_create_stages_private_rootfs_copy_and_passes_it_to_forkd() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fake = directory.path().join("forkd");
        let log = directory.path().join("forkd-args.log");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> "{}"
case "$1" in
  snapshot)
    echo "snapshot created"
    ;;
  snapshot-info)
    echo '{{"tag":"snap","status":"ready","bootable":true,"provenance":{{"status":"complete","kernel_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","memory_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","vmstate_sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}}}}'
    ;;
  *) exit 1 ;;
esac
"#,
            log.display()
        );
        fs::write(&fake, script).expect("write fake forkd");
        let mut perms = fs::metadata(&fake).expect("stat fake forkd").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake, perms).expect("chmod fake forkd");

        let kernel = directory.path().join("vmlinux");
        fs::write(&kernel, b"ELF").expect("write kernel");
        let rootfs = directory.path().join("forkd-agent.ext4");
        fs::write(&rootfs, b"pristine-ext4").expect("write rootfs");
        let copy = directory.path().join("custom-copy.ext4");

        let output = cli()
            .args([
                "--json",
                "forkd",
                "snapshot-create",
                "--tag",
                "snap",
                "--forkd-bin",
                fake.to_str().unwrap(),
                "--kernel",
                kernel.to_str().unwrap(),
                "--rootfs",
                rootfs.to_str().unwrap(),
                "--rootfs-copy",
                copy.to_str().unwrap(),
            ])
            .output()
            .expect("run rfb-cli");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(
            value["rootfs_copy"],
            serde_json::Value::String(copy.to_string_lossy().into_owned())
        );
        // The forkd binary received the copy path, never the artifact original.
        let log_contents = fs::read_to_string(&log).expect("read forkd args log");
        let snapshot_line = log_contents
            .lines()
            .find(|line| line.contains("--rootfs"))
            .expect("forkd snapshot args");
        assert!(
            snapshot_line.contains(&format!("--rootfs {}", copy.display())),
            "forkd received: {snapshot_line}"
        );
        // The artifact rootfs stays byte-identical (rw boot touched the copy).
        assert_eq!(fs::read(&rootfs).expect("read rootfs"), b"pristine-ext4");
        assert!(copy.is_file(), "rootfs copy must exist");
    }

    #[test]
    fn snapshot_delete_delegates_to_rmi() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fake = fake_forkd(directory.path());
        let output = run(&[
            "--json",
            "forkd",
            "snapshot-delete",
            "--tag",
            "snap",
            "--forkd-bin",
            fake.to_str().unwrap(),
        ]);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(value["ok"], true);
        assert_eq!(value["deleted"], true);
    }
}

#[test]
fn image_build_rootfs_rejects_missing_runtime_before_external_tools() {
    // The thin-wrapper build-image.sh contract: a nonexistent runtime is a
    // validation error (exit 3) before any tool delegation.
    let output = run(&[
        "image",
        "build-rootfs",
        "/tmp/does-not-exist",
        "/tmp/rfb-script-contract.ext4",
        "--mode",
        "forkd-agent",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a readable regular file"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn web_bench_reads_api_key_env_from_config_and_requires_it() {
    // The former rfb-web-concurrency.sh contract: the config names a model-key
    // environment variable; if that variable is absent the bench blocks (exit
    // 3) without touching any network or process. Fully deterministic.
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = directory.path().join("config.json");
    fs::write(
        &config,
        r#"{"rig": {"api_key_env": "RFB_TEST_WEB_BENCH_MISSING_KEY"}}"#,
    )
    .expect("write config");
    let output = run(&[
        "web",
        "bench",
        "--config",
        config.to_str().unwrap(),
        "--pid",
        "1",
    ]);
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "required model key environment variable is absent: RFB_TEST_WEB_BENCH_MISSING_KEY"
        ),
        "stderr: {stderr}"
    );
}
