//! Clap command-line types for `rfb-cli`. The binary (`src/bin/rfb-cli.rs`)
//! is a thin router over these; the structs live here so `rfb-cli --help`
//! and the shared CLI library expose one coherent command surface.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[cfg(unix)]
use crate::cli::web_bench::WebBenchArgs;

/// Parsed command-line arguments for the `rfb-cli` binary.
#[derive(Parser, Debug)]
#[command(
    name = "rfb-cli",
    version,
    about = "RFB image staging workflow and sandbox tooling: doctor, image, forkd, rfb1, cleanup, bench",
    long_about = "RFB image staging workflow and sandbox tooling.\n\nTypical usage:\n  rfb-cli doctor\n  rfb-cli image init IMAGE_DIR\n  rfb-cli image validate MANIFEST\n  rfb-cli image build MANIFEST --execute\n  rfb-cli forkd preflight\n\nExit codes:\n  0   success (or an optional VM check was skipped)\n  2   usage or argument error\n  3   validation failure\n  4   I/O failure\n  5   external tool failure\n  12  required VM prerequisites unavailable"
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true, help = "Emit machine-readable JSON")]
    pub json: bool,
    /// Selected top-level operation.
    #[command(subcommand)]
    pub command: CommandLine,
}

/// Top-level `rfb-cli` commands.
#[derive(Subcommand, Debug)]
pub enum CommandLine {
    /// Detect host capabilities and tool availability.
    Doctor,
    /// Image staging, build, and inspection.
    Image {
        /// Image staging/build subcommand.
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// forkd controller/guest orchestration.
    Forkd {
        /// forkd orchestration subcommand.
        #[command(subcommand)]
        command: ForkdCommand,
    },
    /// RFB1 framed-vsock acceptance against a real Firecracker VM.
    #[cfg(unix)]
    Rfb1 {
        /// RFB1 acceptance subcommand.
        #[command(subcommand)]
        command: Rfb1Command,
    },
    /// ZeroBoot ZBRT real-VM acceptance and benchmark.
    #[cfg(unix)]
    Zeroboot {
        /// ZeroBoot acceptance/benchmark subcommand.
        #[command(subcommand)]
        command: ZerobootCommand,
    },
    /// Scoped artifact cleanup in an rfb-runtime tree.
    Cleanup(CleanupArgs),
    /// Unified entry point used by the thin shell wrappers.
    #[command(hide = true)]
    Run(RunArgs),
    /// Run the forkd microbenchmark and workload, aggregated.
    Bench {
        /// Benchmark subcommand.
        #[command(subcommand)]
        command: BenchCommand,
    },
    /// Real RFB web service concurrency + resource sampler (Linux/WSL).
    #[cfg(unix)]
    Web {
        /// Web benchmark subcommand.
        #[command(subcommand)]
        command: WebCommand,
    },
}

/// Image staging, inspection, and build subcommands.
#[derive(Subcommand, Debug)]
pub enum ImageCommand {
    /// Print the parsed manifest and derived image diagnostics.
    Inspect(ImagePath),
    /// Validate an image manifest against the current profile.
    Validate(ImagePath),
    /// Initialize a new image staging directory.
    Init(InitArgs),
    /// Build an image from a manifest (dry-run unless `--execute`).
    Build(BuildArgs),
    /// Build a rootfs ext4 directly from a runtime binary (env setup path).
    BuildRootfs(BuildRootfsArgs),
    /// Validate a kernel ELF and print its digest (check-kernel.sh).
    CheckKernel(ImagePath),
    /// Build a static runtime binary for a target triple (build-static.sh).
    BuildStatic(BuildStaticArgs),
}

/// forkd controller/guest orchestration subcommands.
#[derive(Subcommand, Debug)]
pub enum ForkdCommand {
    /// Read-only controller/snapshot preflight.
    Preflight(ForkdPreflightArgs),
    /// Bind a local artifact manifest to a ready controller snapshot.
    SnapshotBind(ForkdSnapshotBindArgs),
    /// Full acceptance: create/ping/stream/exec/RPC + destroy (ZBRT VERSION=1 full protocol gate).
    Acceptance(ForkdGateArgs),
    /// Microbenchmark create/ping/stream/exec/cleanup quantiles.
    Benchmark(ForkdBenchArgs),
    /// Complex business workload across multiple sandboxes.
    Workload(ForkdWorkloadArgs),
    /// Create one sandbox and report its guest address.
    SandboxCreate(ForkdSandboxArgs),
    /// Destroy a sandbox by id.
    SandboxDestroy(ForkdSandboxDestroyArgs),
    /// Create a snapshot through the official `forkd` binary (thin wrapper).
    SnapshotCreate(ForkdSnapshotCreateArgs),
    /// Show snapshot info through the official `forkd` binary (thin wrapper).
    SnapshotInfo(ForkdSnapshotInfoArgs),
    /// Delete a snapshot through the official `forkd` binary (thin wrapper).
    SnapshotDelete(ForkdSnapshotDeleteArgs),
}

/// RFB1 framed-vsock acceptance subcommands.
#[cfg(unix)]
#[derive(Subcommand, Debug)]
pub enum Rfb1Command {
    /// Run RFB1 StartTurn/full-protocol acceptance against a real Firecracker v1.16.1 VM.
    Acceptance(Rfb1AcceptanceArgs),
}

/// ZeroBoot ZBRT real-VM verification subcommands.
#[cfg(unix)]
#[derive(Subcommand, Debug)]
pub enum ZerobootCommand {
    /// Run ZeroBoot ZBRT VERSION=1 full-protocol verification (echo/true/false/concurrent/malformed).
    Verify(ZerobootVerifyArgs),
}

/// A single image manifest path argument.
#[derive(clap::Args, Debug)]
pub struct ImagePath {
    /// Path to the image manifest JSON.
    #[arg(value_name = "MANIFEST", help = "Path to the image manifest JSON")]
    pub manifest: PathBuf,
}

/// Arguments for `rfb-cli image init`.
#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// Directory to initialize as an image staging area.
    #[arg(value_name = "DIRECTORY")]
    pub directory: PathBuf,
    /// Initial staging directory size in bytes (default 64 MiB).
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    pub size: u64,
    /// Overwrite an existing staging directory.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `rfb-cli image build`.
#[derive(clap::Args, Debug)]
pub struct BuildArgs {
    /// Path to the image manifest JSON.
    #[arg(value_name = "MANIFEST", help = "Path to the image manifest JSON")]
    pub manifest: PathBuf,
    /// Output image path (defaults to the manifest's output path).
    #[arg(long, value_name = "IMAGE")]
    pub output: Option<PathBuf>,
    /// Allow invoking mke2fs/debugfs; without this, only a safe dry-run is performed.
    #[arg(
        long,
        help = "Allow invoking mke2fs/debugfs; without this, only a safe dry-run is performed"
    )]
    pub execute: bool,
}

/// Arguments for `rfb-cli image build-rootfs`.
#[derive(clap::Args, Debug)]
pub struct BuildRootfsArgs {
    /// Runtime binary to install in the rootfs.
    #[arg(
        value_name = "RUNTIME_BIN",
        help = "Runtime binary to install in the rootfs"
    )]
    pub runtime: PathBuf,
    /// Output ext4 rootfs image path.
    #[arg(value_name = "OUTPUT", help = "Output ext4 rootfs image path")]
    pub output: PathBuf,
    /// Rootfs size in MiB (defaults to an automatic size).
    #[arg(long, value_name = "MIB")]
    pub size_mb: Option<u64>,
    /// Rootfs mode: `rfb-vsock`, `forkd-agent`, or `zeroboot-zbrt` (default `rfb-vsock`).
    #[arg(
        long,
        default_value = "rfb-vsock",
        value_parser = ["rfb-vsock", "forkd-agent", "zeroboot-zbrt"],
        help = "Rootfs mode: rfb-vsock, forkd-agent, or zeroboot-zbrt"
    )]
    pub mode: String,
    /// Allow dynamically-linked runtime binaries.
    #[arg(long, help = "Allow dynamically-linked runtime binaries")]
    pub allow_dynamic: bool,
    /// Refuse to overwrite an existing output without this flag.
    #[arg(
        long,
        help = "Refuse to overwrite an existing output without this flag"
    )]
    pub force: bool,
}

/// Arguments for `rfb-cli cleanup`.
#[derive(clap::Args, Debug)]
pub struct CleanupArgs {
    /// Target rfb-runtime directory to clean.
    #[arg(long, value_name = "PATH", help = "Target rfb-runtime directory")]
    pub target: PathBuf,
    /// Actually delete (otherwise dry-run).
    #[arg(long, help = "Actually delete (otherwise dry-run)")]
    pub yes: bool,
    /// Only list what would be deleted.
    #[arg(long, help = "Only list what would be deleted")]
    pub dry_run: bool,
}

/// Arguments for `rfb-cli image build-static`.
#[derive(clap::Args, Debug)]
pub struct BuildStaticArgs {
    /// Workspace root (the directory containing the workspace Cargo.toml).
    #[arg(
        long,
        default_value = ".",
        value_name = "ROOT",
        help = "Workspace root (Cargo.toml parent)"
    )]
    pub root: PathBuf,
    /// Rust target triple to build for (default `x86_64-unknown-linux-musl`).
    #[arg(long, default_value = "x86_64-unknown-linux-musl")]
    pub target: String,
    /// Package to build within the workspace (default `rfb-runtime`).
    #[arg(long, default_value = "rfb-runtime")]
    pub package: String,
}

/// Unified dispatch used by the thin shell wrappers.
#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Target operation to run.
    #[command(subcommand)]
    pub target: RunTarget,
}

/// Targets for the wrapper-driven `rfb-cli run` path.
#[derive(Subcommand, Debug)]
pub enum RunTarget {
    /// forkd acceptance (the run-forkd.sh path).
    Forkd(RunForkdArgs),
    /// RFB1/vsock acceptance (the run-rfb1.sh path).
    #[cfg(unix)]
    Rfb1(Rfb1AcceptanceArgs),
    /// Workload gate across multiple sandboxes.
    Workload(ForkdWorkloadArgs),
    /// Microbenchmark.
    Benchmark(ForkdBenchArgs),
    /// Preflight gate.
    Preflight(ForkdPreflightArgs),
}

/// Arguments for the forkd acceptance path used by shell wrappers.
#[derive(clap::Args, Debug)]
pub struct RunForkdArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to use (`FORKD_SNAPSHOT_TAG`, default `rfb`).
    #[arg(long, default_value = "rfb", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
    /// Exit 12 if prerequisites are missing.
    #[arg(long, help = "Exit 12 if prerequisites are missing")]
    pub require_vm: bool,
    /// Require controller provenance and a verified snapshot binding.
    #[arg(
        long,
        help = "Require controller provenance and verified snapshot binding"
    )]
    pub require_provenance: bool,
}

/// Benchmark subcommands.
#[derive(Subcommand, Debug)]
pub enum BenchCommand {
    /// forkd microbenchmark (create/ping/stream/exec/cleanup quantiles).
    Forkd(BenchForkdArgs),
    /// RFB1 codec microbenchmark is kept in the runtime benches; this lists
    /// available `cargo bench` targets.
    List,
}

/// Arguments for the forkd benchmark.
#[derive(clap::Args, Debug)]
pub struct BenchForkdArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to benchmark (`FORKD_SNAPSHOT_TAG`, default `rfb`).
    #[arg(long, default_value = "rfb", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
    /// Number of benchmark iterations (default 10).
    #[arg(long, default_value_t = 10)]
    pub n: usize,
}

/// Real RFB web service benchmark subcommands.
#[cfg(unix)]
#[derive(Subcommand, Debug)]
pub enum WebCommand {
    /// Real RFB web service SSE concurrency + resource bench (the former
    /// rfb-web-concurrency.sh path).
    Bench(WebBenchArgs),
}

/// Arguments for `rfb-cli forkd preflight`.
#[derive(clap::Args, Debug)]
pub struct ForkdPreflightArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to preflight (`FORKD_SNAPSHOT_TAG`, default `rfb`).
    #[arg(long, default_value = "rfb", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
    /// Optional artifact manifest to cross-check against the snapshot.
    #[arg(long)]
    pub artifact_manifest: Option<PathBuf>,
    /// Optional snapshot binding to validate during preflight.
    #[arg(long)]
    pub snapshot_binding: Option<PathBuf>,
    /// Exit 12 if prerequisites are missing.
    #[arg(long, help = "Exit 12 if prerequisites are missing")]
    pub require_vm: bool,
}

/// Arguments for `rfb-cli forkd snapshot-bind`.
#[derive(clap::Args, Debug)]
pub struct ForkdSnapshotBindArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to bind.
    #[arg(long)]
    pub tag: String,
    /// Local artifact manifest to bind to the snapshot.
    #[arg(long)]
    pub artifact: PathBuf,
    /// Output path for the generated binding.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Require controller provenance and matching snapshot digest.
    #[arg(
        long,
        help = "Require controller provenance and matching snapshot digest"
    )]
    pub require_provenance: bool,
}

/// Arguments for `rfb-cli forkd acceptance`.
#[derive(clap::Args, Debug)]
pub struct ForkdGateArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to accept (`FORKD_SNAPSHOT_TAG`, default `rfb`).
    #[arg(long, default_value = "rfb", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
    /// Exit non-zero if prerequisites are missing.
    #[arg(long, help = "Exit non-zero if prerequisites are missing")]
    pub require_vm: bool,
    /// Require controller provenance and a verified snapshot binding.
    #[arg(
        long,
        help = "Require controller provenance and verified snapshot binding"
    )]
    pub require_provenance: bool,
}

/// Arguments for `rfb-cli forkd benchmark`.
#[derive(clap::Args, Debug)]
pub struct ForkdBenchArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to benchmark (`FORKD_SNAPSHOT_TAG`, default `rfb`).
    #[arg(long, default_value = "rfb", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
    /// Number of benchmark iterations (default 10).
    #[arg(long, default_value_t = 10)]
    pub n: usize,
}

/// Arguments for `rfb-cli forkd workload`.
#[derive(clap::Args, Debug)]
pub struct ForkdWorkloadArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to run the workload on (`FORKD_SNAPSHOT_TAG`, default `rfb-final-verified`).
    #[arg(long, default_value = "rfb-final-verified", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
    /// Number of sandboxes to create (default 3).
    #[arg(long, default_value_t = 3)]
    pub sandboxes: usize,
    /// Number of workload rounds per sandbox (default 2).
    #[arg(long, default_value_t = 2)]
    pub rounds: usize,
    /// Number of exec operations to reuse across rounds (default 25).
    #[arg(long, default_value_t = 25)]
    pub reuse_execs: usize,
}

/// Arguments for `rfb-cli forkd sandbox-create`.
#[derive(clap::Args, Debug)]
pub struct ForkdSandboxArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to create the sandbox from (`FORKD_SNAPSHOT_TAG`, default `rfb`).
    #[arg(long, default_value = "rfb", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
}

/// Arguments for `rfb-cli forkd sandbox-destroy`.
#[derive(clap::Args, Debug)]
pub struct ForkdSandboxDestroyArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Sandbox identifier to destroy.
    #[arg(value_name = "SANDBOX_ID", help = "Sandbox identifier to destroy")]
    pub id: String,
}

/// Arguments for `rfb-cli forkd snapshot-create`.
#[derive(clap::Args, Debug)]
pub struct ForkdSnapshotCreateArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to create.
    #[arg(long)]
    pub tag: String,
    /// vmlinux kernel path (`FORKD_KERNEL`, default `resx/kernel/vmlinux-5.10.225`).
    #[arg(long)]
    pub kernel: Option<PathBuf>,
    /// forkd-agent rootfs path (`FORKD_ROOTFS`, default `resx/rootfs/forkd-agent.ext4`).
    #[arg(long)]
    pub rootfs: Option<PathBuf>,
    /// Snapshot-private rootfs copy path (`FORKD_ROOTFS_COPY`).
    ///
    /// `forkd snapshot` boots the parent VM against this ext4 file with write
    /// access, so the artifact-pinned rootfs is never booted directly. When
    /// omitted, the copy lives inside the forkd snapshot directory
    /// (`<XDG_DATA_HOME|HOME>/.local/share/forkd/snapshots/<tag>/rootfs.ext4`)
    /// so the snapshot stays self-contained.
    #[arg(long, env = "FORKD_ROOTFS_COPY")]
    pub rootfs_copy: Option<PathBuf>,
    /// Host tap device to attach (`FORKD_TAP`, default `forkd-tap0`).
    #[arg(long)]
    pub tap: Option<String>,
    /// Parent VM memory in MiB (default from forkd boot config).
    #[arg(long)]
    pub mem_size_mib: Option<u64>,
    /// Seconds to wait for the guest to settle before snapshotting (default 10).
    #[arg(long, default_value_t = 10)]
    pub boot_wait_secs: u64,
    /// Official forkd binary to delegate to (`FORKD_BIN`, `resx/forkd/forkd`, or PATH).
    #[arg(long, env = "FORKD_BIN")]
    pub forkd_bin: Option<PathBuf>,
    /// Fail closed unless the created snapshot reports complete provenance.
    #[arg(
        long,
        help = "Require complete controller provenance from the created snapshot"
    )]
    pub require_provenance: bool,
}

/// Arguments for `rfb-cli forkd snapshot-info`.
#[derive(clap::Args, Debug)]
pub struct ForkdSnapshotInfoArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to inspect.
    #[arg(long)]
    pub tag: String,
    /// Official forkd binary to delegate to (`FORKD_BIN`, `resx/forkd/forkd`, or PATH).
    #[arg(long, env = "FORKD_BIN")]
    pub forkd_bin: Option<PathBuf>,
    /// Fail closed unless the snapshot reports complete provenance.
    #[arg(
        long,
        help = "Require complete controller provenance from the snapshot"
    )]
    pub require_provenance: bool,
}

/// Arguments for `rfb-cli forkd snapshot-delete`.
#[derive(clap::Args, Debug)]
pub struct ForkdSnapshotDeleteArgs {
    /// forkd controller URL (`FORKD_URL`, default `http://127.0.0.1:8889`).
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub url: String,
    /// Snapshot tag to delete.
    #[arg(long)]
    pub tag: String,
    /// Official forkd binary to delegate to (`FORKD_BIN`, `resx/forkd/forkd`, or PATH).
    #[arg(long, env = "FORKD_BIN")]
    pub forkd_bin: Option<PathBuf>,
    /// Delete this snapshot and every snapshot chained off it (mirrors `forkd rmi --cascade`).
    #[arg(long, conflicts_with = "force")]
    pub cascade: bool,
    /// Delete even if it would orphan child snapshots (mirrors `forkd rmi --force`).
    #[arg(long, conflicts_with = "cascade")]
    pub force: bool,
}

/// Arguments for the RFB1/vsock acceptance against a real Firecracker VM.
#[cfg(unix)]
#[derive(clap::Args, Debug)]
pub struct Rfb1AcceptanceArgs {
    /// Kernel image path (default `/boot/vmlinux`).
    #[arg(long, default_value = "/boot/vmlinux")]
    pub kernel: PathBuf,
    /// Rootfs ext4 image path (default `/tmp/rfb-runtime-rootfs.ext4`).
    #[arg(long, default_value = "/tmp/rfb-runtime-rootfs.ext4")]
    pub rootfs: PathBuf,
    /// Firecracker binary to use (default `firecracker`).
    #[arg(long, default_value = "firecracker")]
    pub firecracker: String,
    /// Exit non-zero if prerequisites are missing.
    #[arg(long, help = "Exit non-zero if prerequisites are missing")]
    pub require_vm: bool,
}

/// Arguments for the ZeroBoot ZBRT real-VM verification.
#[cfg(unix)]
#[derive(clap::Args, Debug)]
pub struct ZerobootVerifyArgs {
    /// Kernel image path (default `/boot/vmlinux`).
    #[arg(long, default_value = "/boot/vmlinux")]
    pub kernel: PathBuf,
    /// Rootfs ext4 image path (default `/tmp/rootfs.ext4`).
    #[arg(long, default_value = "/tmp/rootfs.ext4")]
    pub rootfs: PathBuf,
    /// Firecracker binary to use (default `firecracker`).
    #[arg(long, default_value = "firecracker")]
    pub firecracker: String,
    /// Exit non-zero if prerequisites are missing.
    #[arg(long, help = "Exit non-zero if prerequisites are missing")]
    pub require_vm: bool,
    /// Run the 100-sample round-trip benchmark.
    #[arg(long, help = "Run the 100-sample round-trip benchmark")]
    pub bench: bool,
}
