//! `rfb-cli` shared library: one place for tool discovery, host detection,
//! localhost policy, error rendering, and sanitized reporting.
//!
//! The binary (`src/bin/rfb-cli.rs`) is a thin command router over these
//! modules, so every subcommand (doctor, env, image, forkd, rfb1, cleanup,
//! bench) gets identical diagnostics and reporting instead of per-script
//! heuristics.

pub mod cleanup;
pub mod commands;
pub mod dispatch;
// `error.rs` opens with plain `//` comments because it is also `include!`d
// into a test that cannot carry inner doc comments; allow the missing module
// docs here.
#[allow(missing_docs)]
pub mod error;
pub mod forkd;
pub mod host;
pub mod image_build;
pub mod localhost;
pub mod report;
#[cfg(unix)]
pub mod rfb1;
pub mod skills;
pub mod tool;
#[cfg(unix)]
pub mod web_bench;
#[cfg(unix)]
pub mod zeroboot;
