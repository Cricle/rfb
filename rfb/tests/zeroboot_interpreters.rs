#![cfg(all(target_os = "linux", feature = "cli"))]

//! Real-VM interpreter E2E for the multi-call zeroboot-zbrt guest.
//!
//! Boots the image produced by `rfb-cli image build-all --features
//! cli,rustpython,mlua` and drives structured ZBRT V1 Execute frames (full
//! argv + stdin bytes) directly over the vsock UDS relay. This covers argv
//! entries containing spaces (`python3 -c "..."`, `lua -e "..."`) that the
//! shell-string splitting in `zeroboot verify` cannot express, plus offline
//! import from the baked site roots and the executor's timeout kill.
//!
//! Triple-gated like the other real-VM tests: linux+cli cfg, `#[ignore]`, and
//! `RFB_REAL_E2E=1` (checked in-test, never silently skipped). Run on WSL or
//! native Linux with:
//!
//! ```text
//! RFB_REAL_E2E=1 RFB_E2E_ROOTFS_DIR=<image build-all output dir> \
//!   cargo test -p rfb --features cli --test zeroboot_interpreters \
//!   -- --ignored --test-threads=1
//! ```

mod common;

use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

use rfb::cli::rfb1::{boot_firecracker_with, connect_vsock_uds, BootOptions};
use rfb::protocol::{Error, Execute, Exit, Frame, Kind, Output};

const GUEST_CID: u32 = 3;
const GUEST_PORT: u16 = 5000;
const BOOT_TIMEOUT: Duration = Duration::from_secs(45);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// One booted Firecracker VM; killed and cleaned up on drop.
struct Vm {
    child: Child,
    uds: PathBuf,
    work_dir: PathBuf,
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.work_dir);
    }
}

fn boot_vm() -> Vm {
    let kernel = common::resx("kernel/vmlinux-arcbox-0.0.24");
    let rootfs = common::rootfs("rootfs.ext4");
    assert!(kernel.is_file(), "kernel missing: {}", kernel.display());
    assert!(
        rootfs.is_file(),
        "rootfs missing: {} (point RFB_E2E_ROOTFS_DIR at the image build-all output directory)",
        rootfs.display()
    );
    let firecracker = std::env::var("RFB_E2E_FIRECRACKER").unwrap_or_else(|_| {
        let candidate = common::resx("firecracker/firecracker-v1.12.1");
        if candidate.is_file() {
            candidate.to_string_lossy().into_owned()
        } else {
            "firecracker".to_owned()
        }
    });

    let work_dir = std::env::temp_dir().join(format!("rfb-zb-interp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).expect("create work dir");
    let uds = work_dir.join("vsock.sock");
    let log = work_dir.join("firecracker.log");
    let child = boot_firecracker_with(BootOptions {
        firecracker: &firecracker,
        kernel: &kernel,
        rootfs: &rootfs,
        work_dir: &work_dir,
        cid: GUEST_CID,
        uds: &uds,
        log: &log,
        init_path: "/init",
        vsock_flag: false,
    })
    .expect("boot firecracker");
    Vm {
        child,
        uds,
        work_dir,
    }
}

/// Terminal outcome of one Execute round trip.
enum Outcome {
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Rejected {
        code: u32,
        message: String,
    },
}

/// One Execute on a fresh vsock connection: full argv (arguments may contain
/// spaces), stdin bytes, and an explicit deadline. Collects Output frames and
/// the terminal Exit, or the guest's Error frame.
fn execute(vm: &Vm, name: &str, argv: &[&str], stdin: &str, timeout_ms: u32) -> Outcome {
    let mut stream = connect_vsock_uds(&vm.uds, GUEST_PORT, BOOT_TIMEOUT)
        .unwrap_or_else(|error| panic!("{name}: vsock connect failed: {error:?}"));
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set write timeout");

    let mut request_id = [0u8; 16];
    let label = name.as_bytes();
    let used = label.len().min(16);
    request_id[..used].copy_from_slice(&label[..used]);

    let request = Execute {
        argv: argv.iter().map(|argument| argument.to_string()).collect(),
        cwd: None,
        stdin: stdin.as_bytes().to_vec(),
        timeout_ms,
    };
    let frame = Frame {
        kind: Kind::Execute,
        flags: 0,
        request_id,
        payload: request
            .encode()
            .unwrap_or_else(|error| panic!("{name}: encode Execute: {error}")),
    };
    frame
        .encode(&mut stream)
        .unwrap_or_else(|error| panic!("{name}: write Execute frame: {error}"));

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let response = Frame::decode(&mut stream)
            .unwrap_or_else(|error| panic!("{name}: read frame: {error}"));
        assert_eq!(
            response.request_id, request_id,
            "{name}: request id mismatch"
        );
        match response.kind {
            Kind::Output => {
                let output = Output::decode(&response.payload)
                    .unwrap_or_else(|error| panic!("{name}: invalid Output: {error}"));
                if output.stream == 0 {
                    stdout.extend(output.data);
                } else {
                    stderr.extend(output.data);
                }
            }
            Kind::Exit => {
                let exit = Exit::decode(&response.payload)
                    .unwrap_or_else(|error| panic!("{name}: invalid Exit: {error}"));
                return Outcome::Exited {
                    code: exit.code,
                    stdout,
                    stderr,
                };
            }
            Kind::Error => {
                let error = Error::decode(&response.payload)
                    .unwrap_or_else(|error| panic!("{name}: invalid Error: {error}"));
                return Outcome::Rejected {
                    code: error.code,
                    message: error.message,
                };
            }
            other => panic!("{name}: unexpected frame kind {other:?}"),
        }
    }
}

fn exited(name: &str, outcome: Outcome) -> (i32, Vec<u8>, Vec<u8>) {
    match outcome {
        Outcome::Exited {
            code,
            stdout,
            stderr,
        } => (code, stdout, stderr),
        Outcome::Rejected { code, message } => {
            panic!("{name}: expected Exit, got Error code={code} message={message}")
        }
    }
}

#[test]
#[ignore]
fn zeroboot_guest_interpreters() {
    common::require_real();
    let vm = boot_vm();

    // python3 -c: arithmetic proves a full interpreter bootstrap, not a stub.
    let (code, stdout, _stderr) = exited(
        "python-c",
        execute(
            &vm,
            "python-c",
            &["python3", "-c", "print(1 + 1)"],
            "",
            30000,
        ),
    );
    assert_eq!(code, 0, "python3 -c exit code");
    assert_eq!(stdout, b"2\n", "python3 -c stdout");

    // python3 with a script on stdin (argv "-").
    let (code, stdout, _stderr) = exited(
        "python-stdin",
        execute(
            &vm,
            "python-stdin",
            &["python3", "-"],
            "print(7 * 6)\n",
            30000,
        ),
    );
    assert_eq!(code, 0, "python3 stdin exit code");
    assert_eq!(stdout, b"42\n", "python3 stdin stdout");

    // Offline import from the baked site-packages root (/usr/lib/python3/
    // site-packages/demo/__init__.py written by build-rootfs --py-site-dir).
    let (code, stdout, stderr) = exited(
        "python-site",
        execute(
            &vm,
            "python-site",
            &["python3", "-c", "import demo; print(demo.VALUE)"],
            "",
            30000,
        ),
    );
    assert_eq!(
        code,
        0,
        "python3 site import failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(stdout, b"42\n", "python3 site import stdout");

    // lua -e: require('demo') resolves through /usr/lib/lua/5.4/?.lua.
    let (code, stdout, stderr) = exited(
        "lua-require",
        execute(
            &vm,
            "lua-require",
            &[
                "lua",
                "-e",
                "local demo = require('demo'); print(demo.answer())",
            ],
            "",
            30000,
        ),
    );
    assert_eq!(
        code,
        0,
        "lua require failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(stdout, b"42\n", "lua require stdout");

    // Syntax errors are reported on stderr with exit code 1.
    let (code, _stdout, stderr) = exited(
        "python-syntax",
        execute(&vm, "python-syntax", &["python3", "-c", "def ("], "", 30000),
    );
    assert_eq!(code, 1, "python3 syntax error exit code");
    assert!(!stderr.is_empty(), "python3 syntax error stderr empty");

    // Missing script file exits 1 with a diagnostic.
    let (code, _stdout, _stderr) = exited(
        "python-missing-file",
        execute(
            &vm,
            "python-missing-file",
            &["python3", "/tmp/nope.py"],
            "",
            30000,
        ),
    );
    assert_eq!(code, 1, "python3 missing file exit code");

    // Bare `python3`/`lua` read the script from stdin (CPython/lua REPL
    // semantics); an empty stdin therefore executes an empty program and
    // exits 0. Usage errors come from extra arguments, which the multi-call
    // entries refuse to forward.
    let (code, _stdout, _stderr) = exited(
        "python-usage",
        execute(&vm, "python-usage", &["python3", "a", "b"], "", 30000),
    );
    assert_eq!(code, 2, "python3 usage exit code");
    let (code, _stdout, _stderr) = exited(
        "lua-usage",
        execute(&vm, "lua-usage", &["lua", "a", "b"], "", 30000),
    );
    assert_eq!(code, 2, "lua usage exit code");

    // The executor's deadline kills runaway interpreters: the request must
    // terminate promptly with a rejection frame or a non-zero exit.
    let started = Instant::now();
    let outcome = execute(
        &vm,
        "python-timeout",
        &["python3", "-c", "import time; time.sleep(30)"],
        "",
        1500,
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "timeout request took {elapsed:?}; the guest did not enforce the deadline"
    );
    match outcome {
        Outcome::Rejected { .. } => {}
        Outcome::Exited { code, .. } => assert_ne!(code, 0, "timeout kill exited 0"),
    }
}
