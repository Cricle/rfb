//! forkd controller + guest orchestration shared by the CLI.
//!
//! This converges the logic that used to live in `preflight.sh`,
//! `acceptance-forkd.sh`, and `benchmark-real.sh`: controller reachability and
//! snapshot readiness checks, sandbox create/destroy/ping, newline-delimited
//! JSON guest RPC, the full acceptance gate, and a sanitized microbenchmark.
//!
//! Safety invariants carried over from the scripts:
//! - the controller URL must be a loopback target ([`crate::cli::localhost`]);
//! - guest addresses always come from forkd responses, never synthesized;
//! - response payloads are parsed but never logged;
//! - cleanup is scoped to sandboxes created by the current invocation.

mod acceptance;
mod benchmark;
mod binding;
mod preflight;
mod sandbox;
mod snapshot;
mod snapshot_paths;
mod workload;

pub use acceptance::acceptance;
pub use benchmark::benchmark;
pub use binding::{load_snapshot_binding, snapshot_bind, BindingOutput};
pub use preflight::{client_from_env, preflight, preflight_checks, preflight_text, PreflightCheck};
pub use sandbox::{
    create_sandbox, destroy_sandbox, guest_call, list_sandboxes, ping_sandbox,
    wait_for_guest_ready, GUEST_READY_DEADLINE,
};
pub use snapshot::{
    find_resx, provenance_state, resolve_forkd_bin, sanitize_snapshot_info, snapshot_create,
    snapshot_delete, snapshot_info, SnapshotOutput,
};
pub use workload::workload;
