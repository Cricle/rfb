//! Complex business workload across multiple sandboxes: rounds of guest RPC,
//! session reuse, data handoff digests, and leak-free destroy.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::forkd::preflight;
use crate::cli::forkd::sandbox::{
    create_sandbox, destroy_sandbox, guest_call, list_sandboxes, wait_for_guest_ready,
    GUEST_READY_DEADLINE,
};
use crate::cli::image_build::hex_lower;
use crate::forkd_guest::ForkdGuestClient;
use serde_json::{json, Value};

/// Run the workload gate: multiple sandboxes, rounds of guest RPC, session
/// reuse, and leak-free destroy. Returns sanitized aggregate counters only.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn workload(
    url: &str,
    tag: &str,
    sandboxes: usize,
    rounds: usize,
    reuse_execs: usize,
) -> Result<Value, CliError> {
    let mut op_count: std::collections::BTreeMap<String, usize> = Default::default();
    macro_rules! count {
        ($op:expr) => {
            *op_count.entry($op.to_owned()).or_default() += 1
        };
    }

    preflight(url, tag, true).await?;
    let mut created: Vec<String> = Vec::new();
    let mut failed = false;

    for si in 0..sandboxes {
        let result = async {
            let sandbox = create_sandbox(url, tag, 1, Some(32))
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| validation("create returned no sandbox"))?;
            let sid = sandbox.id.clone();
            let address = sandbox.guest_addr.clone();
            created.push(sid.clone());
            wait_for_guest_ready(&address, GUEST_READY_DEADLINE).await?;
            count!("sandbox_create");

            for ri in 0..rounds {
                let ping = ForkdGuestClient::new(address.clone()).ping().await;
                if ping.map(|v| v.get("pong").is_some()).unwrap_or(false) { count!("ping"); }
                else { return Err(validation("ping failed")); }

                let exec = ForkdGuestClient::new(address.clone())
                    .exec_in("/workspace", vec!["/bin/true".into()], 9)
                    .await;
                if exec.map(|v| v.get("exit_code").and_then(Value::as_i64) == Some(0)).unwrap_or(false) {
                    count!("exec");
                } else {
                    return Err(validation("exec failed"));
                }

                let fname = format!("orders-2026-08-30-{si}-{ri}.csv");
                let row = format!("txn-{si}-{ri},pending,value-{}", si * 100 + ri);
                let content = format!("id,status,val\n{row}\n");
                let write = guest_call(
                    &address,
                    json!({"action": "write", "path": format!("/workspace/{fname}"), "data": content.as_bytes()}),
                    false,
                )
                .await;
                if write.ok().and_then(|v| v.get("bytes_written").and_then(Value::as_u64).map(|b| b as usize)) == Some(content.len()) {
                    count!("write");
                } else {
                    return Err(validation("write failed"));
                }

                let read = guest_call(
                    &address,
                    json!({"action": "read", "path": format!("/workspace/{fname}"), "max_bytes": 4096}),
                    false,
                )
                .await;
                if read.map(|v| {
                    v.get("data").and_then(Value::as_array).map(|a| {
                        let bytes: Vec<u8> = a.iter().filter_map(Value::as_u64).map(|b| b as u8).collect();
                        let s = String::from_utf8_lossy(&bytes);
                        s.contains("txn-") && s.ends_with('\n')
                    }).unwrap_or(false)
                }).unwrap_or(false) {
                    count!("read");
                } else {
                    return Err(validation("read mismatch"));
                }

                let find = guest_call(
                    &address,
                    json!({"action": "find", "path": "/workspace", "pattern": ".csv", "max_results": 100}),
                    false,
                )
                .await;
                if find.map(|v| v.get("matches").and_then(Value::as_array).map(|m| m.iter().any(|x| x.as_str().map(|s| s.contains(&fname)).unwrap_or(false))).unwrap_or(false)).unwrap_or(false) {
                    count!("find");
                } else {
                    return Err(validation("find failed"));
                }

                let grep = guest_call(
                    &address,
                    json!({"action": "grep", "path": "/workspace", "pattern": "pending", "max_results": 100, "max_bytes": 8192}),
                    false,
                )
                .await;
                if grep.map(|v| v.get("matches").and_then(Value::as_array).map(|m| !m.is_empty()).unwrap_or(false)).unwrap_or(false) {
                    count!("grep");
                } else {
                    return Err(validation("grep failed"));
                }

                let stream = async {
                    let mut s = ForkdGuestClient::new(address.clone())
                        .stream(vec!["/bin/echo".into(), format!("round-{ri}")], None, Some(false), None)
                        .await
                        .map_err(|e| external(e.to_string()))?;
                    loop {
                        match s.next_event().await.map_err(|e| external(e.to_string()))? {
                            Some(v) => { if v.get("exit_code").is_some() { break; } }
                            None => return Err(validation("stream closed")),
                        }
                    }
                    Ok::<_, CliError>(())
                }
                .await;
                if stream.is_ok() { count!("stream"); } else { return Err(validation("stream failed")); }
            }

            // Session reuse pressure via persistent connection.
            for _ in 0..reuse_execs {
                let exec = ForkdGuestClient::new(address.clone())
                    .exec_in("/workspace", vec!["/bin/true".into()], 9)
                    .await;
                if exec.map(|v| v.get("exit_code").and_then(Value::as_i64) == Some(0)).unwrap_or(false) {
                    count!("reuse_exec");
                } else {
                    return Err(validation("reuse exec failed"));
                }
            }

            // Summary + handoff digest.
            let summary = json!({"sandbox": sid, "rounds": rounds, "ops": op_count});
            let digest = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(serde_json::to_vec(&summary).map_err(|e| io(e.to_string()))?);
                hex_lower(h.finalize())
            };
            let write = guest_call(
                &address,
                json!({"action": "write", "path": "/workspace/summary.json", "data": json!({"digest": digest}).to_string().as_bytes()}),
                false,
            )
            .await;
            if write.ok().and_then(|v| v.get("bytes_written").and_then(Value::as_u64)).unwrap_or(0) > 0 {
                count!("summary_write");
            } else {
                return Err(validation("summary write failed"));
            }
            let read = guest_call(
                &address,
                json!({"action": "read", "path": "/workspace/summary.json", "max_bytes": 4096}),
                false,
            )
            .await;
            let ok = read.map(|v| {
                v.get("data").and_then(Value::as_array).map(|a| {
                    let bytes: Vec<u8> = a.iter().filter_map(Value::as_u64).map(|b| b as u8).collect();
                    serde_json::from_slice::<Value>(&bytes).ok()
                        .and_then(|x| x.get("digest").and_then(Value::as_str).map(|s| s == digest))
                        .unwrap_or(false)
                }).unwrap_or(false)
            }).unwrap_or(false);
            if ok { count!("handoff_verify"); } else { return Err(validation("handoff digest mismatch")); }

            destroy_sandbox(url, &sid).await?;
            count!("sandbox_destroy");
            Ok::<(), CliError>(())
        }
        .await;
        match result {
            Ok(()) => {}
            Err(_) => {
                failed = true;
                // Attempt to destroy any remaining sandboxes from this invocation.
                if let Ok(remaining) = list_sandboxes(url).await {
                    for s in remaining {
                        if created.contains(&s.id) {
                            let _ = destroy_sandbox(url, &s.id).await;
                        }
                    }
                }
            }
        }
    }

    // Orphan check. A list failure must fail the gate: reporting PASS without
    // orphans evidence would claim the documented orphan=0 result blindly.
    let orphans: Vec<String> = match list_sandboxes(url).await {
        Ok(remaining) => remaining
            .iter()
            .filter(|s| created.contains(&s.id))
            .map(|s| s.id.clone())
            .collect(),
        Err(error) => {
            return Err(external(format!(
                "WORKLOAD_RFB FAIL orphan check failed: {}",
                error.message
            )));
        }
    };
    let total_ops: usize = op_count.values().sum();
    if failed || !orphans.is_empty() {
        return Err(external(format!(
            "WORKLOAD_RFB FAIL orphans={orphans:?} op_count={op_count:?}"
        )));
    }
    Ok(json!({
        "result": "PASS",
        "sandboxes_created": op_count.get("sandbox_create").copied().unwrap_or(0),
        "sandboxes_destroyed": op_count.get("sandbox_destroy").copied().unwrap_or(0),
        "rounds_per_sandbox": rounds,
        "reuse_execs_per_sandbox": reuse_execs,
        "total_ops": total_ops,
        "ops": op_count,
        "orphans": orphans,
        "payload": "suppressed",
        "snapshot_deletion": "none",
    }))
}
