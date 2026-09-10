//! Command-line arguments for `rfb-cli web bench`. Env overrides preserve the
//! legacy script's `RFB_WEB_*`, `RFB_WEB_BENCH_*`, and `FORKD_URL` contracts.

use clap::Args;
use std::path::PathBuf;

/// Command-line arguments for `rfb-cli web bench`; the struct carries every
/// tunable from the legacy rfb-web-concurrency.sh sampler.
#[derive(Args, Debug, Clone)]
pub struct WebBenchArgs {
    /// RFB web service config.json (used to locate `rig.api_key_env`).
    #[arg(long, env = "RFB_WEB_CONFIG", value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// RFB web service base URL (loopback only).
    #[arg(
        long,
        default_value = "http://127.0.0.1:5050",
        env = "RFB_WEB_BASE_URL"
    )]
    pub base_url: String,
    /// forkd controller URL (loopback only), probed for readiness.
    #[arg(long, default_value = "http://127.0.0.1:8889", env = "FORKD_URL")]
    pub forkd_url: String,
    /// forkd snapshot tag required by `--require-vm`.
    #[arg(long, default_value = "rfb", env = "FORKD_SNAPSHOT_TAG")]
    pub tag: String,
    /// Prompt sent to every stream turn.
    #[arg(
        long,
        default_value = "Use one available read-only workspace tool, then briefly summarize the result.",
        env = "RFB_WEB_BENCH_PROMPT"
    )]
    pub prompt: String,
    /// Optional model override.
    #[arg(long, env = "RFB_WEB_BENCH_MODEL")]
    pub model: Option<String>,
    /// RFB web service process PID to sample for CPU/RSS/fd.
    #[arg(long, env = "RFB_WEB_PID")]
    pub pid: u32,
    /// Comma-separated concurrency levels from {1,2,4,8,16}.
    #[arg(long, default_value = "1,2,4,8,16", env = "RFB_WEB_BENCH_LEVELS")]
    pub levels: String,
    /// Turns per level required for a PASS.
    #[arg(long, default_value_t = 120, env = "RFB_WEB_BENCH_REQUESTS")]
    pub requests: usize,
    /// Warmup turns per level (excluded from measured aggregates).
    #[arg(long, default_value_t = 10, env = "RFB_WEB_BENCH_WARMUP")]
    pub warmup: usize,
    /// Per-turn timeout in seconds.
    #[arg(long, default_value_t = 180, env = "RFB_WEB_BENCH_TIMEOUT")]
    pub timeout: u64,
    /// Run for N seconds per level instead of `--requests` turns (0 disables).
    #[arg(long, default_value_t = 0.0, env = "RFB_WEB_BENCH_DURATION")]
    pub duration: f64,
    /// Process sampling interval in seconds (0 uses 0.1s).
    #[arg(long, default_value_t = 1.0, env = "RFB_WEB_BENCH_SAMPLE_INTERVAL")]
    pub sample_interval: f64,
    /// Require every stream to observe a tool call and a tool result.
    #[arg(long, env = "RFB_WEB_BENCH_REQUIRE_TOOL")]
    pub require_tool: bool,
    /// Output JSON report path (default: requirements/RFB/0.1.0/...).
    #[arg(long, env = "RFB_WEB_BENCH_REPORT", value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// Require a real VM/controller: run the forkd preflight gate first.
    #[arg(long)]
    pub require_vm: bool,
}
