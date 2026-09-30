#![cfg(all(feature = "zeroboot", target_os = "linux"))]

//! Real-VM fork gate: checkpoint a LIVE sandbox (pause → full snapshot →
//! resume) and restore a second sandbox from it. Triple-gated like the other
//! real-VM suites (cfg + #[ignore] + RFB_REAL_E2E=1); requires hot mode
//! (`RFB_ZBRT_SNAPSHOT_DIR`, single shard is fine).
//!
//! Proven end-to-end:
//! - state inheritance: a file written into the original's workspace exists
//!   in the fork (the workspace tmpfs is guest memory, captured by the dump);
//! - the original survives the fork (resume) and keeps serving;
//! - chained forks work (fork of a fork);
//! - fork dirs are cleaned up when the forked sandboxes drop.

mod common;

use common::realvm::{provider, require_real_hot, ProviderOpts};
use rfb::guest::{ReadRequest, WriteRequest};
use rfb::{Capability, Sandbox, SandboxSpec};
use std::path::Path;

fn spec() -> SandboxSpec {
    SandboxSpec {
        capabilities: vec![
            Capability::Execute,
            Capability::WriteFile,
            Capability::ReadFile,
        ],
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
async fn fork_inherits_live_state_and_original_survives() {
    require_real_hot();
    let provider = provider(ProviderOpts::hot());
    let base = std::env::var("RFB_ZBRT_SNAPSHOT_DIR").unwrap();
    let baseline_dirs = count_fork_dirs(&base);

    let original = provider
        .create_zero_boot(spec())
        .await
        .expect("create original");
    original
        .write(WriteRequest::new("fork-state.txt", b"inherited\n".to_vec()))
        .await
        .expect("write state into original workspace");

    let (original, fork) = original.fork().await.expect("fork original");
    let (fork, grandchild) = fork.fork().await.expect("fork the fork");
    assert_eq!(
        count_fork_dirs(&base),
        baseline_dirs + 2,
        "each fork owns one checkpoint dir"
    );

    // State inheritance: the workspace tmpfs is guest memory, captured whole
    // by the checkpoint — both the fork and the fork-of-fork see the file.
    for sandbox_name in [("fork", &fork), ("grandchild", &grandchild)] {
        let read = sandbox_name
            .1
            .read(ReadRequest::new("fork-state.txt"))
            .await
            .unwrap_or_else(|e| panic!("read state in {}: {e}", sandbox_name.0));
        assert_eq!(
            read.data, b"inherited\n",
            "{} must inherit the workspace state",
            sandbox_name.0
        );
    }

    // Both generations keep serving (resume worked).
    for sandbox_name in [
        ("original", &original),
        ("fork", &fork),
        ("grandchild", &grandchild),
    ] {
        let echo = sandbox_name
            .1
            .exec(rfb::ExecSpec {
                args: vec!["ok".into()],
                ..rfb::ExecSpec::new("echo")
            })
            .await
            .unwrap_or_else(|e| panic!("exec in {}: {e}", sandbox_name.0));
        assert_eq!(echo.stdout, b"ok\n");
    }

    // Dropping the forked sandboxes removes their private checkpoint dirs.
    drop(grandchild);
    drop(fork);
    drop(original);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while count_fork_dirs(&base) > baseline_dirs && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        count_fork_dirs(&base),
        baseline_dirs,
        "fork dirs must not leak past the sandbox lifetime"
    );
    println!("FORK_REAL_OK");
}

fn count_fork_dirs(base: &str) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(std::result::Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("fork-") && entry.path().is_dir() {
                *count += 1;
            } else if entry.path().is_dir() {
                walk(&entry.path(), count);
            }
        }
    }
    let mut count = 0;
    walk(Path::new(base), &mut count);
    count
}
