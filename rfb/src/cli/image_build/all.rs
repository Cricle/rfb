//! `rfb-cli image build-all`: the single command that produces a runnable
//! sandbox image from source. It chains the static runtime build (with the
//! requested cargo features), the rootfs assembly (interpreter hardlinks and
//! offline extension packages follow the features), and — when `--kernel` is
//! given — the kernel check plus a real Firecracker boot verification. This
//! replaces ad-hoc shell pipelines: no temporary scripts, one artifact flow.

use crate::cli::commands::BuildAllArgs;
use crate::cli::error::{external, validation, CliError};
use crate::cli::image_build::{build_rootfs, build_static_runtime, check_kernel, RootfsOptions};
use serde_json::{json, Value};

/// Run the full build pipeline and return a JSON report of every stage.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub fn build_all(args: &BuildAllArgs) -> Result<Value, CliError> {
    let features = args.features.trim();
    if features.is_empty() {
        return Err(validation("features must not be empty"));
    }
    let wants = |feature: &str| features.split(',').any(|token| token.trim() == feature);
    let with_python = wants("rustpython");
    let with_lua = wants("mlua");

    // Stage 1: static runtime binary for the target triple.
    let runtime = build_static_runtime(&args.root, &args.target, &args.package, features)?;
    let binary = runtime["binary"]
        .as_str()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| external("static runtime build did not report a binary path"))?;

    // Stage 2: rootfs image with interpreter hardlinks + offline packages.
    let options = RootfsOptions {
        with_python,
        with_lua,
        py_site_dir: args.py_site_dir.clone(),
        lua_lib_dir: args.lua_lib_dir.clone(),
    };
    let rootfs = build_rootfs(
        &binary,
        &args.output,
        args.size_mb,
        &args.mode,
        false,
        args.force,
        &options,
    )?;

    let mut report = json!({
        "ok": true,
        "runtime": runtime,
        "rootfs": rootfs,
    });

    // Stage 3 (optional): kernel validation + real Firecracker boot check.
    match args.kernel.as_deref() {
        Some(kernel) => {
            let kernel_report = check_kernel(kernel)?;
            report["kernel"] = kernel_report;
            #[cfg(unix)]
            {
                let verify_report = crate::cli::zeroboot::verify(
                    kernel,
                    &args.output,
                    &args.firecracker,
                    args.require_vm,
                    false,
                )?;
                report["verify"] = verify_report;
            }
            #[cfg(not(unix))]
            {
                let _ = args.require_vm;
                return Err(validation(
                    "--kernel boot verification requires a Unix host with KVM",
                ));
            }
        }
        None => {
            let _ = args.require_vm;
        }
    }
    Ok(report)
}
