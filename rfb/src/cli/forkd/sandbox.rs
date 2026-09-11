//! Sandbox lifecycle helpers against the forkd controller and the guest
//! NDJSON RPC: create/destroy/list/ping plus one-shot guest calls.

use crate::cli::error::{external, validation, CliError};
use crate::controller::{CreateSandboxRequest, ForkdClient, SandboxInfo};
use crate::forkd_guest::ForkdGuestClient;
use serde_json::Value;
use std::time::{Duration, Instant};

/// Single shared budget for waiting on guest readiness. Acceptance, workload,
/// and benchmark all use the same total deadline so guest boot behavior is
/// measured consistently across gates.
pub const GUEST_READY_DEADLINE: Duration = Duration::from_secs(30);

/// Wait until the guest agent accepts a ping within a bounded total deadline.
///
/// The address is validated as a real `SocketAddr` before any traffic is sent;
/// forkd must always report a usable host:port for the guest listener.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn wait_for_guest_ready(address: &str, deadline: Duration) -> Result<Value, CliError> {
    if address.trim().is_empty() {
        return Err(validation("sandbox returned an empty guest address"));
    }
    ForkdClient::validate_guest_address(address)
        .map_err(|error| validation(format!("invalid guest address {address:?}: {error}")))?;
    let client = ForkdGuestClient::new(address.to_owned());
    let started = Instant::now();
    let mut delay = Duration::from_millis(100);
    loop {
        match client.ping().await {
            Ok(value) if value.get("pong").is_some() => return Ok(value),
            Ok(_) => {}
            Err(_) => {}
        }
        if started.elapsed() >= deadline {
            return Err(external(format!(
                "guest agent not ready at {address} within {}s",
                deadline.as_secs()
            )));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(1000));
    }
}

/// Create `n` sandboxes from a bootable snapshot. Returns the parsed response.
///
/// `per_child_netns` is required for concurrent sandboxes: the default shared
/// host tap admits only one live sandbox at a time and the controller rejects
/// further creates with 503 until it is deleted.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn create_sandbox(
    url: &str,
    tag: &str,
    n: usize,
    memory_limit_mib: Option<u64>,
    per_child_netns: bool,
) -> Result<Vec<SandboxInfo>, CliError> {
    let client = ForkdClient::new(url.to_owned(), None, Duration::from_secs(30))
        .map_err(|error| validation(error.to_string()))?;
    let request = CreateSandboxRequest {
        snapshot_tag: tag,
        n,
        per_child_netns,
        memory_limit_mib,
        prewarm: false,
        live_fork: false,
        hugepages: false,
    };
    client
        .create_sandbox(&request)
        .await
        .map_err(|error| external(error.to_string()))
}

/// Destroy a sandbox, reconciling a transport anomaly by confirming against the
/// live sandbox list (some controllers close an empty 204 before curl/reply).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn destroy_sandbox(url: &str, sandbox_id: &str) -> Result<(), CliError> {
    let client = ForkdClient::new(url.to_owned(), None, Duration::from_secs(30))
        .map_err(|error| validation(error.to_string()))?;
    match client.delete_sandbox(sandbox_id).await {
        Ok(()) => Ok(()),
        Err(error) => {
            // Confirm the sandbox really is gone before surfacing the error.
            if let Ok(remaining) = list_sandboxes(url).await {
                if !remaining.iter().any(|s| s.id == sandbox_id) {
                    return Ok(());
                }
            }
            Err(external(error.to_string()))
        }
    }
}

/// List live sandboxes (used for destroy reconciliation and orphan checks).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn list_sandboxes(url: &str) -> Result<Vec<SandboxInfo>, CliError> {
    let client = ForkdClient::new(url.to_owned(), None, Duration::from_secs(30))
        .map_err(|error| validation(error.to_string()))?;
    client
        .list_sandboxes()
        .await
        .map_err(|error| external(error.to_string()))
}

/// Ping a sandbox via the controller.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn ping_sandbox(url: &str, sandbox_id: &str) -> Result<Value, CliError> {
    let client = ForkdClient::new(url.to_owned(), None, Duration::from_secs(30))
        .map_err(|error| validation(error.to_string()))?;
    client
        .ping(sandbox_id)
        .await
        .map_err(|error| external(error.to_string()))
}

/// Execute a single NDJSON guest RPC against the guest address returned by
/// forkd. `terminal` stops reading at the `exit_code` frame (exec/stream).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn guest_call(address: &str, action: Value, terminal: bool) -> Result<Value, CliError> {
    let client = ForkdGuestClient::new(address.to_owned());
    if terminal {
        let value = client
            .request(action)
            .await
            .map_err(|error| external(error.to_string()))?;
        value
            .into_iter()
            .last()
            .ok_or_else(|| validation("guest returned no terminal response"))
    } else {
        let value = client
            .request(action)
            .await
            .map_err(|error| external(error.to_string()))?;
        value
            .into_iter()
            .last()
            .ok_or_else(|| validation("guest returned no response"))
    }
}
