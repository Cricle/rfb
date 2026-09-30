use rfb::client::{GuestSandbox, GuestTransport, RfbClient, RfbError, SandboxInfo};

/// The SAME calls on either transport — shapes never change.
async fn flow(sandbox: &GuestSandbox) -> Result<(), RfbError> {
    println!("ping: {}", sandbox.ping().await?);

    let result = sandbox
        .exec(&["echo", "hello"], "/workspace", 60.0, b"")
        .await?;
    println!(
        "exec: exit={} stdout={}",
        result.exit_code,
        result.stdout_text().trim_end()
    );

    sandbox
        .write("notes.txt", b"hello from rfb-sdk", false, None)
        .await?;
    let file = sandbox.read("notes.txt", None, None).await?;
    println!("read back {} bytes", file.data.len());

    for entry in sandbox.ls("/workspace").await? {
        println!("  {}{}", entry.name, if entry.is_dir { "/" } else { "" });
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
        // owns the VM lifecycle. (Start a bridge with rfbsample's
        // `app.py --up` or `rfb-cli zeroboot up`.)
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
