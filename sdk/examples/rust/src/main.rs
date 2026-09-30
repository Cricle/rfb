//! Quickstart for the published `rfb-sdk` crate (crates.io) — both
//! transports, one flow (the UNIFIED_API contract: identical shapes on
//! either backend). The flow body is shared data: this file embeds
//! sdk/shared/conformance/example-flow.json at compile time (`include_str!`),
//! the same file every other language's quickstart reads.
//!
//! Prerequisites:
//!   * forkd (default): a running controller (FORKD_URL / FORKD_TOKEN, default
//!     http://127.0.0.1:8889) with a ready snapshot, e.g. created with
//!     `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`;
//!   * zeroboot: a running ZBRT bridge (RFB_ZBRT_TCP, default
//!     127.0.0.1:15000) — start one with python/repl.py `--up` or
//!     `rfb-cli zeroboot up`.
//!
//! Run: `cargo run -- [--backend zeroboot] [rfb]`

use rfb::client::{GuestSandbox, GuestTransport, RfbClient, RfbError, SandboxInfo};

/// The SAME calls on either transport — the scenario is shared data.
async fn flow(sandbox: &GuestSandbox) -> Result<(), RfbError> {
    let spec: serde_json::Value = serde_json::from_str(include_str!(
        "../../../shared/conformance/example-flow.json"
    ))
    .expect("shared conformance/example-flow.json");
    for op in spec["ops"].as_array().expect("ops") {
        match op["op"].as_str().expect("op") {
            "ping" => println!("ping: {}", sandbox.ping().await?),
            "exec" => {
                let argv: Vec<&str> = op["argv"].as_array().expect("argv")
                    .iter().map(|a| a.as_str().expect("str")).collect();
                let cwd = op["cwd"].as_str().unwrap_or("/workspace");
                let result = sandbox.exec(&argv, cwd, 60.0, b"").await?;
                println!(
                    "exec: exit={} stdout={}",
                    result.exit_code, result.stdout_text().trim_end()
                );
            }
            "write" => {
                let path = op["path"].as_str().expect("path");
                let text = op["text"].as_str().expect("text").as_bytes();
                let written = sandbox.write(path, text, false, None).await?;
                println!("written: {written} bytes");
            }
            "read" => {
                let file = sandbox
                    .read(op["path"].as_str().expect("path"), None, None)
                    .await?;
                println!("read back {} bytes", file.data.len());
            }
            "ls" => {
                let entries = sandbox.ls(op["path"].as_str().expect("path")).await?;
                println!("ls: {:?}", entries.into_iter().map(|e| e.name).collect::<Vec<_>>());
            }
            other => panic!("unknown op {other}"),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), RfbError> {
    let mut backend = String::from("forkd");
    let mut tag = String::from("rfb");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--backend" {
            backend = args.next().unwrap_or(backend);
        } else {
            tag = arg;
        }
    }

    let timeout = std::time::Duration::from_secs(120);
    if backend == "zeroboot" {
        // Direct attach: the bridge speaks ZBRT at the guest agent; no
        // controller, so nothing to create or delete — the bridge's runner
        // owns the VM lifecycle.
        let tcp = std::env::var("RFB_ZBRT_TCP")
            .unwrap_or_else(|_| "127.0.0.1:15000".to_owned());
        let info = SandboxInfo {
            id: "zeroboot-direct".to_owned(),
            snapshot_tag: "zeroboot-zbrt".to_owned(),
            guest_addr: tcp.clone(),
            ..erased()
        };
        let sandbox = GuestSandbox::attach(info, GuestTransport::Zbrt, timeout)?;
        println!("direct ZBRT sandbox at {tcp}");
        return flow(&sandbox).await;
    }

    // `from_env` reads FORKD_URL / FORKD_TOKEN; `RfbClient::new(base_url,
    // token, timeout)` is the explicit form.
    let client = RfbClient::from_env()?;

    // Block until the snapshot reports status=ready and bootable=true.
    let snapshot = client.wait_snapshot(&tag, 60).await?;
    println!("snapshot {} is ready", snapshot.tag);

    // One sandbox from the snapshot, default options (ndjson transport).
    let sandbox = client.create_sandbox1(&tag).await?;
    println!("sandbox {} created", sandbox.id());
    let result = flow(&sandbox).await;
    // Guarded: a mid-flow failure must not leak a live sandbox.
    let _ = sandbox.delete().await;
    result?;
    println!("sandbox deleted");
    Ok(())
}

/// Placeholder values for the fields a direct attach does not carry.
fn erased() -> SandboxInfo {
    SandboxInfo {
        id: String::new(),
        snapshot_tag: String::new(),
        netns: None,
        created_at_unix: None,
        guest_addr: String::new(),
        memory_limit_mib: None,
        pid: None,
        has_branched: false,
        branch_count: 0,
    }
}
