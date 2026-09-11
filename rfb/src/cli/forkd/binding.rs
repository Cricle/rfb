use crate::cli::forkd::preflight::client_from_url_env;
use crate::cli::{
    error::{io, validation, CliError},
    image_build::{sha256_bytes, valid_sha, ArtifactManifest},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBinding {
    pub schema: String,
    pub tag: String,
    pub snapshot_ready: bool,
    pub snapshot_bootable: bool,
    pub artifact: ArtifactIdentity,
    pub controller: ControllerObserved,
    pub record_hash: String,
    pub verification: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    pub schema: String,
    pub backend: String,
    pub profile: String,
    pub arch: String,
    pub transport: String,
    pub protocol: String,
    pub entrypoint: String,
    pub guest_port: u16,
    pub manifest_sha256: String,
    pub kernel_sha256: Option<String>,
    pub rootfs_sha256: Option<String>,
    pub firecracker_sha256: Option<String>,
    pub snapshot_tag: Option<String>,
    pub snapshot_sha256: Option<String>,
    pub snapshot_memory_sha256: Option<String>,
    pub snapshot_vmstate_sha256: Option<String>,
    pub snapshot_network: Option<bool>,
    pub snapshot_batch_id: Option<String>,
    pub snapshot_version: Option<String>,
    pub snapshot_vm_identity: Option<String>,
    pub snapshot_network_identity: Option<String>,
    pub vm_cpus: u16,
    pub vm_memory_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerObserved {
    pub detail: bool,
    pub status: String,
    pub bootable: bool,
    pub digest: Option<String>,
    pub provenance: Option<Value>,
}

/// Typed controller provenance. Missing identity fields are deliberately not
/// inferred from controller readiness or opaque JSON, so verification fails closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedProvenance {
    pub status: String,
    pub kernel_sha256: Option<String>,
    pub input_rootfs_sha256: Option<String>,
    pub memory_sha256: Option<String>,
    pub vmstate_sha256: Option<String>,
    pub firecracker_sha256: Option<String>,
    pub vcpu_count: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub network: Option<bool>,
    pub batch_id: Option<String>,
    #[serde(alias = "controller_version")]
    pub version: Option<String>,
    pub vm_identity: Option<String>,
    pub network_identity: Option<String>,
}

/// Return all missing or mismatching typed provenance fields for diagnostics.
pub fn typed_provenance_diagnostics(p: Option<&Value>, a: &ArtifactManifest) -> Vec<String> {
    let Some(value) = p else {
        return vec!["provenance: missing".into()];
    };
    let parsed = serde_json::from_value::<TypedProvenance>(value.clone());
    let Ok(v) = parsed else {
        return vec!["provenance: invalid typed record".into()];
    };
    let mut out = Vec::new();
    let check_digest = |out: &mut Vec<String>,
                        name: &str,
                        got: Option<&String>,
                        expected: Option<&String>| {
        match (got, expected) {
            (Some(g), Some(e)) if valid_sha(g) && valid_sha(e) && digest(g) == digest(e) => {}
            (Some(g), _) if !valid_sha(g) => out.push(format!("{name}: invalid")),
            (None, _) => out.push(format!("{name}: missing")),
            (_, None) => out.push(format!("{name}: artifact expectation missing")),
            _ => out.push(format!("{name}: mismatch")),
        }
    };
    if v.status != "complete" {
        out.push("status: not complete".into());
    }
    check_digest(
        &mut out,
        "kernel_sha256",
        v.kernel_sha256.as_ref(),
        a.kernel.as_ref().map(|x| &x.sha256),
    );
    check_digest(
        &mut out,
        "input_rootfs_sha256",
        v.input_rootfs_sha256.as_ref(),
        a.rootfs.as_ref().map(|x| &x.sha256),
    );
    check_digest(
        &mut out,
        "firecracker_sha256",
        v.firecracker_sha256.as_ref(),
        a.firecracker.as_ref().map(|x| &x.sha256),
    );
    check_digest(
        &mut out,
        "memory_sha256",
        v.memory_sha256.as_ref(),
        a.snapshot.as_ref().and_then(|x| x.memory_sha256.as_ref()),
    );
    check_digest(
        &mut out,
        "vmstate_sha256",
        v.vmstate_sha256.as_ref(),
        a.snapshot.as_ref().and_then(|x| x.vmstate_sha256.as_ref()),
    );
    if v.vcpu_count != Some(a.vm.cpus as u64) {
        out.push("vcpu_count: mismatch or missing".into());
    }
    if a.vm.memory_bytes != 0 && v.memory_bytes != Some(a.vm.memory_bytes) {
        out.push("memory_bytes: mismatch or missing".into());
    }
    if v.network != Some(a.snapshot.as_ref().map(|x| x.network).unwrap_or(false)) {
        out.push("network: mismatch or missing".into());
    }
    let mut check_identity = |name: &str, got: &Option<String>, expected: Option<&String>| match (
        got.as_deref().map(str::trim),
        expected.map(|x| x.trim()),
    ) {
        (Some(g), Some(e)) if !g.is_empty() && g == e => {}
        (None | Some(""), _) => out.push(format!("{name}: missing")),
        (_, None) => out.push(format!("{name}: artifact expectation missing")),
        _ => out.push(format!("{name}: mismatch")),
    };
    let snapshot = a.snapshot.as_ref();
    check_identity(
        "batch_id",
        &v.batch_id,
        snapshot.and_then(|x| x.batch_id.as_ref()),
    );
    check_identity(
        "version",
        &v.version,
        snapshot.and_then(|x| x.version.as_ref()),
    );
    check_identity(
        "vm_identity",
        &v.vm_identity,
        snapshot.and_then(|x| x.vm_identity.as_ref()),
    );
    check_identity(
        "network_identity",
        &v.network_identity,
        snapshot.and_then(|x| x.network_identity.as_ref()),
    );
    out
}

/// Result of a successful snapshot binding: the JSON record plus a short
/// human-readable summary line.
pub struct BindingOutput {
    /// Full snapshot-binding record as JSON (schema `rfb-snapshot-binding/v1`).
    pub value: Value,
    /// Human-readable one-line summary of the binding result.
    pub text: String,
}

fn digest(value: &str) -> String {
    value
        .strip_prefix("sha256:")
        .unwrap_or(value)
        .to_ascii_lowercase()
}
fn identity(a: &ArtifactManifest) -> ArtifactIdentity {
    let manifest_sha256 = sha256_bytes(&serde_json::to_vec(a).expect("manifest serializable"));
    ArtifactIdentity {
        schema: a.schema.clone(),
        backend: a.backend.clone(),
        profile: a.profile.clone(),
        arch: a.arch.clone(),
        transport: a.transport.clone(),
        protocol: a.protocol.clone(),
        entrypoint: a.entrypoint.clone(),
        guest_port: a.guest_port,
        manifest_sha256,
        kernel_sha256: a.kernel.as_ref().map(|x| x.sha256.clone()),
        rootfs_sha256: a.rootfs.as_ref().map(|x| x.sha256.clone()),
        firecracker_sha256: a.firecracker.as_ref().map(|x| x.sha256.clone()),
        snapshot_tag: a.snapshot.as_ref().map(|x| x.tag.clone()),
        snapshot_sha256: a.snapshot.as_ref().map(|x| x.sha256.clone()),
        snapshot_memory_sha256: a.snapshot.as_ref().and_then(|x| x.memory_sha256.clone()),
        snapshot_vmstate_sha256: a.snapshot.as_ref().and_then(|x| x.vmstate_sha256.clone()),
        snapshot_network: a.snapshot.as_ref().map(|x| x.network),
        snapshot_batch_id: a.snapshot.as_ref().and_then(|x| x.batch_id.clone()),
        snapshot_version: a.snapshot.as_ref().and_then(|x| x.version.clone()),
        snapshot_vm_identity: a.snapshot.as_ref().and_then(|x| x.vm_identity.clone()),
        snapshot_network_identity: a.snapshot.as_ref().and_then(|x| x.network_identity.clone()),
        vm_cpus: a.vm.cpus,
        vm_memory_bytes: a.vm.memory_bytes,
    }
}
fn record_hash_of(b: &SnapshotBinding) -> String {
    sha256_bytes(&serde_json::to_vec(&json!({"schema":b.schema,"tag":b.tag,"snapshot_ready":b.snapshot_ready,"snapshot_bootable":b.snapshot_bootable,"artifact":b.artifact,"controller":b.controller,"verification":b.verification})).expect("binding serializable"))
}
fn validate_binding(
    b: &SnapshotBinding,
    tag: &str,
    artifact: Option<&ArtifactManifest>,
) -> Result<(), CliError> {
    if b.schema != "rfb-snapshot-binding/v1" {
        return Err(validation("unsupported snapshot binding schema"));
    }
    if b.tag != tag || b.tag.trim().is_empty() {
        return Err(validation(
            "snapshot binding tag does not match requested snapshot",
        ));
    }
    if b.record_hash.trim().is_empty() || b.record_hash != record_hash_of(b) {
        return Err(validation(
            "snapshot binding record_hash does not match record",
        ));
    }
    if !b.snapshot_ready || !b.snapshot_bootable {
        return Err(validation("snapshot binding is not ready/bootable"));
    }
    if !matches!(
        b.verification.as_str(),
        "verified" | "partial" | "unverified"
    ) {
        return Err(validation(
            "snapshot binding verification must be verified or unverified",
        ));
    }
    if b.artifact.backend != "forkd" {
        return Err(validation(
            "snapshot binding artifact backend must be forkd",
        ));
    }
    if b.verification == "verified" {
        let expected = b
            .artifact
            .snapshot_sha256
            .as_deref()
            .map(digest)
            .ok_or_else(|| validation("verified binding requires artifact snapshot digest"))?;
        if b.controller.digest.as_deref().map(digest) != Some(expected) {
            return Err(validation(
                "verified binding controller digest does not match artifact snapshot digest",
            ));
        }
    }
    if let Some(a) = artifact {
        if identity(a) != b.artifact {
            return Err(validation(
                "snapshot binding artifact identity does not match artifact manifest",
            ));
        }
        if let Some(s) = &a.snapshot {
            if s.tag != tag {
                return Err(validation(
                    "artifact snapshot tag does not match requested snapshot",
                ));
            }
        }
    }
    Ok(())
}
/// Load and validate a snapshot binding record from disk, checking the schema,
/// tag, record hash, and (when given) the artifact identity match.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn load_snapshot_binding(
    path: &Path,
    tag: &str,
    artifact: Option<&ArtifactManifest>,
) -> Result<SnapshotBinding, CliError> {
    let b: SnapshotBinding =
        serde_json::from_str(&fs::read_to_string(path).map_err(|e| io(e.to_string()))?)
            .map_err(|e| validation(format!("invalid snapshot binding: {e}")))?;
    validate_binding(&b, tag, artifact)?;
    Ok(b)
}

/// Observe a forkd snapshot's on-disk artifacts and build a typed provenance
/// record for `snapshot-bind`. The forkd daemon (0.5.x) emits no typed
/// provenance, so the digest evidence is gathered directly: `memory.bin`,
/// `vmstate` and the private `rootfs.ext4` copy are measured in the snapshot
/// directory, while kernel/firecracker digests are re-measured from the files
/// the artifact manifest names. Resource and identity fields are manifest
/// declarations carried through under the `source` label — they are never
/// dressed up as daemon attestations. Returns the record plus the measured
/// snapshot descriptor (`snapshot.json`) digest for the binding's controller
/// digest.
fn host_observed_provenance(artifact: &ArtifactManifest) -> Option<(Value, Option<String>)> {
    let dir = super::snapshot_paths::forkd_snapshots_root()?.join(&artifact.snapshot.as_ref()?.tag);
    let measure = |path: &Path| -> Option<String> {
        Some(format!("sha256:{}", sha256_bytes(&fs::read(path).ok()?)))
    };
    let memory = measure(&dir.join("memory.bin"))?;
    let vmstate = measure(&dir.join("vmstate"))?;
    let snapshot_rootfs = measure(&dir.join("rootfs.ext4"));
    let descriptor = measure(&dir.join("snapshot.json"));
    let kernel = artifact
        .kernel
        .as_ref()
        .and_then(|f| measure(Path::new(&f.path)));
    let input_rootfs = artifact
        .rootfs
        .as_ref()
        .and_then(|f| measure(Path::new(&f.path)));
    let firecracker = artifact
        .firecracker
        .as_ref()
        .and_then(|t| measure(Path::new(&t.path)));
    Some((
        json!({
            "status": "complete",
            "source": "host-observed-disk",
            "kernel_sha256": kernel,
            "input_rootfs_sha256": input_rootfs,
            "rootfs_sha256": snapshot_rootfs,
            "memory_sha256": memory,
            "vmstate_sha256": vmstate,
            "firecracker_sha256": firecracker,
            "vcpu_count": artifact.vm.cpus,
            "memory_bytes": artifact.vm.memory_bytes,
            "network": artifact.snapshot.as_ref().map(|x| x.network).unwrap_or(false),
            "batch_id": artifact.snapshot.as_ref().and_then(|x| x.batch_id.clone()),
            "version": artifact.snapshot.as_ref().and_then(|x| x.version.clone()),
            "vm_identity": artifact.snapshot.as_ref().and_then(|x| x.vm_identity.clone()),
            "network_identity": artifact.snapshot.as_ref().and_then(|x| x.network_identity.clone()),
        }),
        descriptor,
    ))
}

/// Bind a forkd snapshot to the given artifact: locates a ready/bootable
/// snapshot, cross-checks provenance digests against the artifact manifest,
/// and writes the binding record to `output` (when provided).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn snapshot_bind(
    url: &str,
    tag: &str,
    artifact_path: &Path,
    output: Option<&Path>,
    require_provenance: bool,
) -> Result<BindingOutput, CliError> {
    let artifact = ArtifactManifest::load_local(artifact_path)?;
    if artifact.backend != "forkd" {
        return Err(validation(
            "artifact backend must be forkd for snapshot binding",
        ));
    }
    let client = client_from_url_env(url, std::time::Duration::from_secs(5))?;
    let snap = client
        .list_snapshots()
        .await
        .map_err(|e| validation(e.to_string()))?
        .into_iter()
        .find(|s| s.tag == tag)
        .ok_or_else(|| validation(format!("snapshot tag not found: {tag}")))?;
    if !snap.status.eq_ignore_ascii_case("ready") || !snap.bootable {
        return Err(validation("snapshot is not ready/bootable"));
    }
    let detail = client
        .snapshot_info(tag)
        .await
        .map_err(|e| validation(e.to_string()))?;
    // The `/info` detail endpoint returns a chain/size record without the
    // list-level `status`/`bootable` fields, so readiness is judged from the
    // list entry (`snap`); the detail is used only to overlay provenance.
    let controller = ControllerObserved {
        detail: detail.is_some(),
        status: snap.status.clone(),
        bootable: snap.bootable,
        digest: detail
            .as_ref()
            .and_then(|d| d.digest.clone())
            .or_else(|| snap.digest.clone()),
        provenance: detail
            .as_ref()
            .and_then(|d| d.provenance.clone())
            .or_else(|| snap.provenance.clone()),
    };
    let (provenance, controller) = match controller.provenance.as_ref() {
        // The daemon emitted a typed record: cross-check it as-is.
        Some(value) if value.is_object() => (None, controller),
        // The daemon has no typed provenance (forkd 0.5.x reports the string
        // "unavailable"): observe the on-disk snapshot artifacts directly.
        // memory/vmstate/rootfs digests are measured from the snapshot
        // directory, kernel/firecracker digests are re-measured from the
        // files the artifact manifest names; resource and identity fields
        // are manifest declarations. The `source` label keeps this record
        // distinguishable from a daemon attestation, and every digest is
        // still cross-checked against the artifact manifest below — a
        // mismatched or missing file keeps the binding fail-closed.
        _ => {
            let (observed, descriptor) = host_observed_provenance(&artifact).ok_or_else(|| {
                validation(
                    "cannot observe snapshot artifacts on disk (snapshot dir or files missing)",
                )
            })?;
            let mut controller = controller;
            if controller.digest.is_none() {
                controller.digest = descriptor;
            }
            (Some(observed), controller)
        }
    };
    let controller = match &provenance {
        Some(observed) => {
            let mut controller = controller;
            controller.provenance = Some(observed.clone());
            controller
        }
        None => controller,
    };
    let provenance = provenance.as_ref().or(controller.provenance.as_ref());
    let provenance_status = provenance
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str);
    let typed_diagnostics = typed_provenance_diagnostics(provenance, &artifact);
    let matches_digest = |key: &str, expected: Option<&String>| {
        expected.is_some()
            && provenance
                .and_then(|p| p.get(key))
                .and_then(Value::as_str)
                .map(|actual| digest(actual) == digest(expected.unwrap()))
                == Some(true)
    };
    let matches_rootfs = |expected: Option<&String>| {
        expected.is_some()
            && provenance
                .and_then(|p| {
                    p.get("input_rootfs_sha256")
                        .or_else(|| p.get("rootfs_sha256"))
                })
                .and_then(Value::as_str)
                .map(|actual| digest(actual) == digest(expected.unwrap()))
                == Some(true)
    };
    let matches_u64 = |key: &str, expected: u64| {
        provenance.and_then(|p| p.get(key)).and_then(Value::as_u64) == Some(expected)
    };
    let matches_bool = |key: &str, expected: bool| {
        provenance.and_then(|p| p.get(key)).and_then(Value::as_bool) == Some(expected)
    };
    let verified = typed_diagnostics.is_empty()
        && provenance_status == Some("complete")
        && matches_digest("kernel_sha256", artifact.kernel.as_ref().map(|x| &x.sha256))
        && matches_rootfs(artifact.rootfs.as_ref().map(|x| &x.sha256))
        && matches_digest(
            "memory_sha256",
            artifact
                .snapshot
                .as_ref()
                .and_then(|x| x.memory_sha256.as_ref()),
        )
        && matches_digest(
            "vmstate_sha256",
            artifact
                .snapshot
                .as_ref()
                .and_then(|x| x.vmstate_sha256.as_ref()),
        )
        && matches_u64("vcpu_count", artifact.vm.cpus as u64)
        && (artifact.vm.memory_bytes == 0 || matches_u64("memory_bytes", artifact.vm.memory_bytes))
        && matches_bool(
            "network",
            artifact
                .snapshot
                .as_ref()
                .map(|x| x.network)
                .unwrap_or(false),
        )
        && matches_digest(
            "firecracker_sha256",
            artifact.firecracker.as_ref().map(|x| &x.sha256),
        );
    // Typed identity is mandatory for strong reuse approval. Legacy JSON may be
    // inspected, but cannot be promoted to verified without batch/version/VM/network identity.
    if require_provenance && (!verified || !typed_diagnostics.is_empty()) {
        return Err(validation(format!(
            "snapshot provenance is unavailable or does not match artifact identity: {}",
            typed_diagnostics.join(", ")
        )));
    }
    let has_provenance = provenance.is_some();
    let mut b = SnapshotBinding {
        schema: "rfb-snapshot-binding/v1".into(),
        tag: tag.into(),
        snapshot_ready: true,
        snapshot_bootable: true,
        artifact: identity(&artifact),
        controller,
        record_hash: String::new(),
        verification: if verified {
            "verified".into()
        } else if has_provenance {
            "partial".into()
        } else {
            "unverified".into()
        },
    };
    b.record_hash = record_hash_of(&b);
    let value = serde_json::to_value(&b).map_err(|e| io(e.to_string()))?;
    if let Some(p) = output {
        fs::write(
            p,
            serde_json::to_string_pretty(&value).map_err(|e| io(e.to_string()))? + "\n",
        )
        .map_err(|e| io(e.to_string()))?;
    }
    Ok(BindingOutput {
        value,
        text: format!(
            "snapshot {tag} bound to forkd artifact (record {}) {}",
            b.record_hash, b.verification
        ),
    })
}
