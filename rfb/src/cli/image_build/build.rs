//! Image building: ext4 creation from a staging manifest, rootfs images from a
//! runtime binary, and profile diagnostics. Delegates all ext4 manipulation to
//! e2fsprogs (`mke2fs`/`debugfs`) and verifies every injected file by digest.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::image_build::manifest::{
    safe_join, sha256, sha256_bytes, validate, StagingManifest,
};
use crate::cli::image_build::verify::{is_dynamically_linked, run_debugfs};
use crate::cli::tool::tool_available;
use crate::image_profiles::ImageManifestProfile;
use serde_json::{json, Value};
use std::{fs, io::Write, path::Path, process::Command};
use tempfile::NamedTempFile;

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
) -> Result<Value, CliError> {
    let manifest = validate(manifest_path)?;
    let output_path = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest_path.with_extension("img"));
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

    let status = Command::new("mke2fs")
        .args([
            "-t",
            "ext4",
            "-F",
            output_path.to_str().unwrap_or("image.img"),
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
            &output_path,
            &format!("stat {}", debugfs_quote(&directory)),
            false,
        )
        .is_err()
        {
            run_debugfs(
                &output_path,
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
            &output_path,
            &format!(
                "write {} {}",
                debugfs_quote(&source.to_string_lossy()),
                debugfs_quote(&destination)
            ),
            false,
        )?;
        let actual = run_debugfs(
            &output_path,
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

    let digest = sha256(&output_path).map_err(|error| io(error.to_string()))?;
    if let Some(expected) = image.digest.as_deref() {
        let expected = expected.strip_prefix("sha256:").unwrap_or(expected);
        if !digest.eq_ignore_ascii_case(expected) {
            return Err(external(format!(
                "image digest mismatch: expected {expected}, got sha256:{digest}"
            )));
        }
    }
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
    // Interpreter hardlinks/site dirs are only wired in the ZBRT image layout.
    if mode != "zeroboot-zbrt"
        && (options.with_python
            || options.with_lua
            || options.py_site_dir.is_some()
            || options.lua_lib_dir.is_some())
    {
        return Err(validation(
            "interpreter options (--with-python/--with-lua/--py-site-dir/--lua-lib-dir) require --mode zeroboot-zbrt",
        ));
    }

    // ZBRT images are read-mostly: only /workspace receives writes, so size
    // from the installed content plus a bounded write headroom instead of the
    // one-size 32 MiB floor used by rw system images.
    let zbrt = mode == "zeroboot-zbrt";
    let floor_mb = if zbrt { 4 } else { 32 };
    let headroom_bytes: u64 = 4 * 1024 * 1024;
    let mut content_bytes = fs::metadata(runtime_bin)
        .map_err(|error| io(error.to_string()))?
        .len();
    // /bin/sh (rfb-busybox) installs in every image mode.
    if let Some(parent) = runtime_bin.parent() {
        content_bytes += fs::metadata(parent.join("rfb-busybox"))
            .map(|meta| meta.len())
            .unwrap_or(0);
    }
    if zbrt {
        // The multi-call applets are installed alongside the runtime binary.
        // Interpreter hardlinks add zero bytes; site-package trees are real
        // content.
        if let Some(parent) = runtime_bin.parent() {
            content_bytes += fs::metadata(parent.join("rfb-mini-tools"))
                .map(|meta| meta.len())
                .unwrap_or(0);
        }
        for dir in [
            options.py_site_dir.as_deref(),
            options.lua_lib_dir.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            content_bytes += site_tree_bytes(dir)?;
        }
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
    let status = Command::new("mke2fs")
        .args(["-q", "-t", "ext4", "-F", &image])
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
        let _ = run_debugfs(&image_path, "unlink /bin/sh", false);
        let _ = run_debugfs(&image_path, "rm /bin/sh", false);
        run_debugfs(
            &image_path,
            &format!(
                "write {} /bin/sh",
                debugfs_quote(&busybox_src.to_string_lossy())
            ),
            false,
        )?;
        run_debugfs(&image_path, "set_inode_field /bin/sh mode 0100755", false)?;
        // Hardlinked multi-call entry points (same inode, zero extra image
        // bytes): `bash` is the same shell under its alternative name and
        // `sleep` the one coreutil the timeout/eval contracts exercise.
        // Unimplemented applet names stay absent rather than
        // present-but-broken.
        for applet in ["bash", "sleep"] {
            run_debugfs(&image_path, &format!("ln /bin/sh /bin/{applet}"), false)?;
        }
    }
    run_debugfs(
        &image_path,
        &format!(
            "write {} {install_path}",
            debugfs_quote(&runtime_bin.to_string_lossy())
        ),
        false,
    )?;
    run_debugfs(
        &image_path,
        &format!("set_inode_field {install_path} mode 0100755"),
        false,
    )?;
    if mode == "forkd-agent" {
        run_debugfs(
            &image_path,
            &format!(
                "write {} {entrypoint}",
                debugfs_quote(&runtime_bin.to_string_lossy())
            ),
            false,
        )?;
        run_debugfs(
            &image_path,
            &format!("set_inode_field {entrypoint} mode 0100755"),
            false,
        )?;
    }
    if mode == "zeroboot-zbrt" {
        let marker = NamedTempFile::new().map_err(|error| io(error.to_string()))?;
        fs::write(marker.path(), "zbrt\n").map_err(|error| io(error.to_string()))?;
        run_debugfs(
            &image_path,
            &format!(
                "write {} /etc/zeroboot-protocol",
                debugfs_quote(&marker.path().to_string_lossy())
            ),
            false,
        )?;
        fs::write(marker.path(), "1\n").map_err(|error| io(error.to_string()))?;
        run_debugfs(
            &image_path,
            &format!(
                "write {} /etc/zeroboot-protocol-version",
                debugfs_quote(&marker.path().to_string_lossy())
            ),
            false,
        )?;
        // Single source of truth: same vocabulary the guest HelloAck and
        // `rfb-cli zeroboot verify` use (crate::protocol re-exports the
        // canonical const from rfb-runtime).
        fs::write(
            marker.path(),
            format!("{}\n", crate::protocol::ZBRT_V1_CAPABILITIES.join(",")),
        )
        .map_err(|error| io(error.to_string()))?;
        run_debugfs(
            &image_path,
            &format!(
                "write {} /etc/zeroboot-capabilities",
                debugfs_quote(&marker.path().to_string_lossy())
            ),
            false,
        )?;
        fs::write(marker.path(), "5000\n").map_err(|error| io(error.to_string()))?;
        run_debugfs(
            &image_path,
            &format!(
                "write {} /etc/zeroboot-guest-port",
                debugfs_quote(&marker.path().to_string_lossy())
            ),
            false,
        )?;
    }
    if mode == "rfb-vsock" {
        // Write protocol-version and environment markers via debugfs `write`
        // from temp files (debugfs cannot inline stdin).
        let work = NamedTempFile::new().map_err(|error| io(error.to_string()))?;
        fs::write(work.path(), "1\n").map_err(|error| io(error.to_string()))?;
        run_debugfs(
            &image_path,
            &format!(
                "write {} /etc/rfb-runtime/protocol-version",
                debugfs_quote(&work.path().to_string_lossy())
            ),
            false,
        )?;
        fs::write(
            work.path(),
            "RFB_RUNTIME_EXECUTOR=workspace\nRFB_RUNTIME_WORKSPACE=/workspace\n",
        )
        .map_err(|error| io(error.to_string()))?;
        run_debugfs(
            &image_path,
            &format!(
                "write {} /etc/rfb-runtime/environment",
                debugfs_quote(&work.path().to_string_lossy())
            ),
            false,
        )?;
    }

    // Verify every published contract artifact before reporting success.
    let entry_stat = run_debugfs(&image_path, &format!("stat {entrypoint}"), true)?;
    let entry_stat = String::from_utf8_lossy(&entry_stat);
    if !entry_stat.contains("Type:")
        || !entry_stat.contains("regular")
        || !(entry_stat.contains("0755") || entry_stat.contains("-rwxr-xr-x"))
    {
        return Err(external(format!(
            "entrypoint verification failed: {entrypoint}; debugfs stat: {}",
            entry_stat.trim().replace('\n', " | ")
        )));
    }
    let installed = run_debugfs(&image_path, &format!("cat {install_path}"), true)?;
    let runtime_bytes = fs::read(runtime_bin).map_err(|error| io(error.to_string()))?;
    if installed != runtime_bytes {
        return Err(external(format!(
            "runtime verification failed: {install_path}"
        )));
    }
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
        let marker = run_debugfs(&image_path, "cat /etc/zeroboot-protocol", true)?;
        let marker = String::from_utf8_lossy(&marker).trim().to_owned();
        let version = run_debugfs(&image_path, "cat /etc/zeroboot-protocol-version", true)?;
        let version = String::from_utf8_lossy(&version).trim().to_owned();
        let capabilities = run_debugfs(&image_path, "cat /etc/zeroboot-capabilities", true)?;
        let capabilities = String::from_utf8_lossy(&capabilities).trim().to_owned();
        let guest_port = run_debugfs(&image_path, "cat /etc/zeroboot-guest-port", true)?;
        let guest_port = String::from_utf8_lossy(&guest_port).trim().to_owned();
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
            // Clear any pre-created applet before writing the mini-tools binary.
            if run_debugfs(&image_path, &format!("stat {target}"), false).is_ok() {
                let _ = run_debugfs(&image_path, &format!("unlink {target}"), false);
                let _ = run_debugfs(&image_path, &format!("rm {target}"), false);
            }
            run_debugfs(
                &image_path,
                &format!(
                    "write {} {target}",
                    debugfs_quote(&mini_tools.to_string_lossy())
                ),
                false,
            )?;
            run_debugfs(
                &image_path,
                &format!("set_inode_field {target} mode 0100755"),
                false,
            )?;
        }
        // Interpreter multi-call hardlinks: /bin/python3 and /bin/lua point
        // at /init, which dispatches on argv[0]. Zero extra image bytes; the
        // interpreter itself must be compiled into the runtime binary via the
        // `rustpython`/`mlua` cargo features.
        if options.with_python {
            install_hardlink(&image_path, "/init", "/bin/python3")?;
        }
        if options.with_lua {
            install_hardlink(&image_path, "/init", "/bin/lua")?;
        }
        // Offline extension packages: static files under the interpreter
        // import roots, written and digest-verified like every other payload.
        install_site_dir(
            &image_path,
            options.py_site_dir.as_deref(),
            PY_SITE_PACKAGES,
        )?;
        install_site_dir(&image_path, options.lua_lib_dir.as_deref(), LUA_LIB_DIR)?;
    }
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
    // debugfs mkdir does not create parents, so every ancestor of the root
    // (e.g. /usr, /usr/lib, /usr/lib/python3) must exist before the root can
    // be created. Ancestors sort before the root and its children, so one
    // lexicographically ordered pass is enough.
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
    for directory in directories {
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
    for (rel, source) in files {
        let destination = format!("{image_root}/{rel}");
        run_debugfs(
            image_path,
            &format!(
                "write {} {}",
                debugfs_quote(&source.to_string_lossy()),
                debugfs_quote(&destination)
            ),
            false,
        )?;
        let expected = fs::read(&source).map_err(|error| io(error.to_string()))?;
        let actual = run_debugfs(
            image_path,
            &format!("cat {}", debugfs_quote(&destination)),
            true,
        )?;
        if actual != expected {
            return Err(external(format!(
                "site package verification failed: {destination}"
            )));
        }
    }
    Ok(())
}

fn debugfs_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| !character.is_whitespace() && character != '"')
    {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
