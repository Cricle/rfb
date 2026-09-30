//! Image building: ext4 creation from a staging manifest, rootfs images from a
//! runtime binary, and profile diagnostics. Delegates all ext4 manipulation to
//! e2fsprogs (`mke2fs`/`debugfs`) and verifies every injected file by digest.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::image_build::manifest::{
    safe_join, sha256, sha256_bytes, validate, StagingManifest,
};
use crate::cli::image_build::verify::{binary_contains, is_dynamically_linked, run_debugfs};
use crate::cli::tool::tool_available;
use crate::image_profiles::ImageManifestProfile;
use serde_json::{json, Value};
use std::{fs, io::Write, path::Path, process::Command};
use tempfile::NamedTempFile;

/// Whether `path` is an existing, openable regular file.
pub(crate) fn readable_file(path: &Path) -> bool {
    path.is_file() && fs::File::open(path).is_ok()
}

/// Require `path` to be a readable file; `label` names it in the error.
pub(crate) fn require_readable(path: &Path, label: &str) -> Result<(), CliError> {
    if readable_file(path) {
        Ok(())
    } else {
        Err(validation(format!("{label} missing: {}", path.display())))
    }
}

/// Resolve the rootfs source shared by the backend `up` commands: `--rootfs`
/// provided as-is, or built from a pid1 dir via `build_from_pid1`. The
/// mutual-exclusion/required validation is KVM- and permission-free by design
/// so non-root environments (CI) exercise the same contract.
pub(crate) fn resolve_rootfs_source(
    rootfs: Option<&Path>,
    pid1_dir: Option<&Path>,
    build_from_pid1: impl FnOnce(&Path) -> Result<std::path::PathBuf, CliError>,
) -> Result<(std::path::PathBuf, String), CliError> {
    match (rootfs, pid1_dir) {
        (Some(rootfs), None) => Ok((rootfs.to_path_buf(), "provided".to_owned())),
        (None, Some(dir)) => Ok((build_from_pid1(dir)?, "built from pid1".to_owned())),
        (Some(_), Some(_)) => Err(validation("--rootfs and --pid1-dir are mutually exclusive")),
        (None, None) => Err(validation("one of --rootfs or --pid1-dir is required")),
    }
}

/// Initialize a new staging directory + manifest.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn init(directory: &Path, size: u64, force: bool) -> Result<Value, CliError> {
    if size == 0 || !size.is_multiple_of(4096) {
        return Err(validation("--size must be a non-zero multiple of 4096"));
    }
    if directory.exists() && !force {
        return Err(io("directory exists (use --force)"));
    }
    let staging = directory.join("staging");
    fs::create_dir_all(&staging).map_err(|error| io(error.to_string()))?;
    let manifest = StagingManifest {
        format: "rfb-cli-staging/v1".to_owned(),
        image_size_bytes: size,
        block_size: 4096,
        staging: "staging".to_owned(),
        files: Vec::new(),
        image: None,
    };
    let manifest_path = directory.join("manifest.json");
    let contents =
        serde_json::to_string_pretty(&manifest).map_err(|error| io(error.to_string()))?;
    fs::write(&manifest_path, format!("{contents}\n")).map_err(|error| io(error.to_string()))?;
    Ok(json!({
        "ok": true,
        "manifest": manifest_path,
        "staging": staging,
    }))
}

/// Dry-run or real ext4 build from a validated manifest.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn build(
    manifest_path: &Path,
    output: Option<&Path>,
    execute: bool,
    force: bool,
) -> Result<Value, CliError> {
    let manifest = validate(manifest_path)?;
    let output_path = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest_path.with_extension("img"));
    // The output path is passed to mke2fs/debugfs as an argv string; a
    // non-UTF-8 path must fail validation instead of silently retargeting
    // the build to a default name.
    if output_path.to_str().is_none() {
        return Err(validation(format!(
            "output image path must be valid UTF-8: {}",
            output_path.display()
        )));
    }
    if !execute {
        let mke2fs = tool_available("mke2fs");
        let debugfs = tool_available("debugfs");
        let tools_ready = mke2fs && debugfs;
        return Ok(json!({
            "ok": true,
            "dry_run": true,
            "status": if tools_ready { "READY" } else { "SKIP" },
            "output": output_path,
            "image_size_bytes": manifest.image_size_bytes,
            "delegation": ["mke2fs", "debugfs"],
            "tools": {"mke2fs": mke2fs, "debugfs": debugfs},
        }));
    }
    let image = manifest
        .image
        .as_ref()
        .ok_or_else(|| validation("--execute requires an image manifest with an entrypoint"))?;
    let entrypoint = image
        .entrypoint
        .first()
        .ok_or_else(|| validation("--execute requires a non-empty image entrypoint"))?;
    let entrypoint_path = entrypoint.trim_start_matches('/');
    if entrypoint_path.is_empty()
        || !manifest
            .files
            .iter()
            .any(|file| file.path == entrypoint_path)
    {
        return Err(validation(format!(
            "entrypoint is not present in staging files: {entrypoint}"
        )));
    }
    if !tool_available("mke2fs") || !tool_available("debugfs") {
        return Err(external("mke2fs and debugfs are required for --execute"));
    }
    // Overwrite gate, same contract as build_rootfs: a previous image is only
    // replaced with --force.
    if output_path.exists() && !force {
        return Err(validation(format!(
            "refusing to overwrite existing output (use --force): {}",
            output_path.display()
        )));
    }

    // Build beside the destination and publish only after all verification
    // succeeds. Keeping the temporary file in the same directory makes the
    // final rename atomic; a mid-build failure (mke2fs, a debugfs injection,
    // the digest verify below) then leaves any prior output intact instead
    // of a corrupt half-written image where a good one was.
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|error| io(error.to_string()))?;
    }
    let temp_image = NamedTempFile::new_in(output_path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| io(error.to_string()))?;
    let image_path = temp_image.path().to_path_buf();
    let image_arg = image_path.to_str().ok_or_else(|| {
        external(format!(
            "temporary image path is not valid UTF-8: {}",
            image_path.display()
        ))
    })?;
    let status = Command::new("mke2fs")
        .args([
            "-t",
            "ext4",
            "-F",
            image_arg,
            &format!("{}", manifest.image_size_bytes / 4096),
        ])
        .status()
        .map_err(|error| external(error.to_string()))?;
    if !status.success() {
        return Err(external(format!("mke2fs failed with {status}")));
    }

    // Let e2fsprogs own all ext4 manipulation. Create directories first,
    // then inject each validated staging file with debugfs(8).
    let mut directories = std::collections::BTreeSet::new();
    for file in &manifest.files {
        let mut parent = Path::new(&file.path).parent();
        while let Some(path) = parent {
            if !path.as_os_str().is_empty() {
                directories.insert(path.to_string_lossy().replace('\\', "/"));
            }
            parent = path.parent();
        }
    }
    for directory in directories {
        // mke2fs already creates default top-level directories (`/bin`, `/sbin`,
        // `/etc`, ...). Skip a directory that already exists instead of failing.
        if run_debugfs(
            &image_path,
            &format!("stat {}", debugfs_quote(&directory)),
            false,
        )
        .is_err()
        {
            run_debugfs(
                &image_path,
                &format!("mkdir {}", debugfs_quote(&directory)),
                false,
            )?;
        }
    }
    for file in &manifest.files {
        let source = safe_join(
            manifest_path.parent().unwrap_or_else(|| Path::new(".")),
            &format!("{}/{}", manifest.staging, file.path),
        )?;
        let destination = format!("/{}", file.path);
        run_debugfs(
            &image_path,
            &format!(
                "write {} {}",
                debugfs_quote(&source.to_string_lossy()),
                debugfs_quote(&destination)
            ),
            false,
        )?;
        let actual = run_debugfs(
            &image_path,
            &format!("cat {}", debugfs_quote(&destination)),
            true,
        )?;
        // manifest::validate accepts non-lowercase hex, so compare
        // case-insensitively here too (artifact.rs does the same).
        if actual.len() as u64 != file.size
            || !sha256_bytes(&actual).eq_ignore_ascii_case(&file.sha256)
        {
            return Err(external(format!(
                "debugfs verification failed for {}",
                file.path
            )));
        }
    }

    let digest = sha256(&image_path).map_err(|error| io(error.to_string()))?;
    if let Some(expected) = image.digest.as_deref() {
        let expected = expected.strip_prefix("sha256:").unwrap_or(expected);
        if !digest.eq_ignore_ascii_case(expected) {
            return Err(external(format!(
                "image digest mismatch: expected {expected}, got sha256:{digest}"
            )));
        }
    }
    // Publish the fully verified image atomically, leaving any prior output
    // intact if construction or verification above fails.
    temp_image
        .persist(&output_path)
        .map_err(|error| io(error.error.to_string()))?;
    let artifact =
        crate::cli::image_build::ArtifactManifest::for_image(&output_path, &digest, image)?;
    artifact.validate_schema()?;
    let artifact_manifest_path = artifact.write_sidecar(&output_path)?;
    Ok(json!({
        "ok": true,
        "output": output_path,
        "digest": format!("sha256:{digest}"),
        "entrypoint": image.entrypoint,
        "artifact_manifest_file": artifact_manifest_path,
    }))
}

/// Profile diagnostics extracted from a staging manifest for `image inspect`.
pub fn image_diagnostics(manifest: &StagingManifest) -> (String, String, Vec<String>, String) {
    let Some(image) = manifest.image.as_ref() else {
        return (
            "unknown".to_owned(),
            "unknown".to_owned(),
            Vec::new(),
            "none".to_owned(),
        );
    };
    let profile = image.profile.clone().unwrap_or_else(|| "custom".to_owned());
    let capabilities = if image.capabilities.is_empty() {
        vec!["execute".to_owned()]
    } else {
        image.capabilities.clone()
    };
    (
        image.transport.clone(),
        image.protocol.clone(),
        capabilities,
        profile,
    )
}

/// All known named profiles as a JSON array for `doctor`.
pub fn profile_summary() -> Value {
    let profiles: Vec<Value> = ImageManifestProfile::all()
        .iter()
        .map(|profile| {
            json!({
                "profile": profile.name(),
                "transport": profile.transport(),
                "protocol": profile.protocol(),
                "guest_port": profile.guest_port(),
                "entrypoint": profile.entrypoint(),
                "capabilities": profile.capabilities(),
                "status": "supported"
            })
        })
        .collect();
    json!(profiles)
}
/// Optional interpreter wiring for `image build-rootfs`.
#[derive(Debug, Default, Clone)]
pub struct RootfsOptions {
    /// Install `/bin/python3` as a hardlink to the runtime binary (the
    /// binary must be built with the `rustpython` cargo feature).
    pub with_python: bool,
    /// Install `/bin/lua` as a hardlink to the runtime binary (the binary
    /// must be built with the `mlua` cargo feature).
    pub with_lua: bool,
    /// Local tree of pure-Python packages baked into the image at
    /// `/usr/lib/python3/site-packages` (the offline `sys.path` root).
    pub py_site_dir: Option<std::path::PathBuf>,
    /// Local tree of Lua modules baked into the image at `/usr/lib/lua/5.4`
    /// (the offline `package.path` root).
    pub lua_lib_dir: Option<std::path::PathBuf>,
    /// Extra files baked into the image verbatim (build-script payloads:
    /// configs, assets, prebuilt app binaries). Installed and digest-verified
    /// like every other payload.
    pub extra_files: Vec<ExtraFile>,
}

/// One extra file a build script bakes into the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraFile {
    /// Host source path.
    pub source: std::path::PathBuf,
    /// Absolute guest destination path (must start with `/`).
    pub guest: String,
    /// POSIX mode for the guest inode (e.g. `0o755`).
    pub mode: u32,
}

/// Python package root baked into the image; the embedded interpreter appends
/// it to `sys.path`.
pub const PY_SITE_PACKAGES: &str = "/usr/lib/python3/site-packages";
/// Lua module root baked into the image; the embedded interpreter installs it
/// into `package.path`.
pub const LUA_LIB_DIR: &str = "/usr/lib/lua/5.4";

/// Build a rootfs image directly from a runtime binary + mode (the `env
/// setup`/`image build-rootfs` path). Creates the ext4 with a fixed 0755
/// entrypoint, protocol marker, and (for rfb-vsock) the executor environment.
/// `force` must be set to overwrite an existing output.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn build_rootfs(
    runtime_bin: &Path,
    output: &Path,
    size_mb: Option<u64>,
    mode: &str,
    allow_dynamic: bool,
    force: bool,
    options: &RootfsOptions,
) -> Result<Value, CliError> {
    let (install_path, entrypoint, protocol): (&str, &str, &str) = match mode {
        "rfb-vsock" => ("/sbin/rfb-runtime", "/sbin/rfb-runtime", "rfb1"),
        "forkd-agent" => ("/sbin/forkd-agent", "/forkd-init.sh", "forkd"),
        "zeroboot-zbrt" => ("/init", "/init", "zbrt"),
        _ => {
            return Err(validation(
                "image mode must be rfb-vsock, forkd-agent, or zeroboot-zbrt",
            ))
        }
    };
    if !runtime_bin.is_file() {
        return Err(validation("runtime binary is not a readable regular file"));
    }
    if output.exists() && !force {
        return Err(validation(format!(
            "refusing to overwrite existing output (use --force): {}",
            output.display()
        )));
    }
    for command in ["debugfs", "mke2fs", "sha256sum", "stat", "truncate"] {
        if !tool_available(command) {
            return Err(validation(format!("{command} is required")));
        }
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| io(error.to_string()))?;
    }
    if !allow_dynamic && is_dynamically_linked(runtime_bin)? {
        return Err(validation(
            "refusing dynamic runtime; build x86_64-unknown-linux-musl first (or set --allow-dynamic)",
        ));
    }
    // Interpreter hardlinks dispatch by argv[0] at guest runtime, which is
    // mode-independent (the multi-call binary serves python3/lua under every
    // mode), so every mode may install them. The hardlink SOURCE is the
    // mode's own entrypoint (see install below).
    // The hardlink install cannot make an interpreter work: if the runtime was
    // built without the feature, /bin/python3 would dispatch to "unknown
    // runtime mode" at guest runtime. Verify the markers before installing.
    if (options.with_python || options.py_site_dir.is_some())
        && !binary_contains(runtime_bin, b"RustPython")?
    {
        return Err(validation(
            "--with-python/--py-site-dir require the runtime built with the `rustpython` cargo feature (image build-static --features cli,rustpython)",
        ));
    }
    if (options.with_lua || options.lua_lib_dir.is_some())
        && !binary_contains(runtime_bin, b"Lua 5.4")?
    {
        return Err(validation(
            "--with-lua/--lua-lib-dir require the runtime built with the `mlua` cargo feature (image build-static --features cli,mlua)",
        ));
    }

    // ZBRT images are tiny disposable per-sandbox copies (workspace writes
    // land on the tmpfs), so: no journal, no reserved blocks, small floor.
    // forkd images keep a journal (the rootfs is shared across children) but
    // also drop the root-reserved percentage; their floor is 8 MiB because
    // the rootfs is effectively read-only too.
    let zbrt = mode == "zeroboot-zbrt";
    let floor_mb = if zbrt { 2 } else { 8 };
    // Headroom covers ext4 metadata (inode tables, bitmaps, GDT): zbrt
    // images carry no journal, so 3 MiB suffices.
    let headroom_bytes: u64 = if zbrt {
        3 * 1024 * 1024
    } else {
        8 * 1024 * 1024
    };
    let runtime_len = fs::metadata(runtime_bin)
        .map_err(|error| io(error.to_string()))?
        .len();
    // The runtime installs ONCE in every mode: the forkd entrypoint is a
    // hardlink to the installed binary (same inode).
    let mut content_bytes = runtime_len;
    // /bin/sh (rfb-busybox) installs in every image mode.
    if let Some(parent) = runtime_bin.parent() {
        content_bytes += fs::metadata(parent.join("rfb-busybox"))
            .map(|meta| meta.len())
            .unwrap_or(0);
    }
    if zbrt {
        // The multi-call applets are installed alongside the runtime binary.
        if let Some(parent) = runtime_bin.parent() {
            content_bytes += fs::metadata(parent.join("rfb-mini-tools"))
                .map(|meta| meta.len())
                .unwrap_or(0);
        }
    }
    // Site-package trees and build-script payload files are real content in
    // every mode (interpreters install in every mode; extra files too).
    for dir in [
        options.py_site_dir.as_deref(),
        options.lua_lib_dir.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        content_bytes += site_tree_bytes(dir)?;
    }
    for extra in &options.extra_files {
        content_bytes += fs::metadata(&extra.source)
            .map_err(|error| io(error.to_string()))?
            .len();
    }
    let size_mb = match size_mb {
        Some(mb) => mb.max(floor_mb),
        None => (content_bytes + headroom_bytes)
            .div_ceil(1024 * 1024)
            .max(floor_mb),
    };
    // Build beside the destination and publish only after all verification succeeds.
    // Keeping the temporary file in the same directory makes the final rename atomic.
    let temp_image = NamedTempFile::new_in(output.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| io(error.to_string()))?;
    let image_path = temp_image.path().to_path_buf();
    let image = image_path.to_str().unwrap_or("image.ext4").to_owned();
    let status = Command::new("truncate")
        .args(["-s", &format!("{size_mb}M"), &image])
        .status()
        .map_err(|error| external(error.to_string()))?;
    if !status.success() {
        return Err(external("truncate failed"));
    }
    // mke2fs sizing: -m 0 drops the 5% root-reserved blocks (the guest runs
    // as root and the workspace tmpfs is the write target anyway). ZBRT
    // images additionally drop the journal: each image is a disposable
    // per-sandbox copy whose writes land on the tmpfs, so crash-consistency
    // of the rootfs does not matter (~4 MiB saved per image).
    let mkfs_args: &[&str] = if zbrt {
        &[
            "-q",
            "-t",
            "ext4",
            "-F",
            "-O",
            "^has_journal",
            "-m",
            "0",
            &image,
        ]
    } else {
        &["-q", "-t", "ext4", "-F", "-m", "0", &image]
    };
    let status = Command::new("mke2fs")
        .args(mkfs_args)
        .status()
        .map_err(|error| external(error.to_string()))?;
    if !status.success() {
        return Err(external("mke2fs failed"));
    }
    for d in [
        "bin",
        "sbin",
        "etc",
        "etc/rfb-runtime",
        "etc/zeroboot",
        "proc",
        "sys",
        "dev",
        "run",
        "tmp",
        "workspace",
    ] {
        if run_debugfs(&image_path, &format!("stat /{d}"), false).is_err() {
            run_debugfs(&image_path, &format!("mkdir /{d}"), false)?;
        }
    }
    // rfb-busybox becomes /bin/sh plus hardlinked applets, so guest shells
    // and shell-string commands (eval, exec) work in every image mode.
    let busybox_src = runtime_bin
        .parent()
        .map(|parent| parent.join("rfb-busybox"));
    if let Some(busybox_src) = busybox_src.filter(|path| path.is_file()) {
        // Clear any pre-created /bin/sh first — debugfs write fails closed on
        // an existing inode (same pattern as the applet writes below).
        force_write(&image_path, &busybox_src, "/bin/sh")?;
        // Hardlinked multi-call entry points (same inode, zero extra image
        // bytes): `bash` is the same shell under its alternative name and
        // `sleep` the one coreutil the timeout/eval contracts exercise.
        // Unimplemented applet names stay absent rather than
        // present-but-broken.
        for applet in ["bash", "sleep"] {
            run_debugfs(&image_path, &format!("ln /bin/sh /bin/{applet}"), false)?;
        }
    }
    force_write(&image_path, runtime_bin, install_path)?;
    if mode == "forkd-agent" {
        // /forkd-init.sh is a HARDLINK to the installed runtime binary (same
        // inode, zero image bytes): the multi-call dispatch keys on the
        // argv[0] basename, which a hardlink preserves. Writing the binary a
        // second time doubled every forkd image by the runtime size.
        install_hardlink(&image_path, install_path, entrypoint)?;
    }
    if mode == "zeroboot-zbrt" {
        write_marker(&image_path, "/etc/zeroboot-protocol", "zbrt\n")?;
        write_marker(&image_path, "/etc/zeroboot-protocol-version", "1\n")?;
        // Single source of truth: same vocabulary the guest HelloAck and
        // `rfb-cli zeroboot verify` use (crate::protocol re-exports the
        // canonical const from rfb-runtime).
        write_marker(
            &image_path,
            "/etc/zeroboot-capabilities",
            &format!("{}\n", crate::protocol::ZBRT_V1_CAPABILITIES.join(",")),
        )?;
        write_marker(&image_path, "/etc/zeroboot-guest-port", "5000\n")?;
    }
    if mode == "rfb-vsock" {
        // Write protocol-version and environment markers via debugfs `write`
        // from temp files (debugfs cannot inline stdin).
        write_marker(&image_path, "/etc/rfb-runtime/protocol-version", "1\n")?;
        write_marker(
            &image_path,
            "/etc/rfb-runtime/environment",
            "RFB_RUNTIME_EXECUTOR=workspace\nRFB_RUNTIME_WORKSPACE=/workspace\n",
        )?;
    }

    // Verify every published contract artifact before reporting success.
    let entry_stat = run_debugfs(&image_path, &format!("stat {entrypoint}"), true)?;
    if !crate::cli::image_build::stat_is_executable_regular(&String::from_utf8_lossy(&entry_stat)) {
        return Err(external(format!(
            "entrypoint verification failed: {entrypoint}; debugfs stat: {}",
            String::from_utf8_lossy(&entry_stat)
                .trim()
                .replace('\n', " | ")
        )));
    }
    crate::cli::image_build::verify_installed_file(&image_path, runtime_bin, install_path)?;
    if mode == "rfb-vsock" {
        let marker = run_debugfs(&image_path, "cat /etc/rfb-runtime/protocol-version", true)?;
        if String::from_utf8_lossy(&marker).trim() != "1" {
            return Err(external("protocol marker verification failed"));
        }
        let environment = run_debugfs(&image_path, "cat /etc/rfb-runtime/environment", true)?;
        let environment = String::from_utf8_lossy(&environment);
        if !environment.contains("RFB_RUNTIME_EXECUTOR=workspace")
            || !environment.contains("RFB_RUNTIME_WORKSPACE=/workspace")
        {
            return Err(external("environment verification failed"));
        }
    }
    if mode == "zeroboot-zbrt" {
        let [protocol, version, capabilities, guest_port] = read_zbrt_markers(&image_path);
        let marker = protocol?;
        let version = version?;
        let capabilities = capabilities?;
        let guest_port = guest_port?;
        if marker != "zbrt" || version != "1" {
            return Err(external(
                "ZeroBoot zbrt protocol/version marker verification failed",
            ));
        }
        if capabilities != crate::protocol::ZBRT_V1_CAPABILITIES.join(",") {
            return Err(external("ZeroBoot capability marker verification failed"));
        }
        if guest_port != "5000" {
            return Err(external(format!(
                "ZeroBoot guest port verification failed: {guest_port:?}"
            )));
        }
        // A ZeroBoot rootfs must not ship the legacy rfb-runtime/forkd agent or
        // the forkd marker.
        for forbidden in [
            "/sbin/forkd-agent",
            "/sbin/rfb-runtime",
            "/etc/zeroboot-forkd-init",
            "/forkd-init.sh",
            "/etc/zeroboot-forkd",
        ] {
            let stat =
                run_debugfs(&image_path, &format!("stat {forbidden}"), true).unwrap_or_default();
            let stat = String::from_utf8_lossy(&stat);
            if stat.contains("Inode:") && stat.contains("Type:") {
                return Err(external(format!(
                    "ZeroBoot zbrt rootfs must not contain {forbidden}"
                )));
            }
        }
        // The ZBRT execute contract runs real applets (echo/true/false) via
        // the workspace executor, so the image needs a minimal userland.
        // `cargo build -p rfb-runtime` produces the multi-call
        // `rfb-mini-tools` next to the runtime binary; install it as the
        // applets the contract exercises.
        let mini_tools = runtime_bin
            .parent()
            .map(|parent| parent.join("rfb-mini-tools"))
            .ok_or_else(|| external("runtime binary has no parent directory"))?;
        if !mini_tools.is_file() {
            return Err(external(format!(
                "rfb-mini-tools is missing next to the runtime binary ({}); build it with the same cargo invocation",
                mini_tools.display()
            )));
        }
        if run_debugfs(&image_path, "stat /bin", false).is_err() {
            run_debugfs(&image_path, "mkdir /bin", false)?;
        }
        // The busybox /bin/sh install happens once in the mode-common path
        // above; it is NOT repeated here — a second write hits the existing
        // inode and debugfs fails the whole build.
        for applet in ["echo", "true", "false", "netprobe"] {
            let target = format!("/bin/{applet}");
            force_write(&image_path, &mini_tools, &target)?;
        }
        // `nproc` is that same multi-call binary under another name, so link
        // it instead of paying another copy: capacity runs use it to report
        // the CPU count the guest actually brought up.
        let nproc = "/bin/nproc";
        clear_inode(&image_path, nproc);
        run_debugfs(&image_path, &format!("ln /bin/echo {nproc}"), false)?;
    }
    // Interpreter multi-call hardlinks: /bin/python3 and /bin/lua point
    // at the mode's entrypoint, which dispatches on argv[0]. Zero extra
    // image bytes; the interpreter itself must be compiled into the
    // runtime binary via the `rustpython`/`mlua` cargo features.
    if options.with_python {
        install_hardlink(&image_path, entrypoint, "/bin/python3")?;
    }
    if options.with_lua {
        install_hardlink(&image_path, entrypoint, "/bin/lua")?;
    }
    // Offline extension packages: static files under the interpreter import
    // roots, written and digest-verified like every other payload. Every mode
    // installs them (the interpreters that consume the roots do too).
    install_site_dir(
        &image_path,
        options.py_site_dir.as_deref(),
        PY_SITE_PACKAGES,
    )?;
    install_site_dir(&image_path, options.lua_lib_dir.as_deref(), LUA_LIB_DIR)?;
    // Build-script payload files (configs, assets, prebuilt app binaries):
    // same install + digest-verify contract as the site trees.
    install_extra_files(&image_path, &options.extra_files)?;
    let logical_bytes = fs::metadata(&image_path)
        .map_err(|error| io(error.to_string()))?
        .len();
    let digest = sha256(&image_path).map_err(|error| io(error.to_string()))?;

    // Publish the fully verified image atomically, leaving any prior output intact
    // if construction or verification above fails.
    temp_image
        .persist(output)
        .map_err(|error| io(error.error.to_string()))?;

    let artifact = crate::cli::image_build::ArtifactManifest::for_rootfs(
        output, &digest, mode, entrypoint, protocol,
    )?;
    artifact.validate()?;
    let artifact_manifest_path = artifact.write_sidecar(output)?;

    // Publish companion artifacts next to the image: a portable relative-path
    // checksum file and a compact manifest, mirroring build-rootfs.sh.
    let checksum_path = output.with_extension("ext4.sha256");
    let base = output.parent().unwrap_or_else(|| Path::new("."));
    let rel = |path: &Path| -> String {
        if path.is_absolute() && base.is_absolute() {
            path.strip_prefix(base)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        } else {
            path.to_string_lossy().replace('\\', "/")
        }
    };
    let checksum = format!("{}  {}\n", digest, rel(output));
    atomic_write(&checksum_path, checksum.as_bytes())?;
    let mut interpreters: Vec<&str> = Vec::new();
    if options.with_python {
        interpreters.push("python");
    }
    if options.with_lua {
        interpreters.push("lua");
    }
    let manifest = json!({
        "runtime": install_path,
        "entrypoint": entrypoint,
        "protocol": protocol,
        "size_mb": size_mb,
        "logical_bytes": logical_bytes,
        "linkage": if allow_dynamic { "dynamically-linked-or-static" } else { "static" },
        "image": output.file_name().and_then(|n| n.to_str()).unwrap_or("image.ext4"),
        "digest": format!("sha256:{digest}"),
        "interpreters": interpreters,
        "site_dirs": {
            "python": options.py_site_dir.as_ref().map(|p| p.to_string_lossy()),
            "lua": options.lua_lib_dir.as_ref().map(|p| p.to_string_lossy()),
        },
    });
    let manifest_path = output.with_extension("ext4.manifest.json");
    let manifest_contents =
        serde_json::to_string_pretty(&manifest).map_err(|error| io(error.to_string()))?;
    atomic_write(&manifest_path, manifest_contents.as_bytes())?;

    Ok(json!({
        "ok": true,
        "output": output,
        "size_mb": size_mb,
        "logical_bytes": logical_bytes,
        "protocol": protocol,
        "digest": format!("sha256:{digest}"),
        "entrypoint": entrypoint,
        "mode": mode,
        "checksum_file": checksum_path,
        "manifest_file": manifest_path,
        "artifact_manifest_file": artifact_manifest_path,
    }))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), CliError> {
    let parent = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = NamedTempFile::new_in(parent).map_err(|error| io(error.to_string()))?;
    temp.write_all(contents)
        .map_err(|error| io(error.to_string()))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| io(error.to_string()))?;
    temp.persist(path)
        .map_err(|error| io(error.error.to_string()))?;
    Ok(())
}

/// Hardlink `target_in_image` to `link_in_image` (same inode, zero extra
/// image bytes), clearing any pre-created target first.
fn install_hardlink(image_path: &Path, source: &str, link: &str) -> Result<(), CliError> {
    if run_debugfs(image_path, &format!("stat {link}"), false).is_ok() {
        let _ = run_debugfs(image_path, &format!("unlink {link}"), false);
        let _ = run_debugfs(image_path, &format!("rm {link}"), false);
    }
    run_debugfs(
        image_path,
        &format!("ln {} {}", debugfs_quote(source), debugfs_quote(link)),
        false,
    )?;
    let stat = run_debugfs(image_path, &format!("stat {link}"), true)?;
    let stat = String::from_utf8_lossy(&stat);
    if !stat.contains("Inode:") || !stat.contains("regular") {
        return Err(external(format!(
            "interpreter hardlink verification failed: {link}"
        )));
    }
    Ok(())
}

/// Remove a pre-created inode from the image if present: debugfs `write`
/// fails closed on an existing inode, so rewrite targets are cleared first.
fn clear_inode(image_path: &Path, target: &str) {
    if run_debugfs(image_path, &format!("stat {target}"), false).is_ok() {
        let _ = run_debugfs(image_path, &format!("unlink {target}"), false);
        let _ = run_debugfs(image_path, &format!("rm {target}"), false);
    }
}

/// Write a host file into the image at `target`, clearing any pre-created
/// inode first and forcing the regular-file 0755 mode.
fn force_write(image_path: &Path, source: &Path, target: &str) -> Result<(), CliError> {
    clear_inode(image_path, target);
    run_debugfs(
        image_path,
        &format!(
            "write {} {target}",
            debugfs_quote(&source.to_string_lossy())
        ),
        false,
    )?;
    run_debugfs(
        image_path,
        &format!("set_inode_field {target} mode 0100755"),
        false,
    )?;
    Ok(())
}

/// Write marker `content` to `guest_path` via a temp file + debugfs `write`
/// (debugfs cannot inline stdin).
fn write_marker(image_path: &Path, guest_path: &str, content: &str) -> Result<(), CliError> {
    let marker = NamedTempFile::new().map_err(|error| io(error.to_string()))?;
    fs::write(marker.path(), content).map_err(|error| io(error.to_string()))?;
    run_debugfs(
        image_path,
        &format!(
            "write {} {guest_path}",
            debugfs_quote(&marker.path().to_string_lossy())
        ),
        false,
    )?;
    Ok(())
}

/// Read the four ZeroBoot contract markers from an ext4 image via debugfs,
/// trimmed. Each read fails separately so every caller keeps its own missing
/// marker wording.
pub(crate) fn read_zbrt_markers(image_path: &Path) -> [Result<String, CliError>; 4] {
    let cat = |path: &str| {
        run_debugfs(image_path, &format!("cat {path}"), true)
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
    };
    [
        cat("/etc/zeroboot-protocol"),
        cat("/etc/zeroboot-protocol-version"),
        cat("/etc/zeroboot-capabilities"),
        cat("/etc/zeroboot-guest-port"),
    ]
}

/// Recursively collect regular files under `dir` as (relative path, source).
/// Symlinks are rejected: baked extension packages must be plain content.
fn collect_site_files(
    dir: &Path,
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<(), CliError> {
    let entries = fs::read_dir(dir).map_err(|error| io(format!("{}: {error}", dir.display())))?;
    let mut entries: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| (entry.file_name(), entry.path()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in entries {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| io(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() {
            return Err(validation(format!(
                "refusing symlinked extension package content: {}",
                path.display()
            )));
        }
        let rel = name.to_string_lossy().replace('\\', "/");
        if metadata.is_dir() {
            let mut child_files = Vec::new();
            collect_site_files(&path, &mut child_files)?;
            out.extend(
                child_files
                    .into_iter()
                    .map(|(child_rel, source)| (format!("{rel}/{child_rel}"), source)),
            );
        } else if metadata.is_file() {
            out.push((rel, path));
        }
    }
    Ok(())
}

/// Total byte size of a site tree, for the image size floor.
fn site_tree_bytes(dir: &Path) -> Result<u64, CliError> {
    let mut files = Vec::new();
    collect_site_files(dir, &mut files)?;
    Ok(files
        .iter()
        .filter_map(|(_, source)| fs::metadata(source).ok())
        .map(|meta| meta.len())
        .sum())
}

/// Write a local extension-package tree into `image_root/<rel>`, creating
/// parent directories and digest-verifying every file like other payloads.
fn install_site_dir(
    image_path: &Path,
    dir: Option<&Path>,
    image_root: &str,
) -> Result<(), CliError> {
    let Some(dir) = dir else {
        return Ok(());
    };
    if !dir.is_dir() {
        return Err(validation(format!(
            "site directory is not a readable directory: {}",
            dir.display()
        )));
    }
    let mut files = Vec::new();
    collect_site_files(dir, &mut files)?;
    // Create the root plus every parent directory first (debugfs mkdir fails
    // on existing directories, and mke2fs only pre-creates the base layout).
    let mut directories = std::collections::BTreeSet::new();
    // Ancestors of the root (e.g. /usr, /usr/lib, /usr/lib/python3) come
    // first; ancestors sort before the root and its children, so one pass of
    // ensure_image_dirs over the sorted set is enough.
    let mut ancestor = String::new();
    for component in image_root.split('/').filter(|part| !part.is_empty()) {
        ancestor.push('/');
        ancestor.push_str(component);
        directories.insert(ancestor.clone());
    }
    for (rel, _) in &files {
        let mut parent = Path::new(rel).parent();
        while let Some(path) = parent {
            if !path.as_os_str().is_empty() {
                directories.insert(format!("{image_root}/{}", path.to_string_lossy()));
            }
            parent = path.parent();
        }
    }
    ensure_image_dirs(image_path, directories)?;
    for (rel, source) in files {
        install_image_file(image_path, &source, &format!("{image_root}/{rel}"), None)?;
    }
    Ok(())
}

/// Create every directory in `dirs` (BTreeSet-ordered: parents before
/// children; debugfs mkdir neither creates parents nor tolerates re-creates).
fn ensure_image_dirs(
    image_path: &Path,
    dirs: impl IntoIterator<Item = String>,
) -> Result<(), CliError> {
    for directory in dirs {
        if run_debugfs(
            image_path,
            &format!("stat {}", debugfs_quote(&directory)),
            false,
        )
        .is_err()
        {
            run_debugfs(
                image_path,
                &format!("mkdir {}", debugfs_quote(&directory)),
                false,
            )?;
        }
    }
    Ok(())
}

/// Write one file into the image, clear any pre-created inode first, apply an
/// optional mode, and digest-verify the installed bytes.
fn install_image_file(
    image_path: &Path,
    source: &Path,
    destination: &str,
    mode: Option<u32>,
) -> Result<(), CliError> {
    if run_debugfs(
        image_path,
        &format!("stat {}", debugfs_quote(destination)),
        false,
    )
    .is_ok()
    {
        let _ = run_debugfs(
            image_path,
            &format!("unlink {}", debugfs_quote(destination)),
            false,
        );
        let _ = run_debugfs(
            image_path,
            &format!("rm {}", debugfs_quote(destination)),
            false,
        );
    }
    run_debugfs(
        image_path,
        &format!(
            "write {} {}",
            debugfs_quote(&source.to_string_lossy()),
            debugfs_quote(destination)
        ),
        false,
    )?;
    if let Some(mode) = mode {
        // debugfs parses the mode as octal only with a leading zero
        // (e.g. 0100644 = S_IFREG|0644).
        run_debugfs(
            image_path,
            &format!(
                "set_inode_field {} mode {:07o}",
                debugfs_quote(destination),
                mode | 0o100000
            ),
            false,
        )?;
    }
    crate::cli::image_build::verify_installed_file(image_path, source, destination)
}

/// Validate one build-script file mapping. The guest path must be absolute
/// and normalize inside the image (no `..`, no empty segments at the end).
pub fn validate_extra_guest_path(guest: &str) -> Result<(), CliError> {
    if !guest.starts_with('/') {
        return Err(validation(format!(
            "extra file guest path must be absolute: {guest:?}"
        )));
    }
    if guest.len() > 4096 || guest.as_bytes().contains(&0) || guest.contains('\\') {
        return Err(validation(format!(
            "extra file guest path is malformed: {guest:?}"
        )));
    }
    if guest.split('/').any(|segment| segment == "..") {
        return Err(validation(format!(
            "extra file guest path must not contain '..': {guest:?}"
        )));
    }
    if guest.len() > 1 && guest.ends_with('/') {
        return Err(validation(format!(
            "extra file guest path must name a file, not a directory: {guest:?}"
        )));
    }
    Ok(())
}

/// Bake build-script payload files into the image: create every parent
/// directory, write the file, set its mode, and digest-verify the bytes.
fn install_extra_files(image_path: &Path, files: &[ExtraFile]) -> Result<(), CliError> {
    // Every ancestor of every destination (sorted so parents precede children).
    let mut directories = std::collections::BTreeSet::new();
    for extra in files {
        validate_extra_guest_path(&extra.guest)?;
        if !extra.source.is_file() {
            return Err(validation(format!(
                "extra file source is not a readable file: {}",
                extra.source.display()
            )));
        }
        let guest = Path::new(&extra.guest);
        let mut parent = guest.parent();
        while let Some(path) = parent {
            let text = path.to_string_lossy();
            if !text.is_empty() && text != "/" {
                directories.insert(text.into_owned());
            }
            parent = path.parent();
        }
    }
    ensure_image_dirs(image_path, directories)?;
    for extra in files {
        install_image_file(image_path, &extra.source, &extra.guest, Some(extra.mode))?;
    }
    Ok(())
}

pub(super) fn debugfs_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| !character.is_whitespace() && character != '"')
    {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
