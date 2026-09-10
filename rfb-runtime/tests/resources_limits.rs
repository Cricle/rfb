//! Runtime limits invariants (moved out of `src/resources.rs` per the
//! tests-folder gate: `scripts/check-tests-folder.sh`).

use rfb_runtime::resources::{RuntimeLimits, WORKSPACE_TMPFS_BYTES};

#[test]
fn default_workspace_limit_matches_guest_tmpfs() {
    // The executor limit must not exceed the kernel-enforced tmpfs cap,
    // otherwise writes die with raw ENOSPC instead of a policy error.
    assert_eq!(
        RuntimeLimits::default().max_workspace_bytes,
        WORKSPACE_TMPFS_BYTES
    );
    assert_eq!(WORKSPACE_TMPFS_BYTES, 256 * 1024 * 1024);
}
