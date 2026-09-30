// `rfb-cli forkd backend-up`: one command that brings a local forkd backend
// up from its moving parts — pid1 binaries (or a ready rootfs), the
// controller process, the shared TAP, and a ready snapshot. Extracted from
// the sample app's bootstrap so SDK users only need this command and then
// plain HTTP.

use crate::cli::error::{external, validation, CliError};
use crate::cli::image_build::{build_rootfs, RootfsOptions};
use crate::cli::localhost::require_localhost;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// How long one controller start or snapshot build may take before
/// `backend-up` gives up (snapshot builds boot a parent VM: tens of seconds
/// cold, minutes on slow storage).
const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// Second-probe timeout for a controller that missed the first health check:
/// declaring a controller dead leads to `kill_leftover_firecrackers`, so a
/// slow-but-alive controller gets one retry with a longer timeout before
/// backend-up declares death and sweeps.
const CONTROLLER_SLOW_RETRY_TIMEOUT: Duration = Duration::from_secs(60);

fn controller_client(url: &str) -> Result<crate::controller::ForkdClient, CliError> {
    crate::controller::ForkdClient::new(url.to_owned(), None, Duration::from_secs(10))
        .map_err(|error| external(format!("controller client: {error}")))
}

/// The current effective uid via `id -u` (portable across unix; the CLI
/// already shells out for ip/pkill). `None` when `id` cannot be run.
fn current_uid() -> Option<String> {
    let uid = Command::new("id").arg("-u").output().ok()?;
    Some(String::from_utf8_lossy(&uid.stdout).trim().to_owned())
}

/// Root check via `id -u` (portable across unix; the CLI already shells out
/// for ip/pkill).
fn require_root() -> Result<(), CliError> {
    let uid = current_uid().ok_or_else(|| external("id: failed to run"))?;
    if uid != "0" {
        return Err(validation(
            "backend-up needs root: it manages the TAP device and Firecracker",
        ));
    }
    Ok(())
}

/// Kill leftover Firecracker processes: a killed VM leaves a zombie holding
/// the shared TAP, and the next create fails with "Resource busy" until it
/// is gone. A machine-global `pkill -9 -x firecracker` would also kill
/// unrelated users' VMs (and a slow-controller blip would become a mass
/// kill), so the sweep is scoped: first to the firecracker processes whose
/// command line references THIS backend's state dir (`pgrep -f`, verified
/// down to the process name so the controller's own `--state` argument never
/// matches), then — only when that sweep is unavailable or finds nothing —
/// to an exact-name match (`-x`, not a `-f` command line substring) restricted
/// to the current effective user, which still never touches other users'
/// VMs. Best-effort: pkill/pgrep may be absent.
fn kill_leftover_firecrackers(state_dir: &Path) {
    let needle = state_dir.to_string_lossy().into_owned();
    if let Ok(output) = Command::new("pgrep").arg("-f").arg(&needle).output() {
        if output.status.success() {
            let mut matched = false;
            for pid in String::from_utf8_lossy(&output.stdout).split_whitespace() {
                // pgrep -f matches the full command line, which also matches
                // this backend's own controller (`--state <dir>/state.json`);
                // verify the process name before killing.
                let is_firecracker = Command::new("ps")
                    .args(["-p", pid, "-o", "comm="])
                    .output()
                    .map(|out| {
                        out.status.success()
                            && String::from_utf8_lossy(&out.stdout).trim() == "firecracker"
                    })
                    .unwrap_or(false);
                if is_firecracker {
                    matched = true;
                    let _ = Command::new("kill").args(["-9", pid]).status();
                }
            }
            if matched {
                return;
            }
        }
    }
    // Fallback sweep: same exact-name match as before, restricted to the
    // current effective user. Without a usable uid the kill is skipped
    // entirely — better one busy TAP than killing unknown processes.
    if let Some(uid) = current_uid() {
        let _ = Command::new("pkill")
            .args(["-9", "-x", "-u", &uid, "firecracker"])
            .status();
    }
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

/// [`clear_stale_sandboxes`], gated on this invocation having spawned the
/// controller. A controller that was already running owns live sandboxes
/// whose teardown is not this command's decision — re-running backend-up
/// must converge instead of slaughtering a healthy stack's VMs (same
/// contract as the bring-up logic that never touches a reachable
/// controller).
async fn clear_stale_sandboxes_if_spawned(
    client: &crate::controller::ForkdClient,
    controller_started: bool,
) {
    if !controller_started {
        return;
    }
    clear_stale_sandboxes(client).await;
}

/// Test hook (same `#[doc(hidden)]` pattern as the ZeroBootSession test
/// accessors): run the gated stale-sandbox sweep against a client built for
/// `base_url`, so the no-clear-on-healthy-controller contract is assertable
/// from a mock controller without a root/KVM backend-up run.
///
/// # Errors
///
/// Returns `Err` when the controller client cannot be built.
#[doc(hidden)]
pub async fn clear_stale_sandboxes_for_test(
    base_url: &str,
    controller_started: bool,
) -> Result<(), CliError> {
    let client = controller_client(base_url)?;
    clear_stale_sandboxes_if_spawned(&client, controller_started).await;
    Ok(())
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

/// Whether a controller already answers `GET /v1/snapshots` on `url`. The
/// first probe uses the standard client timeout; a controller that misses it
/// gets ONE retry with a longer timeout before backend-up declares it dead —
/// declaring death leads to `kill_leftover_firecrackers` plus a replacement
/// controller spawn, neither of which may fire against a slow-but-alive
/// controller that is merely over its health-check window.
async fn controller_reachable(client: &crate::controller::ForkdClient, url: &str) -> bool {
    if client.list_snapshots().await.is_ok() {
        return true;
    }
    let Ok(retry) =
        crate::controller::ForkdClient::new(url.to_owned(), None, CONTROLLER_SLOW_RETRY_TIMEOUT)
    else {
        return false;
    };
    retry.list_snapshots().await.is_ok()
}

/// Loopback gate for the controller bind target, via the shared
/// [`require_localhost`] single source: `127.0.0.1:PORT` and `localhost:PORT`
/// hosts are both accepted (the same vocabulary the zeroboot up path uses),
/// and the validated base URL is returned for the controller client.
///
/// # Errors
///
/// Returns `Err` when the bind target is not loopback.
pub fn require_loopback_bind(bind: &str) -> Result<String, CliError> {
    require_localhost(&format!("http://{bind}"))
}

/// Build the delegated `forkd snapshot-create` arguments for the backend's
/// own snapshot: the controller URL, the resolved rootfs, and — passed
/// through, never dropped — the caller's `--require-provenance` gate.
pub fn snapshot_create_args_for_backend(
    url: &str,
    args: &crate::cli::commands::ForkdBackendUpArgs,
    rootfs: &Path,
) -> crate::cli::commands::ForkdSnapshotCreateArgs {
    super::super::commands::ForkdSnapshotCreateArgs {
        url: super::super::commands::ForkdUrlArgs {
            url: url.to_owned(),
        },
        tag: args.tag.clone(),
        kernel: Some(args.kernel.clone()),
        rootfs: Some(rootfs.to_path_buf()),
        tap: Some(args.tap.clone()),
        forkd_bin: None,
        rootfs_copy: None,
        mem_size_mib: None,
        boot_wait_secs: 10,
        require_provenance: args.require_provenance,
    }
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
    //
    // The daemon itself refuses unauthenticated non-loopback binds; fail
    // here with the actionable message instead of after the TAP churn.
    let url = require_loopback_bind(&args.bind)?;
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

    let client = controller_client(&url)?;
    let mut controller_started = false;
    if !controller_reachable(&client, &url).await {
        // First bring-up (or the previous stack died): leftovers from the dead
        // run still hold the shared TAP, so clear them before spawning a new
        // controller. A REACHABLE controller is never touched — re-run
        // convergence must not slaughter the live VMs it owns.
        kill_leftover_firecrackers(&state_dir);
        let snapshot_root = default_snapshot_root()?;
        spawn_controller(&controller_bin, &state_dir, &snapshot_root, &args.bind)?;
        controller_started = true;
    }
    wait_controller(&client, &url).await.map_err(|error| {
        let log = state_dir.join("controller.log");
        external(format!("{error:?}; controller log: {}", log.display()))
    })?;

    clear_stale_sandboxes_if_spawned(&client, controller_started).await;

    if !client.snapshot_ready(&args.tag).await.unwrap_or(false) {
        let create_args = snapshot_create_args_for_backend(&url, args, &rootfs);
        let _ = super::snapshot_create(&create_args)?;
        wait_snapshot(&client, &args.tag).await?;
    }
    // Fail closed on demand: once the snapshot is ready, re-check its info
    // record when --require-provenance was set. The delegated create above
    // already gates its own result; this re-check also covers the reuse path
    // (an already-ready snapshot is never re-created, but its provenance
    // must still verify before backend-up reports success).
    if args.require_provenance {
        let info_args = super::super::commands::ForkdSnapshotInfoArgs {
            url: super::super::commands::ForkdUrlArgs { url: url.clone() },
            tag: args.tag.clone(),
            forkd_bin: None,
            require_provenance: true,
        };
        let _ = super::snapshot_info(&info_args)?;
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
