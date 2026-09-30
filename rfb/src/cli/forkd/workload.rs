//! Complex business workload across multiple sandboxes: rounds of guest RPC,
//! session reuse, data handoff digests, and leak-free destroy.

use crate::cli::error::{external, io, CliError};
use crate::cli::forkd::preflight;
use crate::cli::forkd::sandbox::{
    create_ready_sandbox, destroy_sandbox, guest_call, list_sandboxes, stream_to_exit,
};
use crate::cli::image_build::hex_lower;
use crate::cli::report::checked;
use crate::cli::report::count_op;
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

    preflight(url, tag, true).await?;
    let mut created: Vec<String> = Vec::new();
    let mut failed = false;

    for si in 0..sandboxes {
        let result = async {
            let sandbox = create_ready_sandbox(url, tag, "create returned no sandbox").await?;
            let sid = sandbox.id.clone();
            let address = sandbox.guest_addr.clone();
            created.push(sid.clone());
            count_op(&mut op_count, "sandbox_create");

            for ri in 0..rounds {
                let ping = ForkdGuestClient::new(address.clone()).ping().await;
                checked(
                    &mut op_count,
                    ping.map(|v| v.get("pong").is_some()).unwrap_or(false),
                    "ping",
                    "ping failed",
                )?;

                let exec = ForkdGuestClient::new(address.clone())
                    .exec_in("/workspace", vec!["/bin/true".into()], 9)
                    .await;
                checked(
                    &mut op_count,
                    exec.map(|v| v.get("exit_code").and_then(Value::as_i64) == Some(0))
                        .unwrap_or(false),
                    "exec",
                    "exec failed",
                )?;

                let fname = format!("orders-2026-08-30-{si}-{ri}.csv");
                let row = format!("txn-{si}-{ri},pending,value-{}", si * 100 + ri);
                let content = format!("id,status,val\n{row}\n");
                let write = guest_call(
                    &address,
                    json!({"action": "write", "path": format!("/workspace/{fname}"), "data": content.as_bytes()}),
                    false,
                )
                .await;
                checked(
                    &mut op_count,
                    write.ok().and_then(|v| v.get("bytes_written").and_then(Value::as_u64).map(|b| b as usize)) == Some(content.len()),
                    "write",
                    "write failed",
                )?;

                let read = guest_call(
                    &address,
                    json!({"action": "read", "path": format!("/workspace/{fname}"), "max_bytes": 4096}),
                    false,
                )
                .await;
                checked(
                    &mut op_count,
                    read.map(|v| {
                        v.get("data").and_then(Value::as_array).map(|a| {
                            let bytes: Vec<u8> = a.iter().filter_map(Value::as_u64).map(|b| b as u8).collect();
                            let s = String::from_utf8_lossy(&bytes);
                            s.contains("txn-") && s.ends_with('\n')
                        }).unwrap_or(false)
                    }).unwrap_or(false),
                    "read",
                    "read mismatch",
                )?;

                // find 语义是 glob 全名匹配（PROTOCOL.md §2.5a）：无通配符的
                // 模式是精确名匹配——写 `*.csv` 才命中刚写入的 orders-*.csv
                //（`.csv` 字面量只匹配名为 ".csv" 的文件，曾让真机 E2E 的
                // workload 门整体失败）。
                let find = guest_call(
                    &address,
                    json!({"action": "find", "path": "/workspace", "pattern": "*.csv", "max_results": 100}),
                    false,
                )
                .await;
                checked(
                    &mut op_count,
                    find.map(|v| v.get("matches").and_then(Value::as_array).map(|m| m.iter().any(|x| x.as_str().map(|s| s.contains(&fname)).unwrap_or(false))).unwrap_or(false)).unwrap_or(false),
                    "find",
                    "find failed",
                )?;

                let grep = guest_call(
                    &address,
                    json!({"action": "grep", "path": "/workspace", "pattern": "pending", "max_results": 100, "max_bytes": 8192}),
                    false,
                )
                .await;
                checked(
                    &mut op_count,
                    grep.map(|v| v.get("matches").and_then(Value::as_array).map(|m| !m.is_empty()).unwrap_or(false)).unwrap_or(false),
                    "grep",
                    "grep failed",
                )?;

                let stream = stream_to_exit(
                    &address,
                    vec!["/bin/echo".into(), format!("round-{ri}")],
                    false,
                )
                .await;
                checked(&mut op_count, stream.is_ok(), "stream", "stream failed")?;
            }

            // Session reuse pressure via persistent connection.
            for _ in 0..reuse_execs {
                let exec = ForkdGuestClient::new(address.clone())
                    .exec_in("/workspace", vec!["/bin/true".into()], 9)
                    .await;
                checked(
                    &mut op_count,
                    exec.map(|v| v.get("exit_code").and_then(Value::as_i64) == Some(0))
                        .unwrap_or(false),
                    "reuse_exec",
                    "reuse exec failed",
                )?;
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
            checked(
                &mut op_count,
                write.ok().and_then(|v| v.get("bytes_written").and_then(Value::as_u64)).unwrap_or(0) > 0,
                "summary_write",
                "summary write failed",
            )?;
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
            checked(&mut op_count, ok, "handoff_verify", "handoff digest mismatch")?;

            destroy_sandbox(url, &sid).await?;
            count_op(&mut op_count, "sandbox_destroy");
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
