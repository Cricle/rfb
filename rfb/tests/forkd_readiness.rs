#![cfg(feature = "forkd")]

use rfb::forkd::SnapshotInfo;

#[test]
fn snapshot_readiness_requires_matching_tag_ready_status_and_bootable() {
    let snapshots = [
        SnapshotInfo {
            tag: "target".into(),
            dir: String::new(),
            created_at_unix: None,
            branched_from: None,
            pause_ms: None,
            diff_ms: None,
            diff_physical_bytes: None,
            diff_logical_bytes: None,
            warning: None,
            status: "READY".into(),
            bootable: false,
            digest: None,
            provenance: None,
        },
        SnapshotInfo {
            tag: "other".into(),
            dir: String::new(),
            created_at_unix: None,
            branched_from: None,
            pause_ms: None,
            diff_ms: None,
            diff_physical_bytes: None,
            diff_logical_bytes: None,
            warning: None,
            status: "ready".into(),
            bootable: true,
            digest: None,
            provenance: None,
        },
    ];
    assert!(!snapshots
        .iter()
        .any(|s| s.tag == "target" && s.status.eq_ignore_ascii_case("ready") && s.bootable));

    let mut target = snapshots[0].clone();
    target.bootable = true;
    assert!(target.status.eq_ignore_ascii_case("ready") && target.bootable);
}
