// Exit-code carrying error type shared by every `rfb-cli` subcommand. This
// file opens with plain `//` comments (no `//!` inner docs) because it is also
// pulled into `tests/cli_forkd_snapshot_paths.rs` via `include!`, which cannot
// carry inner doc comments.

use serde_json::{json, Value};

/// Error exit codes used across the CLI. Stable so scripts and CI can branch
/// on them without parsing message text.
pub const EXIT_OK: i32 = 0;
/// Exit code for invalid command-line usage (bad flags or arguments).
pub const EXIT_USAGE: i32 = 2;
/// Exit code for input that failed semantic validation.
pub const EXIT_VALIDATION: i32 = 3;
/// Exit code for an I/O failure while reading or writing files.
pub const EXIT_IO: i32 = 4;
/// Exit code for a failure in an external tool or subprocess.
pub const EXIT_EXTERNAL: i32 = 5;
/// Real VM/backend prerequisites are missing and the command required them
/// (`--require-vm`, the historical preflight gate).
pub const EXIT_NOVM: i32 = 12;

/// Build a "VM required but unavailable" error (exit 12).
pub fn no_vm(message: impl Into<String>) -> CliError {
    CliError::new(EXIT_NOVM, message)
}

/// A CLI failure carrying a stable process exit code.
#[derive(Debug)]
pub struct CliError {
    /// Stable process exit code for this failure.
    pub code: i32,
    /// Human-readable description of the failure.
    pub message: String,
}

impl CliError {
    /// Build a new error with an exit code and a message.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Shorthand for validation/usage/io/external errors.
pub fn validation(message: impl Into<String>) -> CliError {
    CliError::new(EXIT_VALIDATION, message)
}

/// Shorthand for a CLI usage error.
pub fn usage(message: impl Into<String>) -> CliError {
    CliError::new(EXIT_USAGE, message)
}

/// Shorthand for an I/O error.
pub fn io(message: impl Into<String>) -> CliError {
    CliError::new(EXIT_IO, message)
}

/// Shorthand for an external-tool/process error.
pub fn external(message: impl Into<String>) -> CliError {
    CliError::new(EXIT_EXTERNAL, message)
}

/// Render the error for human or JSON output.
pub fn render_error(json_out: bool, error: &CliError) {
    if json_out {
        println!(
            "{}",
            json!({"ok": false, "error": {"code": error.code, "message": error.message}})
        );
    } else {
        eprintln!(
            "error[{}]: {} (try 'rfb-cli --help')",
            error.code, error.message
        );
    }
}

/// Print a command result either as pretty JSON or as plain text.
pub fn render_output(json_out: bool, value: Value, text: String) {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("JSON values are serializable")
        );
    } else {
        println!("{text}");
    }
}
