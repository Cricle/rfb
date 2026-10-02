//! Quickstart: the same five sandbox calls on forkd (default, controller
//! snapshot) or zeroboot (direct ZBRT attach). Backends and run matrix:
//! see ../README.md.
//!
//! Run: `cargo run -- [--backend zeroboot] [rfb]`

use rfb::client::{GuestSandbox, GuestTransport, RfbClient, RfbError, SandboxInfo};

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
    let (sandbox, owned) = if backend == "zeroboot" {
        // Direct attach: the bridge speaks ZBRT at the guest agent; the
        // bridge's runner owns the VM — nothing to create or delete.
        let tcp =
            std::env::var("RFB_ZBRT_TCP").unwrap_or_else(|_| "127.0.0.1:15000".to_owned());
        let info = SandboxInfo {
            id: "zeroboot-direct".to_owned(),
            snapshot_tag: "zeroboot-zbrt".to_owned(),
            guest_addr: tcp,
            ..erased()
        };
        (GuestSandbox::attach(info, GuestTransport::Zbrt, timeout)?, false)
    } else {
        // `from_env` reads FORKD_URL / FORKD_TOKEN; one sandbox from the
        // snapshot once it is ready.
        let client = RfbClient::from_env()?;
        let snapshot = client.wait_snapshot(&tag, 60).await?;
        println!("snapshot {} is ready", snapshot.tag);
        (client.create_sandbox1(&tag).await?, true)
    };
    println!("sandbox {} via {backend}", sandbox.id());

    let result = flow(&sandbox).await;
    if owned {
        // Guarded: a mid-flow failure must not leak a live sandbox.
        let _ = sandbox.delete().await;
        println!("sandbox deleted");
    }
    result
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

/// The SAME five calls on either backend.
async fn flow(sandbox: &GuestSandbox) -> Result<(), RfbError> {
    println!("ping: {}", sandbox.ping().await?);
    let result = sandbox.exec(&["echo", "hello"], "/workspace", 60.0, b"").await?;
    println!(
        "exec: exit={} stdout={}",
        result.exit_code, result.stdout_text().trim_end()
    );
    let written = sandbox
        .write("notes.txt", b"hello from rfb-sdk", false, None)
        .await?;
    println!("written: {written} bytes");
    let file = sandbox.read("notes.txt", None, None).await?;
    println!("read back {} bytes", file.data.len());
    let names: Vec<_> = sandbox
        .ls("/workspace")
        .await?
        .into_iter()
        .map(|e| e.name)
        .collect();
    println!("ls: {names:?}");
    Ok(())
}
