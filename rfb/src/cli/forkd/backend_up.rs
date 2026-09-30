// `rfb-cli forkd backend-up`: one command that brings a local forkd backend
// up from its moving parts — pid1 binaries (or a ready rootfs), the
// controller process, the shared TAP, and a ready snapshot. Extracted from
// the sample app's bootstrap so SDK users only need this command and then
// plain HTTP.

use crate::cli::error::{external, validation, CliError};
use crate::cli::image_build::{build_rootfs, RootfsOptions};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// How long one controller start or snapshot build may take before
/// `backend-up` gives up (snapshot builds boot a parent VM: tens of seconds
/// cold, minutes on slow storage).
const READY_TIMEOUT: Duration = Duration::from_secs(300);

fn controller_client(url: &str) -> Result<crate::controller::ForkdClient, CliError> {
    crate::controller::ForkdClient::new(url.to_owned(), None, Duration::from_secs(10))
        .map_err(|error| external(format!("controller client: {error}")))
}

/// Root check via `id -u` (portable across unix; the CLI already shells out
/// for ip/pkill).
fn require_root() -> Result<(), CliError> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| external(format!("id: {error}")))?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
    if uid != "0" {
        return Err(validation(
            "backend-up needs root: it manages the TAP device and Firecracker",
        ));
    }
    Ok(())
}

/// Kill leftover Firecracker processes: a killed VM leaves a zombie holding
/// the shared TAP, and the next create fails with "Resource busy" until it
/// is gone. Scoped to an exact process-name match (`-x`, not a `-f` command
/// line substring) so unrelated processes whose arguments merely mention
/// firecracker are never hit. Best-effort: pkill may be absent.
fn kill_leftover_firecrackers() {
    let _ = Command::new("pkill")
        .args(["-9", "-x", "firecracker"])
        .status();
}

/// Ensure the shared TAP exists and is UP. The address is the standard
/// 10.42.0.1/24 segment (real-machine-runbook.md).
fn ensure_tap(tap: &str) -> Result<(), CliError> {
    let step = |mut command: Command| -> Result<(), CliError> {
        command
            .status()
            .map_err(|error| external(format!("{command:?}: {error}")))?
            .success()
            .then_some(())
            .ok_or_else(|| external(format!("{command:?} failed")))
    };
    let mut show = Command::new("ip");
    show.args(["link", "show", tap])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let exists = show.status().is_ok_and(|status| status.success());
    if !exists {
        let mut add = Command::new("ip");
        add.args(["tuntap", "add", "dev", tap, "mode", "tap"]);
        step(add)?;
        let mut addr = Command::new("ip");
        addr.args(["addr", "add", "10.42.0.1/24", "dev", tap]);
        step(addr)?;
    } else {
        // Idempotent convergence: a TAP left over from a run that crashed
        // between `tuntap add` and `addr add` (or whose address was flushed)
        // must get its address back — otherwise guest networking is dead and
        // the failure surfaces far away, at create/snapshot time.
        let has_addr = Command::new("ip")
            .args(["addr", "show", "dev", tap])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains("10.42.0.1"))
            .unwrap_or(false);
        if !has_addr {
            let mut addr = Command::new("ip");
            addr.args(["addr", "add", "10.42.0.1/24", "dev", tap]);
            step(addr)?;
        }
    }
    let mut up = Command::new("ip");
    up.args(["link", "set", tap, "up"]);
    step(up)?;
    Ok(())
}

/// Delete every sandbox the controller still knows about: shared-tap mode
/// admits only one live sandbox, and a stale one makes the next create 503.
async fn clear_stale_sandboxes(client: &crate::controller::ForkdClient) {
    if let Ok(sandboxes) = client.list_sandboxes().await {
        for sandbox in sandboxes {
            let _ = client.delete_sandbox(&sandbox.id).await;
        }
    }
}

/// Spawn the controller detached from this CLI's process group (a short-lived
/// CLI must not take the daemon down) and return the child pid.
fn spawn_controller(
    bin: &Path,
    state_dir: &Path,
    snapshot_root: &Path,
    bind: &str,
) -> Result<u32, CliError> {
    let log_path = state_dir.join("controller.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| external(format!("open controller log: {error}")))?;
    let err = log
        .try_clone()
        .map_err(|error| external(format!("clone controller log fd: {error}")))?;
    let mut command = Command::new(bin);
    command
        .args([
            "serve",
            "--state",
            &state_dir.join("state.json").to_string_lossy(),
            "--audit-log",
            &state_dir.join("audit.log").to_string_lossy(),
            // Must match where `rfb-cli forkd snapshot-create` stores tags by
            // default, or the controller lists an empty set.
            "--snapshot-root",
            &snapshot_root.to_string_lossy(),
            "--bind",
            bind,
        ])
        .stdout(log)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|error| external(format!("spawn controller: {error}")))?;
    Ok(child.id())
}

/// Build the rootfs from pid1 binaries (rfb-runtime + rfb-busybox beside it).
/// This is the no-cargo path: `build_rootfs` only assembles the ext4.
fn build_rootfs_from_pid1(
    pid1_dir: &Path,
    state_dir: &Path,
    tag: &str,
    with_python: bool,
    with_lua: bool,
    allow_dynamic: bool,
) -> Result<PathBuf, CliError> {
    std::fs::create_dir_all(state_dir)
        .map_err(|error| external(format!("create state dir: {error}")))?;
    let runtime = pid1_dir.join("rfb-runtime");
    if !runtime.is_file() {
        return Err(validation(format!(
            "pid1 dir must contain rfb-runtime: {}",
            runtime.display()
        )));
    }
    let busybox = pid1_dir.join("rfb-busybox");
    if !busybox.is_file() {
        return Err(validation(format!(
            "pid1 dir must contain rfb-busybox (guest /bin/sh): {}",
            busybox.display()
        )));
    }
    let output = state_dir.join(format!("{tag}.ext4"));
    build_rootfs(
        &runtime,
        &output,
        None,
        "forkd-agent",
        allow_dynamic,
        true,
        &RootfsOptions {
            with_python,
            with_lua,
            py_site_dir: None,
            lua_lib_dir: None,
            extra_files: Vec::new(),
        },
    )?;
    Ok(output)
}

/// Wait until the controller answers `GET /v1/snapshots`.
async fn wait_controller(
    client: &crate::controller::ForkdClient,
    url: &str,
) -> Result<(), CliError> {
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            if client.list_snapshots().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .map_err(|_| {
        external(format!(
            "controller {url} 未在 {}s 内就绪（日志见 state dir）",
            READY_TIMEOUT.as_secs()
        ))
    })
}

/// Wait until the snapshot reports ready+bootable.
async fn wait_snapshot(client: &crate::controller::ForkdClient, tag: &str) -> Result<(), CliError> {
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            if client.snapshot_ready(tag).await.unwrap_or(false) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .map_err(|_| {
        external(format!(
            "snapshot `{tag}` 未在 {}s 内就绪",
            READY_TIMEOUT.as_secs()
        ))
    })
}

/// The snapshot data directory `forkd snapshot` writes to by default
/// (`$XDG_DATA_HOME/forkd/snapshots`, else `$HOME/.local/share/forkd/snapshots`).
/// The controller's `--snapshot-root` must point here, or it lists nothing.
fn default_snapshot_root() -> Result<PathBuf, CliError> {
    crate::cli::forkd::forkd_snapshots_root().ok_or_else(|| {
        validation("neither XDG_DATA_HOME nor HOME is set; cannot resolve the snapshot root")
    })
}

/// `rfb-cli forkd backend-up` — bring everything up, idempotently:
/// pid1 → rootfs (or reuse a provided one), kill stale Firecrackers, TAP,
/// controller process, snapshot create/wait. Earlier stages stay up on later
/// failures; re-runs converge instead of tearing down.
///
/// # Errors
///
/// Returns `Err` when any stage fails.
pub async fn backend_up(
    args: &crate::cli::commands::ForkdBackendUpArgs,
) -> Result<Value, CliError> {
    // Pure argument validation first (permission-independent, so non-root
    // environments like CI exercise the same contract).
    if args.bind.split(':').next() != Some("127.0.0.1") {
        // The daemon itself refuses unauthenticated non-loopback binds; fail
        // here with the actionable message instead of after the TAP churn.
        return Err(validation(
            "bind must be loopback (the controller refuses 0.0.0.0 without a token)",
        ));
    }
    let state_dir = args.state_dir.clone().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/share/rfb/backend")
    });
    // The rootfs build is pure file work (no TAP/VM): it runs before the
    // root check so non-root environments (CI) exercise the same contract.
    let (rootfs, rootfs_source) = crate::cli::image_build::resolve_rootfs_source(
        args.rootfs.as_deref(),
        args.pid1_dir.as_deref(),
        |dir| {
            build_rootfs_from_pid1(
                dir,
                &state_dir,
                &args.tag,
                args.with_python,
                args.with_lua,
                args.allow_dynamic,
            )
        },
    )?;
    std::fs::create_dir_all(&state_dir)
        .map_err(|error| external(format!("create state dir: {error}")))?;
    crate::cli::image_build::require_readable(&rootfs, "rootfs")?;
    crate::cli::image_build::require_readable(&args.kernel, "kernel")?;
    require_root()?;
    let controller_bin = PathBuf::from("forkd-controller");
    let found = Command::new("which")
        .arg(&controller_bin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| external(format!("which: {error}")))?;
    if !found.success() {
        return Err(validation(
            "forkd-controller not on PATH (install from resx/forkd/)",
        ));
    }

    ensure_tap(&args.tap)?;

    let url = format!("http://{}", args.bind);
    let client = controller_client(&url)?;
    let mut controller_started = false;
    if client.list_snapshots().await.is_err() {
        // First bring-up (or the previous stack died): leftovers from the dead
        // run still hold the shared TAP, so clear them before spawning a new
        // controller. A REACHABLE controller is never touched — re-run
        // convergence must not slaughter the live VMs it owns.
        kill_leftover_firecrackers();
        let snapshot_root = default_snapshot_root()?;
        spawn_controller(&controller_bin, &state_dir, &snapshot_root, &args.bind)?;
        controller_started = true;
    }
    wait_controller(&client, &url).await.map_err(|error| {
        let log = state_dir.join("controller.log");
        external(format!("{error:?}; controller log: {}", log.display()))
    })?;

    clear_stale_sandboxes(&client).await;

    if !client.snapshot_ready(&args.tag).await.unwrap_or(false) {
        let create_args = super::super::commands::ForkdSnapshotCreateArgs {
            url: super::super::commands::ForkdUrlArgs { url: url.clone() },
            tag: args.tag.clone(),
            kernel: Some(args.kernel.clone()),
            rootfs: Some(rootfs.clone()),
            tap: Some(args.tap.clone()),
            forkd_bin: None,
            rootfs_copy: None,
            mem_size_mib: None,
            boot_wait_secs: 10,
            require_provenance: false,
        };
        let _ = super::snapshot_create(&create_args)?;
        wait_snapshot(&client, &args.tag).await?;
    }

    Ok(json!({
        "ok": true,
        "url": url,
        "tag": args.tag,
        "rootfs": rootfs.to_string_lossy(),
        "rootfs_source": rootfs_source,
        "controller_started": controller_started,
        "tap": args.tap,
        "state_dir": state_dir.to_string_lossy(),
    }))
}
