//! Command dispatch for `rfb-cli`. The bin parses args and renders output;
//! this module owns the mapping from parsed commands to the CLI library
//! operations (image, forkd, rfb1, zeroboot, cleanup, bench, web, doctor).

use crate::cli::cleanup;
use crate::cli::commands::{
    BenchCommand, CommandLine, ForkdCommand, ImageCommand, RunTarget, SkillsCommand,
};
#[cfg(unix)]
use crate::cli::commands::{Rfb1Command, WebCommand, ZerobootCommand};
use crate::cli::error::{render_output, validation, CliError};
use crate::cli::forkd;
use crate::cli::host;
use crate::cli::image_build;
use crate::cli::localhost::{require_localhost, require_snapshot_tag};
#[cfg(unix)]
use crate::cli::rfb1;
use crate::cli::tool;
use serde_json::{json, Value};
use std::time::Duration;

/// Run the parsed top-level command.
pub fn run(cli: crate::cli::commands::Cli) -> Result<(), CliError> {
    match cli.command {
        CommandLine::Doctor => doctor(cli.json),
        CommandLine::Image { command } => image(cli.json, command),
        CommandLine::Forkd { command } => forkd(cli.json, command),
        #[cfg(unix)]
        CommandLine::Rfb1 { command } => rfb1(cli.json, command),
        #[cfg(unix)]
        CommandLine::Zeroboot { command } => zeroboot(cli.json, command),
        CommandLine::Cleanup(args) => {
            let value = cleanup::cleanup(&args.target, args.dry_run, args.yes)?;
            render_output(cli.json, value, "cleanup".to_owned());
            Ok(())
        }
        CommandLine::Skills { command } => skills(command),
        CommandLine::Run(args) => run_target(cli.json, args.target),
        CommandLine::Bench { command } => bench(cli.json, command),
        #[cfg(unix)]
        CommandLine::Web { command } => web(cli.json, command),
    }
}

fn image(json_out: bool, command: ImageCommand) -> Result<(), CliError> {
    match command {
        ImageCommand::Inspect(args) => {
            let manifest = image_build::load(&args.manifest)?;
            let image = manifest.core_image()?;
            let (transport, protocol, capabilities, profile) =
                image_build::image_diagnostics(&manifest);
            let entrypoint = manifest
                .image
                .as_ref()
                .map(|image| image.entrypoint.clone())
                .unwrap_or_default();
            let mut value = serde_json::to_value(&manifest)
                .map_err(|error| crate::cli::error::io(error.to_string()))?;
            if let Value::Object(ref mut object) = value {
                object.insert("entrypoint".to_owned(), json!(entrypoint));
                object.insert("transport".to_owned(), json!(transport));
                object.insert("protocol".to_owned(), json!(protocol));
                object.insert("capabilities".to_owned(), json!(capabilities));
                object.insert("profile".to_owned(), json!(profile));
            }
            render_output(
                json_out,
                value,
                format!(
                    "format: {}\nimage size: {} bytes\nblock size: {}\nstaging: {}\nfiles: {}\ncore image: {}\ntransport: {}\nprotocol: {}\ncapabilities: {}\nprofile: {}",
                    manifest.format,
                    manifest.image_size_bytes,
                    manifest.block_size,
                    manifest.staging,
                    manifest.files.len(),
                    if image.is_some() { "present" } else { "absent" },
                    transport,
                    protocol,
                    capabilities.join(", "),
                    profile
                ),
            );
            Ok(())
        }
        ImageCommand::Validate(args) => {
            image_build::validate(&args.manifest)?;
            render_output(
                json_out,
                json!({"ok": true, "manifest": args.manifest}),
                "valid".to_owned(),
            );
            Ok(())
        }
        ImageCommand::Init(args) => {
            let value = image_build::init(&args.directory, args.size, args.force)?;
            render_output(json_out, value, "initialized".to_owned());
            Ok(())
        }
        ImageCommand::Build(args) => {
            let value = image_build::build(&args.manifest, args.output.as_deref(), args.execute)?;
            render_output(json_out, value, "built".to_owned());
            Ok(())
        }
        ImageCommand::BuildRootfs(args) => {
            let options = image_build::RootfsOptions {
                with_python: args.with_python,
                with_lua: args.with_lua,
                py_site_dir: args.py_site_dir.clone(),
                lua_lib_dir: args.lua_lib_dir.clone(),
            };
            let value = image_build::build_rootfs(
                &args.runtime,
                &args.output,
                args.size_mb,
                &args.mode,
                args.allow_dynamic,
                args.force,
                &options,
            )?;
            render_output(json_out, value, "rootfs built".to_owned());
            Ok(())
        }
        ImageCommand::CheckKernel(args) => {
            let value = image_build::check_kernel(&args.manifest)?;
            render_output(json_out, value, "kernel valid".to_owned());
            Ok(())
        }
        ImageCommand::BuildStatic(args) => {
            let value = image_build::build_static_runtime(
                &args.root,
                &args.target,
                &args.package,
                &args.features,
            )?;
            render_output(json_out, value, "static runtime built".to_owned());
            Ok(())
        }
        ImageCommand::BuildAll(args) => {
            let value = image_build::build_all(&args)?;
            render_output(json_out, value, "image built".to_owned());
            Ok(())
        }
    }
}

fn forkd(json_out: bool, command: ForkdCommand) -> Result<(), CliError> {
    match command {
        ForkdCommand::Preflight(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let artifact = if let Some(path) = args.artifact_manifest.as_deref() {
                let artifact = image_build::ArtifactManifest::load(path)?;
                if artifact.backend != "forkd" {
                    return Err(validation("artifact manifest backend must be forkd"));
                }
                Some(artifact)
            } else {
                None
            };
            if let Some(path) = args.snapshot_binding.as_deref() {
                // Strict snapshot-binding validation: schema, tag, ready/bootable,
                // verification status, artifact identity (and backend=forkd), plus
                // record-hash recomputation.
                let artifact = artifact.as_ref();
                forkd::load_snapshot_binding(path, tag, artifact)?;
            }
            let value = block_on(forkd::preflight(&url, tag, args.require_vm))?;
            let text = if json_out {
                String::new()
            } else {
                forkd::preflight_text(&value)
            };
            render_output(json_out, value, text);
            Ok(())
        }
        ForkdCommand::SnapshotBind(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let binding = block_on(forkd::snapshot_bind(
                &url,
                tag,
                &args.artifact,
                args.output.as_deref(),
                args.require_provenance,
            ))?;
            render_output(json_out, binding.value, binding.text);
            Ok(())
        }
        ForkdCommand::Acceptance(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::acceptance(
                &url,
                tag,
                args.require_vm,
                args.require_provenance,
            ))?;
            render_output(json_out, value, "acceptance".to_owned());
            Ok(())
        }
        ForkdCommand::Benchmark(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            if args.n == 0 {
                return Err(validation("--n must be a positive integer"));
            }
            let value = block_on(forkd::benchmark(&url, tag, args.n, Duration::from_secs(10)))?;
            render_output(json_out, value, "benchmark".to_owned());
            Ok(())
        }
        ForkdCommand::Workload(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::workload(
                &url,
                tag,
                args.sandboxes,
                args.rounds,
                args.reuse_execs,
            ))?;
            render_output(json_out, value, "workload".to_owned());
            Ok(())
        }
        ForkdCommand::SandboxCreate(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::create_sandbox(&url, tag, 1, Some(32)))?;
            render_output(json_out, json!(value), "sandbox created".to_owned());
            Ok(())
        }
        ForkdCommand::SandboxDestroy(args) => {
            let url = require_localhost(&args.url)?;
            block_on(forkd::destroy_sandbox(&url, &args.id))?;
            render_output(
                json_out,
                json!({"ok": true, "sandbox": args.id}),
                "destroyed".to_owned(),
            );
            Ok(())
        }
        ForkdCommand::SnapshotCreate(args) => {
            let output = forkd::snapshot_create(&args)?;
            render_output(json_out, output.value, output.text);
            Ok(())
        }
        ForkdCommand::SnapshotInfo(args) => {
            let output = forkd::snapshot_info(&args)?;
            render_output(json_out, output.value, output.text);
            Ok(())
        }
        ForkdCommand::SnapshotDelete(args) => {
            let output = forkd::snapshot_delete(&args)?;
            render_output(json_out, output.value, output.text);
            Ok(())
        }
    }
}

#[cfg(unix)]
fn rfb1(json_out: bool, command: Rfb1Command) -> Result<(), CliError> {
    match command {
        Rfb1Command::Acceptance(args) => {
            let value = rfb1::acceptance(
                &args.kernel,
                &args.rootfs,
                &args.firecracker,
                args.require_vm,
            )?;
            render_output(json_out, value, "rfb1 acceptance".to_owned());
            Ok(())
        }
    }
}

#[cfg(unix)]
fn zeroboot(json_out: bool, command: ZerobootCommand) -> Result<(), CliError> {
    match command {
        ZerobootCommand::Verify(args) => {
            let value = crate::cli::zeroboot::verify(
                &args.kernel,
                &args.rootfs,
                &args.firecracker,
                args.require_vm,
                args.bench,
            )?;
            render_output(json_out, value, "zeroboot verify".to_owned());
            Ok(())
        }
    }
}

fn bench(json_out: bool, command: BenchCommand) -> Result<(), CliError> {
    match command {
        BenchCommand::Forkd(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::benchmark(&url, tag, args.n, Duration::from_secs(10)))?;
            render_output(json_out, value, "benchmark".to_owned());
            Ok(())
        }
        BenchCommand::List => {
            render_output(
                json_out,
                json!({"ok": true, "benchmarks": ["rfb-runtime/codec"]}),
                "cargo bench -p rfb-runtime --bench codec".to_owned(),
            );
            Ok(())
        }
    }
}

#[cfg(unix)]
fn web(json_out: bool, command: WebCommand) -> Result<(), CliError> {
    match command {
        WebCommand::Bench(args) => {
            let value = block_on(crate::cli::web_bench::bench(args))?;
            render_output(json_out, value, "web bench".to_owned());
            Ok(())
        }
    }
}

/// Embedded agent-readable skills: `list` advertises, `read` prints raw
/// markdown (or a JSON envelope with `--json`).
fn skills(command: SkillsCommand) -> Result<(), CliError> {
    match command {
        SkillsCommand::List { path } => {
            let value = crate::cli::skills::list(path.as_deref());
            // List output is machine-first (like `lark-cli skills list`):
            // always a JSON envelope.
            render_output(true, value, String::new());
            Ok(())
        }
        SkillsCommand::Read { name, json } => {
            let content = crate::cli::skills::read(&name)?;
            if json {
                println!("{}", crate::cli::skills::content_json(&content)?);
            } else {
                println!("{}", content.content);
            }
            Ok(())
        }
    }
}

/// Dispatch the `run` subcommand used by the thin shell wrappers.
fn run_target(json_out: bool, target: RunTarget) -> Result<(), CliError> {
    match target {
        RunTarget::Forkd(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::acceptance(
                &url,
                tag,
                args.require_vm,
                args.require_provenance,
            ))?;
            render_output(json_out, value, "acceptance".to_owned());
            Ok(())
        }
        #[cfg(unix)]
        RunTarget::Rfb1(args) => {
            let value = rfb1::acceptance(
                &args.kernel,
                &args.rootfs,
                &args.firecracker,
                args.require_vm,
            )?;
            render_output(json_out, value, "rfb1 acceptance".to_owned());
            Ok(())
        }
        RunTarget::Workload(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::workload(
                &url,
                tag,
                args.sandboxes,
                args.rounds,
                args.reuse_execs,
            ))?;
            render_output(json_out, value, "workload".to_owned());
            Ok(())
        }
        RunTarget::Benchmark(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::benchmark(&url, tag, args.n, Duration::from_secs(10)))?;
            render_output(json_out, value, "benchmark".to_owned());
            Ok(())
        }
        RunTarget::Preflight(args) => {
            let url = require_localhost(&args.url)?;
            let tag = require_snapshot_tag(&args.tag)?;
            let value = block_on(forkd::preflight(&url, tag, args.require_vm))?;
            let text = if json_out {
                String::new()
            } else {
                forkd::preflight_text(&value)
            };
            render_output(json_out, value, text);
            Ok(())
        }
    }
}

fn doctor(json_out: bool) -> Result<(), CliError> {
    let caps = host::detect();
    let tools = tool::tool_matrix();
    let profiles = image_build::profile_summary();
    if json_out {
        let platform = match caps.kind {
            tool::HostKind::Linux => "linux",
            tool::HostKind::Wsl => "wsl",
            tool::HostKind::Windows => "windows",
        };
        let value = json!({
            "ok": true,
            "platform": platform,
            "arch": caps.arch,
            "kvm": caps.kvm,
            "tools": tools,
            "transport": ["vsock", "virtio-vsock"],
            "protocol": ["rfb1", "zbrt"],
            "capabilities": ["execute"],
            "profiles": profiles,
        });
        render_output(json_out, value, String::new());
    } else {
        let value = host::capability_text(&caps, &tools);
        println!("{value}");
        println!(
            "profiles: {}",
            profiles
                .as_array()
                .map(|a| a
                    .iter()
                    .map(|p| p["profile"].as_str().unwrap_or("?").to_owned())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Run an async command on a current-thread runtime.
///
/// Runtime construction is fallible; returning a CLI error keeps the binary
/// from panicking with exit 101 when the host cannot create its executor.
fn block_on<F, T>(future: F) -> Result<T, CliError>
where
    F: std::future::Future<Output = Result<T, CliError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| crate::cli::error::external(format!("build async runtime: {error}")))?;
    runtime.block_on(future)
}
