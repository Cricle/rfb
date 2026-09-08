//! Real RFB web service concurrency and resource sampler.
//!
//! Converges `rfb-runtime/scripts/rfb-web-concurrency.sh` into `rfb-cli` so
//! the last shell script can be deleted. This is a black-box HTTP/SSE sampler
//! for an already-running RFB web service instance: it drives concurrent
//! `POST /api/ai/rfb/stream` turns, samples the RFB web service process's CPU/RSS/fd
//! counts while they run, and reports sanitized quantiles. It never starts
//! RFB web service, never mocks a backend, and never logs payloads or credentials.
//!
//! Only Linux is supported (the sampler reads `/proc/<pid>`); on other hosts
//! the `web bench` subcommand is absent, matching the script's Linux/WSL scope.

mod aggregate;
pub mod args;
mod run;
mod types;
mod util;

pub use args::WebBenchArgs;
pub use run::bench;

/// Sampling interval for a value of `0` (the script's 0.1s default).
pub fn effective_interval(interval: f64) -> f64 {
    if interval > 0.0 {
        interval
    } else {
        0.1
    }
}
