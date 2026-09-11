//! Full forkd acceptance gate: create a sandbox, exercise ping/stream/exec and
//! the structured filesystem RPCs, validate negative paths, then destroy.

use crate::cli::error::{external, no_vm, validation, CliError};
use crate::cli::forkd::preflight::snapshot_ready;
use crate::cli::forkd::sandbox::{
    create_sandbox, destroy_sandbox, guest_call, ping_sandbox, wait_for_guest_ready,
    GUEST_READY_DEADLINE,
};
use crate::controller::ForkdClient;
use crate::forkd_guest::ForkdGuestClient;
use serde_json::{json, Value};

/// Full acceptance gate: create a sandbox, exercise ping/stream/exec plus the
/// structured filesystem RPCs and negative path validation, then destroy.
/// Provenance requirements are validated by the dispatcher
/// (`--snapshot-binding` + `--artifact-manifest`) before this runs.
pub async fn acceptance(url: &str, tag: &str, require_vm: bool) -> Result<Value, CliError> {
    // Preflight is part of the gate: without a bootable snapshot the gate is a
    // hard BLOCKED when `require_vm` is set. Missing VM prerequisites carry the
    // documented exit-12 contract (`no_vm`), not the validation exit code.
    let client = ForkdClient::new(url.to_owned(), None, GUEST_READY_DEADLINE)
        .map_err(|error| validation(error.to_string()))?;
    let snapshots = match client.list_snapshots().await {
        Ok(snapshots) => snapshots,
        Err(error) => {
            let message = format!("forkd unavailable: {error}");
            return Err(if require_vm {
                no_vm(message)
            } else {
                validation(message)
            });
        }
    };
    if !snapshot_ready(&snapshots, tag) {
        if require_vm {
            return Err(no_vm(format!(
                "bootable ready snapshot '{tag}' is unavailable"
            )));
        }
        return Ok(json!({"status": "skipped", "reason": "snapshot unavailable"}));
    }

    let sandbox = create_sandbox(url, tag, 1, Some(32))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| validation("sandbox create returned no sandbox"))?;
    let sid = sandbox.id.clone();
    let address = sandbox.guest_addr.clone();
    if let Err(error) = wait_for_guest_ready(&address, GUEST_READY_DEADLINE).await {
        let _ = destroy_sandbox(url, &sid).await;
        return Err(error);
    }

    let result: Result<Value, CliError> = async {
        let mut outcomes = Vec::new();
        let mut run = |name: &str, result: Result<Value, CliError>| {
        outcomes.push(json!({"name": name, "ok": result.is_ok()}));
        result
    };

    // Controller ping + guest ping.
    run(
        "controller_ping",
        ping_sandbox(url, &sid).await.map(|_| json!({})),
    )?;
    let ping = run(
        "guest_ping",
        guest_call(&address, json!({"action": "ping"}), false).await,
    )?;
    if ping.get("pong").is_none() {
        return Err(validation("guest ping did not return pong"));
    }

    // Stream with terminal frame collection.
    let stream_ok = async {
        let mut stream = ForkdGuestClient::new(address.clone())
            .stream(
                vec!["/bin/echo".into(), "rfb-cli-acceptance".into()],
                None,
                Some(false),
                None,
            )
            .await
            .map_err(|e| external(e.to_string()))?;
        loop {
            match stream
                .next_event()
                .await
                .map_err(|e| external(e.to_string()))?
            {
                Some(value) => {
                    if let Some(code) = value.get("exit_code") {
                        if code.as_i64() != Some(0) {
                            return Err(validation("guest stream exit code != 0"));
                        }
                        break;
                    }
                }
                None => return Err(validation("guest stream closed before exit")),
            }
        }
        Ok::<_, CliError>(json!({}))
    }
    .await;
    run("guest_stream", stream_ok)?;

    // Exec in the opaque workspace.
    let exec = run(
        "guest_exec",
        guest_call(
            &address,
            json!({"action": "exec", "cwd": "/workspace", "args": ["/bin/true"], "timeout": 10}),
            true,
        )
        .await,
    )?;
    if exec.get("exit_code").and_then(Value::as_i64) != Some(0) {
        return Err(validation("guest exec failed"));
    }

    // Structured filesystem RPCs against the workspace.
    let marker = "/workspace/.rfb-cli-acceptance";
    let payload = b"rfb-cli-acceptance\n";
    let listing = run(
        "guest_ls",
        guest_call(
            &address,
            json!({"action": "ls", "path": "/workspace", "max_results": 100}),
            false,
        )
        .await,
    )?;
    if listing.get("entries").and_then(Value::as_array).is_none() {
        return Err(validation("guest ls failed"));
    }
    let write = run(
        "guest_write",
        guest_call(
            &address,
            json!({"action": "write", "path": marker, "data": payload}),
            false,
        )
        .await,
    )?;
    if write.get("bytes_written").and_then(Value::as_u64) != Some(payload.len() as u64) {
        return Err(validation("guest write failed"));
    }
    let read = run(
        "guest_read",
        guest_call(
            &address,
            json!({"action": "read", "path": marker, "max_bytes": 4096}),
            false,
        )
        .await,
    )?;
    if let Some(data) = read.get("data").and_then(Value::as_array) {
        let bytes: Vec<u8> = data
            .iter()
            .filter_map(Value::as_u64)
            .map(|b| b as u8)
            .collect();
        if bytes != payload {
            return Err(validation("guest read mismatch"));
        }
    } else {
        return Err(validation("guest read returned no data"));
    }
    let find = run(
        "guest_find",
        guest_call(
            &address,
            json!({"action": "find", "path": "/workspace", "pattern": ".rfb-cli-acceptance", "max_results": 100}),
            false,
        )
        .await,
    )?;
    if !find
        .get("matches")
        .and_then(Value::as_array)
        .map(|m| {
            m.iter().any(|v| {
                v.as_str()
                    .map(|s| s.contains(".rfb-cli-acceptance"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
    {
        return Err(validation("guest find failed"));
    }
    run(
        "guest_grep",
        guest_call(
            &address,
            json!({"action": "grep", "path": "/workspace", "pattern": "rfb-cli-acceptance", "max_results": 100, "max_bytes": 4096}),
            false,
        )
        .await,
    )?;

    // Negative cases must be rejected by the guest contract, not clamped.
    let mut negatives = Vec::new();
    for (name, action) in [
        (
            "ls_escape",
            json!({"action": "ls", "path": "../escape", "max_results": 100}),
        ),
        (
            "ls_zero_limit",
            json!({"action": "ls", "path": ".", "max_results": 0}),
        ),
        (
            "grep_too_many_bytes",
            json!({"action": "grep", "path": ".", "pattern": "x", "max_results": 100, "max_bytes": 51201}),
        ),
    ] {
        let result = guest_call(&address, action, false).await;
        negatives.push(json!({"name": name, "rejected": result.is_err()}));
        if result.is_ok() {
            return Err(validation(format!(
                "negative case unexpectedly succeeded: {name}"
            )));
        }
    }

        Ok(json!({
            "status": "passed",
            "sandbox_id": sid,
            "guest_addr": address,
            "checks": outcomes,
            "negative_cases": negatives,
            "payload": "suppressed",
        }))
    }
    .await;

    // Every post-create path, including a mid-gate error, must release this
    // invocation's sandbox. Preserve the original operation error if cleanup
    // itself also fails.
    let cleanup_result = destroy_sandbox(url, &sid).await;
    match (result, cleanup_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}
