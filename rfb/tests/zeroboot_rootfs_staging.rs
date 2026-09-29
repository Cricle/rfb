#![cfg(all(feature = "zeroboot", target_os = "linux"))]

//! Rootfs staging contract: every boot/restore gets a private image copy so
//! guest rw writes never reach the artifact image (or a sibling's copy).

use rfb::zeroboot::stage_private_rootfs;

#[test]
fn stages_a_private_copy_inside_the_work_dir() {
    let work = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let source_path = source.path().join("image.ext4");
    std::fs::write(&source_path, b"rootfs-bytes").unwrap();

    let staged =
        stage_private_rootfs(work.path().to_str().unwrap(), source_path.to_str().unwrap()).unwrap();

    assert!(staged.starts_with(work.path().to_str().unwrap()));
    assert_eq!(std::fs::read(&staged).unwrap(), b"rootfs-bytes");

    // Mutating the staged copy never reaches the shared source image.
    std::fs::write(&staged, b"mutated").unwrap();
    assert_eq!(std::fs::read(&source_path).unwrap(), b"rootfs-bytes");
}
