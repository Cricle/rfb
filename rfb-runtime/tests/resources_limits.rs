//! Runtime limits invariants (moved out of `src/resources.rs` per the
//! tests-folder gate: `scripts/check-tests-folder.sh`).

use rfb_runtime::resources::{
    derive_workspace_bytes, workspace_tmpfs_bytes, RuntimeLimits, WORKSPACE_TMPFS_BYTES,
    WORKSPACE_TMPFS_MAX_BYTES, WORKSPACE_TMPFS_MIN_BYTES,
};

#[test]
fn default_workspace_limit_matches_guest_tmpfs() {
    // The executor limit and the tmpfs the guest actually mounts derive from
    // the same source (40% of MemTotal), so the limit can never exceed the
    // kernel-enforced tmpfs cap — otherwise writes die with raw ENOSPC
    // instead of a policy error.
    assert_eq!(
        RuntimeLimits::default().max_workspace_bytes,
        workspace_tmpfs_bytes()
    );
    // The constant is only the non-Linux/unreadable-/proc fallback; it must
    // stay inside the derived clamp range.
    assert_eq!(WORKSPACE_TMPFS_BYTES, 256 * 1024 * 1024);
    const {
        assert!(WORKSPACE_TMPFS_BYTES >= WORKSPACE_TMPFS_MIN_BYTES);
        assert!(WORKSPACE_TMPFS_BYTES <= WORKSPACE_TMPFS_MAX_BYTES);
    }
}

#[test]
fn workspace_tmpfs_derivation_is_40_percent_clamped_to_whole_mib() {
    // 2 GiB MemTotal -> 40% = 819.2 MiB, floored to whole MiB.
    assert_eq!(
        derive_workspace_bytes(2 * 1024 * 1024 * 1024),
        819 * 1024 * 1024
    );
    // 4 GiB MemTotal -> 40% = 1.6 GiB, clamped to the 1 GiB ceiling.
    assert_eq!(
        derive_workspace_bytes(4 * 1024 * 1024 * 1024),
        WORKSPACE_TMPFS_MAX_BYTES
    );
    // 128 MiB MemTotal -> 40% = 51.2 MiB, clamped to the 64 MiB floor.
    assert_eq!(
        derive_workspace_bytes(128 * 1024 * 1024),
        WORKSPACE_TMPFS_MIN_BYTES
    );
    // Exactly on the floor.
    assert_eq!(
        derive_workspace_bytes(160 * 1024 * 1024),
        WORKSPACE_TMPFS_MIN_BYTES
    );
    // Degenerate/unknown memory still yields a usable workspace.
    assert_eq!(derive_workspace_bytes(0), WORKSPACE_TMPFS_MIN_BYTES);
}
