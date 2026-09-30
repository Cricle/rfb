//! `rfb-cli` process helpers shared by the CLI contract suites (`cli`,
//! `cli_backend`) plus the env-mutation guard from `backend_factory`.
//!
//! Distinct from the top-level [`super::cli`] (which pins the workspace root
//! as current directory for the resx-resolution E2E driver): the contract
//! suites spawn the binary with inherited cwd and no timeout.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
#[cfg(feature = "cli")]
use std::process::{Command, Output};

/// Spawn the real `rfb-cli` binary with the caller's working directory.
#[cfg(feature = "cli")]
pub fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rfb-cli"))
}

/// Run the CLI with `args` and collect stdout/stderr (no timeout: these are
/// argument-validation contracts that never block).
#[cfg(feature = "cli")]
pub fn run(args: &[&str]) -> Output {
    cli().args(args).output().expect("run rfb-cli")
}

#[cfg(feature = "cli")]
pub fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(feature = "cli")]
pub fn combined_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `std::env` manipulation is process-global; serialize the env-dependent
/// tests so parallel test threads cannot race on the variables they swap.
pub fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();
    let saved: Vec<(String, Option<String>)> = vars
        .iter()
        .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
        .collect();
    for (key, value) in vars {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
    let result = f();
    for (key, saved_value) in saved {
        match saved_value {
            Some(v) => std::env::set_var(&key, v),
            None => std::env::remove_var(&key),
        }
    }
    result
}

/// Write a staging manifest JSON document at `dir/manifest.json` and return
/// its path. `Value::Null` for `image` serializes as the absent-image shape
/// (`"image": null`), matching `cli_image_build.rs`'s helper verbatim.
pub fn write_staging_manifest(dir: &Path, files: Value, image: Value) -> PathBuf {
    let manifest_path = dir.join("manifest.json");
    write_staging_manifest_at(&manifest_path, files, image);
    manifest_path
}

/// [`write_staging_manifest`] at an explicit path (the `cli.rs` superset:
/// manifest filenames other than `manifest.json`).
pub fn write_staging_manifest_at(path: &Path, files: Value, image: Value) {
    let value = json!({
        "format": "rfb-cli-staging/v1",
        "image_size_bytes": 4096,
        "block_size": 4096,
        "staging": "staging",
        "files": files,
        "image": image,
    });
    std::fs::write(
        path,
        serde_json::to_vec(&value).expect("serialize manifest"),
    )
    .expect("write manifest");
}
