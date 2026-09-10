// Shared helpers for the real-VM E2E tests (`forkd_real_e2e`,
// `zeroboot_real_e2e`). Not a test target on its own. Each test target embeds
// this module whole, so helpers unused by one target are expected here.
#![allow(dead_code)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Exit code copied from `rfb-cli` (`src/cli/error.rs`); the E2E layer asserts
/// on it so contract drift is caught here.
pub const EXIT_NOVM: i32 = 12;

pub struct RunOutcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Spawn the real `rfb-cli` binary with its current directory at the rfb
/// workspace root so `resx/` asset resolution (find_resx) works.
pub fn cli() -> Command {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rfb-cli"));
    cmd.current_dir(root);
    cmd
}

/// Run with a hard wall-clock timeout; a hang kills the child and panics.
pub fn run(command: &mut Command, timeout: Duration, label: &str) -> RunOutcome {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child: Child = command
        .spawn()
        .unwrap_or_else(|error| panic!("{label}: spawn failed: {error}"));
    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .unwrap_or_else(|error| panic!("{label}: wait failed: {error}"))
        {
            Some(status) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut stdout);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
                }
                return RunOutcome {
                    code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                };
            }
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{label}: timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Strictly parse stdout as a JSON object; a malformed payload fails with an
/// excerpt so the failure stays diagnosable.
pub fn parse_json(out: &RunOutcome, label: &str) -> Value {
    serde_json::from_str(out.stdout.trim()).unwrap_or_else(|error| {
        panic!("{label}: invalid JSON ({error}): {}", truncate(&out.stdout))
    })
}

fn truncate(text: &str) -> String {
    let mut excerpt: String = text.chars().take(400).collect();
    if text.chars().count() > 400 {
        excerpt.push_str("...");
    }
    excerpt
}

/// Triple gate: `#[cfg(all(unix, feature = "cli"))]` + `#[ignore]` +
/// `RFB_REAL_E2E=1`. When the env flag is missing the test panics with a
/// clear message instead of silently passing (never fake real evidence).
pub fn require_real() {
    if std::env::var("RFB_REAL_E2E").as_deref() != Ok("1") {
        panic!("skip: RFB_REAL_E2E=1 required (real-VM test; use tests/run-real.sh)");
    }
}

/// The ready+bootable forkd snapshot tag every forkd E2E runs against.
pub fn snapshot_tag() -> String {
    std::env::var("RFB_E2E_SNAPSHOT_TAG").unwrap_or_else(|_| {
        panic!("RFB_E2E_SNAPSHOT_TAG must name a ready+bootable forkd snapshot")
    })
}

/// Absolute path to a file inside the rfb `resx/` asset tree.
pub fn resx(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("resx")
        .join(relative)
}

/// Guest rootfs image directory. Prebuilt blobs are no longer committed; the
/// `tests/run-real.sh` driver builds them with `rfb-cli image build-rootfs`
/// and points `RFB_E2E_ROOTFS_DIR` at the output. Falls back to
/// `resx/rootfs/` for out-of-band artifacts.
pub fn rootfs(name: &str) -> PathBuf {
    std::env::var_os("RFB_E2E_ROOTFS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| resx("rootfs"))
        .join(name)
}

/// Run a future to completion on a current-thread tokio runtime (rfb's tokio
/// dependency has `rt` but not `macros`, so `#[tokio::test]` is unavailable).
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}
