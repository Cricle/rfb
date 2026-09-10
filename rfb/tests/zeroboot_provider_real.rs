#![cfg(all(feature = "zeroboot", target_os = "linux"))]

// Real-VM verification of the ZeroBoot provider itself (`ZeroBootProvider::create`
// → `boot_and_open` → `ZeroBootSandbox`, one Firecracker VM per sandbox).
// `rfb-cli zeroboot verify` exercises the wire protocol through its own driver;
// these tests exercise the provider/session path that embedders consume.
// Triple gate: cfg + #[ignore] + RFB_REAL_E2E=1 (never in default `cargo test`).
//
// NOTE: a `ZeroBootSession` is bound to the tokio runtime that created it
// (its UDS I/O and worker tasks live on that reactor), so every test runs its
// whole lifecycle — create, operations, drop — inside ONE `futures_block_on`.
// Embedders must do the same: never move a live sandbox across runtimes.

use rfb::core::{Capability, ExecSpec, Sandbox, SandboxProvider, SandboxSpec, TransportKind};
use rfb::guest::{
    CancelRequest, FindRequest, GrepRequest, LsRequest, ReadRequest, StreamEvent, StreamSpec,
    WriteRequest,
};
use rfb::zeroboot::{Config, ZeroBootProvider};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn resx(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("resx")
        .join(relative)
}

fn require_real() {
    if std::env::var("RFB_REAL_E2E").as_deref() != Ok("1") {
        panic!("skip: RFB_REAL_E2E=1 required (real-VM test; use tests/run-real.sh)");
    }
}

fn provider() -> ZeroBootProvider {
    let rootfs = std::env::var_os("RFB_E2E_ZBRT_ROOTFS")
        .map(PathBuf::from)
        .unwrap_or_else(|| resx("rootfs/zeroboot-zbrt-e2e.ext4"));
    ZeroBootProvider::new(Config {
        kernel: Some(resx("kernel/vmlinux-arcbox-0.0.24")),
        rootfs: Some(rootfs),
        firecracker: Some(PathBuf::from("/usr/local/bin/firecracker")),
        guest_port: 5000,
        timeout: Duration::from_secs(30),
    })
}

fn spec_with_all() -> SandboxSpec {
    SandboxSpec {
        capabilities: vec![
            Capability::Execute,
            Capability::Health,
            Capability::Stream,
            Capability::Cancel,
        ],
        ..SandboxSpec::default()
    }
}

fn exec_spec(command: &str, args: &[&str]) -> ExecSpec {
    ExecSpec {
        command: command.into(),
        args: args.iter().map(|a| (*a).into()).collect(),
        cwd: Some("/workspace".into()),
        stdin: None,
        timeout: None,
    }
}

fn process_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn fnv1a_file(path: &Path) -> u64 {
    use std::io::Read;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut file = std::fs::File::open(path).expect("open rootfs");
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).expect("read rootfs");
        if n == 0 {
            break;
        }
        for byte in &buf[..n] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

#[test]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn exec_builtins_and_exit_semantics() {
    require_real();
    futures_block_on(async {
        let sandbox = provider()
            .create(spec_with_all())
            .await
            .expect("create sandbox");
        assert_eq!(sandbox.backend(), rfb::core::BackendKind::VirtualMachine);
        assert_eq!(sandbox.transport(), TransportKind::Vsock);

        // echo
        let out = sandbox
            .exec(exec_spec("echo", &["hello", "zed"]))
            .await
            .expect("exec echo");
        assert_eq!(out.status, Some(0));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello zed");

        // true / false exit codes
        let out = sandbox
            .exec(exec_spec("true", &[]))
            .await
            .expect("exec true");
        assert_eq!(out.status, Some(0));
        let out = sandbox
            .exec(exec_spec("false", &[]))
            .await
            .expect("exec false");
        assert_eq!(out.status, Some(1));

        // Unknown command: ZBRT contract is a normal Result with exit=-1.
        let out = sandbox
            .exec(exec_spec("not-a-command", &[]))
            .await
            .expect("exec unknown");
        assert_eq!(out.status, Some(-1));

        // deadline: bounded execution honours timeout instead of hanging.
        let spec = ExecSpec {
            timeout: Some(Duration::from_secs(5)),
            ..exec_spec("echo", &["with-deadline"])
        };
        let out = sandbox.exec(spec).await.expect("exec with deadline");
        assert_eq!(out.status, Some(0));
    });
}

#[test]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn filesystem_roundtrip_and_escape_rejection() {
    require_real();
    futures_block_on(async {
        let sandbox = provider()
            .create(spec_with_all())
            .await
            .expect("create sandbox");

        // write → read round-trip inside the workspace.
        let payload = b"rfb-zeroboot-provider-e2e\n";
        sandbox
            .write(WriteRequest {
                path: "/workspace/e2e-probe.txt".into(),
                data: payload.to_vec(),
                append: false,
                mode: Some(0o644),
            })
            .await
            .expect("write");
        let read = sandbox
            .read(ReadRequest {
                path: "/workspace/e2e-probe.txt".into(),
                offset: None,
                max_bytes: None,
            })
            .await
            .expect("read");
        assert_eq!(read.data, payload);

        // ls sees the file.
        let ls = sandbox
            .ls(LsRequest {
                path: "/workspace".into(),
                max_results: 100,
            })
            .await
            .expect("ls");
        assert!(ls.entries.iter().any(|e| e.name.contains("e2e-probe.txt")));

        // find matches it.
        let find = sandbox
            .find(FindRequest {
                path: "/workspace".into(),
                pattern: "e2e-probe".into(),
                max_results: 100,
            })
            .await
            .expect("find");
        assert!(find.matches.iter().any(|m| m.contains("e2e-probe.txt")));

        // grep matches the content.
        let grep = sandbox
            .grep(GrepRequest {
                path: "/workspace".into(),
                pattern: "provider-e2e".into(),
                max_results: 100,
                max_bytes: 8192,
            })
            .await
            .expect("grep");
        assert!(grep
            .matches
            .iter()
            .any(|m| m.path.contains("e2e-probe.txt")));

        // Path escape must be rejected, never clamped.
        let escape = sandbox
            .ls(LsRequest {
                path: "../escape".into(),
                max_results: 100,
            })
            .await;
        assert!(escape.is_err(), "ls escape must be rejected");
    });
}

#[test]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn health_stream_and_cancel_roundtrip() {
    require_real();
    futures_block_on(async {
        let sandbox = provider()
            .create(spec_with_all())
            .await
            .expect("create sandbox");

        let health = sandbox.health().await.expect("health");
        assert!(health.healthy, "guest health: {:?}", health.message);

        // Stream a command and collect events through the GuestStream contract.
        let mut stream = Sandbox::stream(
            &*sandbox,
            StreamSpec {
                command: "echo".into(),
                args: vec!["streamed-ok".into()],
                cwd: Some("/workspace".into()),
                pty: None,
                env: Vec::new(),
                timeout: None,
            },
        )
        .await
        .expect("stream");
        let mut saw_exit = false;
        for _ in 0..64 {
            match stream.next_event().await {
                Ok(Some(StreamEvent::Exit { .. })) => {
                    saw_exit = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => {
                    saw_exit = true;
                    break;
                }
                Err(error) => panic!("stream error: {error}"),
            }
        }
        assert!(saw_exit, "stream never terminated");
        stream.stop().await.expect("stream stop");

        // Cancel on a quiescent session is an idempotent round-trip.
        let cancelled = sandbox
            .cancel(CancelRequest { id: None })
            .await
            .expect("cancel");
        assert!(cancelled.cancelled);
    });
}

#[test]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn concurrent_sandboxes_coexist() {
    require_real();
    futures_block_on(async {
        let a = provider().create(spec_with_all()).await.expect("create a");
        let b = provider().create(spec_with_all()).await.expect("create b");
        // Each VM owns an independent vsock UDS, so an identical GUEST_CID must
        // not cross-talk: alternating execs return per-sandbox answers.
        let out_a = a.exec(exec_spec("echo", &["from-a"])).await.expect("a");
        let out_b = b.exec(exec_spec("echo", &["from-b"])).await.expect("b");
        assert_eq!(String::from_utf8_lossy(&out_a.stdout).trim(), "from-a");
        assert_eq!(String::from_utf8_lossy(&out_b.stdout).trim(), "from-b");
        drop(a);
        drop(b);
    });
}

#[test]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn rootfs_source_image_stays_untouched() {
    require_real();
    // Regression for cross-sandbox rootfs pollution: the provider must boot
    // from a private staged copy, so a full sandbox lifecycle (boot, execs,
    // drop) leaves the caller's source image byte-identical. With the old
    // direct-pass path even a clean boot mutated the source (ext4 superblock
    // mount updates), and guest writes leaked into later sandboxes.
    let rootfs = std::env::var_os("RFB_E2E_ZBRT_ROOTFS")
        .map(PathBuf::from)
        .unwrap_or_else(|| resx("rootfs/zeroboot-zbrt-e2e.ext4"));
    let before = fnv1a_file(&rootfs);
    futures_block_on(async {
        let sandbox = provider()
            .create(spec_with_all())
            .await
            .expect("create sandbox");
        let out = sandbox
            .exec(exec_spec("echo", &["isolation-probe"]))
            .await
            .expect("exec echo");
        assert_eq!(out.status, Some(0));
        drop(sandbox);
    });
    let after = fnv1a_file(&rootfs);
    assert_eq!(
        before, after,
        "shared rootfs image was modified by a sandbox lifecycle"
    );
}

#[test]
#[ignore = "boots real Firecracker VMs; run via tests/run-real.sh (RFB_REAL_E2E=1)"]
fn drop_shuts_down_firecracker() {
    require_real();
    futures_block_on(async {
        let sandbox = provider()
            .create_zero_boot(spec_with_all())
            .await
            .expect("create sandbox");
        let pid = sandbox
            .firecracker_pid()
            .expect("sandbox must expose its firecracker pid");
        assert!(
            process_alive(pid),
            "firecracker pid {pid} must be alive while the sandbox exists"
        );
        let out = sandbox.exec(exec_spec("true", &[])).await.expect("exec");
        assert_eq!(out.status, Some(0));
        drop(sandbox);
        // Drop teardown (kill + wait) is synchronous; poll briefly for the
        // process to be reaped. Tracking our own PID is immune to other tests
        // booting their own VMs concurrently — the old global pgrep count was
        // not.
        for _ in 0..40 {
            if !process_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("firecracker pid {pid} survived sandbox drop");
    });
}
