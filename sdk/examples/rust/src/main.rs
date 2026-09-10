//! Quickstart for the published `rfb-sdk` crate (crates.io).
//!
//! Prerequisites:
//!   * a running forkd controller (default `http://127.0.0.1:8889`, or set
//!     `FORKD_URL` / `FORKD_TOKEN`);
//!   * a ready + bootable snapshot, e.g. created with
//!     `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`.
//!
//! Run: `cargo run -- rfb`

use rfb::client::RfbClient;
use rfb::client::RfbError;

#[tokio::main]
async fn main() -> Result<(), RfbError> {
    let tag = std::env::args().nth(1).unwrap_or_else(|| "rfb".to_owned());

    // `from_env` reads FORKD_URL / FORKD_TOKEN; `RfbClient::new(base_url,
    // token, timeout)` is the explicit form.
    let client = RfbClient::from_env()?;

    // Block until the snapshot reports status=ready and bootable=true.
    let snapshot = client.wait_snapshot(&tag, 60).await?;
    println!("snapshot {} is ready", snapshot.tag);

    // One sandbox from the snapshot, default options (ndjson transport).
    let sandbox = client.create_sandbox1(&tag).await?;
    println!("sandbox {} created", sandbox.id());

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

    sandbox.delete().await?;
    println!("sandbox deleted");
    Ok(())
}
