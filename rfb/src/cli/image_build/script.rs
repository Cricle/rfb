//! `build.rfb` — declarative sandbox image build scripts.
//!
//! A build script is one TOML file that describes a complete sandbox image:
//! which interpreters to embed (`python` = RustPython, `lua` = mlua), which
//! offline package trees to bake, which prebuilt Rust app binaries to install,
//! and any extra payload files. `rfb-cli image build-script` drives the whole
//! chain (static runtime build → rootfs assembly → verification) from it:
//!
//! ```toml
//! schema = "rfb-build/v1"
//! mode = "zeroboot-zbrt"            # or "forkd-agent" / "rfb-vsock"
//! output = "resx/rootfs/my.ext4"
//! force = true
//!
//! [interpreters]
//! python = true                     # /bin/python3 (embeds RustPython)
//! lua = true                        # /bin/lua (embeds mlua 5.4)
//!
//! [packages]
//! py-site = "rfb-sites/py"          # baked to /usr/lib/python3/site-packages
//! lua-lib = "rfb-sites/lua"         # baked to /usr/lib/lua/5.4
//!
//! [rust]
//! apps = ["apps/hello"]             # prebuilt musl binaries -> /usr/local/bin
//!
//! [files]
//! "assets/banner.txt" = "/etc/motd" # host path -> guest path
//! ```
//!
//! Host-side relative paths resolve against the script's own directory, so a
//! script is relocatable. Nothing here talks to the network: packages are
//! offline trees, Rust support means baking prebuilt (musl) binaries.

use super::{
    build_rootfs, build_static_runtime, validate_extra_guest_path, ExtraFile, RootfsOptions,
};
use crate::cli::error::{external, io, validation, CliError};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The only schema string this parser accepts.
pub const SCRIPT_SCHEMA: &str = "rfb-build/v1";

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct BuildScript {
    /// Schema tag; must be exactly [`SCRIPT_SCHEMA`].
    pub schema: String,
    /// Rootfs mode: `zeroboot-zbrt`, `forkd-agent`, or `rfb-vsock`.
    pub mode: String,
    /// Output ext4 path (relative to the script directory).
    pub output: PathBuf,
    /// Overwrite an existing output.
    #[serde(default)]
    pub force: bool,
    /// Image size in MiB (default: derived from content).
    pub size_mb: Option<u64>,
    /// Rust target triple (default `x86_64-unknown-linux-musl`).
    pub target: Option<String>,
    /// Embedded interpreters.
    #[serde(default)]
    pub interpreters: Interpreters,
    /// Offline package trees.
    #[serde(default)]
    pub packages: Packages,
    /// Prebuilt (musl) Rust app binaries baked to `/usr/local/bin`.
    #[serde(default)]
    pub rust: RustApps,
    /// Extra payload files: host path -> absolute guest path.
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub struct Interpreters {
    /// Embed RustPython and install `/bin/python3`.
    #[serde(default)]
    pub python: bool,
    /// Embed mlua 5.4 and install `/bin/lua`.
    #[serde(default)]
    pub lua: bool,
}

#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Packages {
    /// Local tree of pure-Python packages (baked to `PY_SITE_PACKAGES`).
    pub py_site: Option<PathBuf>,
    /// Local tree of pure-Lua modules (baked to `LUA_LIB_DIR`).
    pub lua_lib: Option<PathBuf>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub struct RustApps {
    /// Prebuilt static binaries to bake into `/usr/local/bin`.
    #[serde(default)]
    pub apps: Vec<String>,
}

/// Parse a `build.rfb` script body into a `BuildScript`, enforcing the
/// schema tag and the mode vocabulary. Exposed for tests and diagnostics.
pub fn parse_script(text: &str) -> Result<BuildScript, CliError> {
    let script: BuildScript = toml::from_str(text)
        .map_err(|error| validation(format!("invalid build script TOML: {error}")))?;
    if script.schema != SCRIPT_SCHEMA {
        return Err(validation(format!(
            "schema must be {SCRIPT_SCHEMA:?}, got {:?}",
            script.schema
        )));
    }
    if !["zeroboot-zbrt", "forkd-agent", "rfb-vsock"].contains(&script.mode.as_str()) {
        return Err(validation(format!(
            "mode must be zeroboot-zbrt, forkd-agent, or rfb-vsock, got {:?}",
            script.mode
        )));
    }
    Ok(script)
}

/// The cargo feature list implied by the script's interpreter selections.
pub fn features_for(script: &BuildScript) -> String {
    let mut features = "cli".to_owned();
    if script.interpreters.python {
        features.push_str(",rustpython");
    }
    if script.interpreters.lua {
        features.push_str(",mlua");
    }
    features
}

/// Build one image from a `build.rfb` script.
///
/// The full chain: static runtime build (features derived from
/// `[interpreters]`) → rootfs assembly (interpreters, offline packages, Rust
/// apps, extra files) → digest verification → sidecar artifacts. Relative
/// host paths in the script resolve against the script's own directory; the
/// cargo build runs in `root` (the workspace root).
///
/// # Errors
///
/// Returns `Err` when any stage fails; the error type carries the cause.
pub fn build_from_script(
    root: &Path,
    script_path: &Path,
    force_override: Option<bool>,
) -> Result<Value, CliError> {
    let text = std::fs::read_to_string(script_path)
        .map_err(|error| io(format!("read {}: {error}", script_path.display())))?;
    let script = parse_script(&text)?;
    let force = force_override.unwrap_or(script.force);
    let base = script_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let resolve = |path: &Path| -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        }
    };

    // Stage 1: static runtime with the script's interpreter features.
    let target = script
        .target
        .clone()
        .unwrap_or_else(|| "x86_64-unknown-linux-musl".to_owned());
    let runtime = build_static_runtime(root, &target, "rfb-runtime", &features_for(&script))?;
    let binary = runtime["binary"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| external("static runtime build did not report a binary path"))?;

    // Stage 2: payload inventory.
    let mut extra_files = Vec::new();
    for (host, guest) in &script.files {
        validate_extra_guest_path(guest)?;
        extra_files.push(ExtraFile {
            source: resolve(Path::new(host)),
            guest: guest.clone(),
            mode: 0o644,
        });
    }
    for app in &script.rust.apps {
        let source = resolve(Path::new(app));
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| validation(format!("rust app path has no file name: {app:?}")))?;
        extra_files.push(ExtraFile {
            source,
            guest: format!("/usr/local/bin/{name}"),
            mode: 0o755,
        });
    }

    // Stage 3: rootfs assembly with everything the script asked for.
    let options = RootfsOptions {
        with_python: script.interpreters.python,
        with_lua: script.interpreters.lua,
        py_site_dir: script.packages.py_site.as_ref().map(|p| resolve(p)),
        lua_lib_dir: script.packages.lua_lib.as_ref().map(|p| resolve(p)),
        extra_files,
    };
    let output = resolve(&script.output);
    let rootfs = build_rootfs(
        &binary,
        &output,
        script.size_mb,
        &script.mode,
        false,
        force,
        &options,
    )?;

    Ok(json!({
        "ok": true,
        "script": script_path,
        "schema": SCRIPT_SCHEMA,
        "mode": script.mode,
        "features": features_for(&script),
        "rust_apps": script.rust.apps,
        "files": script.files,
        "runtime": runtime,
        "rootfs": rootfs,
    }))
}
