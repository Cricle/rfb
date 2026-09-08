#![cfg(feature = "cli")]
#![allow(dead_code)]

// Forkd snapshot path-helper tests. The helpers stay private to the CLI, so
// this file includes them through a seam module whose nested `tests` module
// can see them (same pattern as `tests/zeroboot_firecracker.rs`). Seam
// contract: `snapshot_paths.rs` must only reference `crate::cli::error` and
// std; `error.rs` is included for it.

mod cli {
    pub mod error {
        include!("../src/cli/error.rs");
    }
    pub mod forkd {
        pub mod snapshot_paths {
            include!("../src/cli/forkd/snapshot_paths.rs");

            #[cfg(test)]
            mod tests {
                use super::*;
                use std::ffi::OsStr;
                use std::fs;
                use std::path::PathBuf;

                fn tmp(name: &str) -> PathBuf {
                    let dir = std::env::temp_dir()
                        .join(format!("rfb-cli-snapshot-{}-{name}", std::process::id()));
                    let _ = fs::remove_dir_all(&dir);
                    fs::create_dir_all(&dir).unwrap();
                    dir
                }

                #[test]
                fn snapshot_dir_prefers_xdg_data_home() {
                    assert_eq!(
                        forkd_snapshots_dir(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/u"))),
                        Some(PathBuf::from("/xdg/forkd/snapshots"))
                    );
                }

                #[test]
                fn snapshot_dir_falls_back_to_home_when_xdg_empty() {
                    assert_eq!(
                        forkd_snapshots_dir(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
                        Some(PathBuf::from("/home/u/.local/share/forkd/snapshots"))
                    );
                }

                #[test]
                fn snapshot_dir_returns_none_when_no_home() {
                    assert_eq!(forkd_snapshots_dir(None, None), None);
                    assert_eq!(forkd_snapshots_dir(Some(OsStr::new("")), None), None);
                }

                #[test]
                fn private_copy_uses_custom_path_verbatim() {
                    let dir = tmp("custom");
                    let src = dir.join("src.ext4");
                    fs::write(&src, b"pristine rootfs").unwrap();
                    let custom = dir.join("nested").join("boot-rootfs.ext4");

                    let copy = prepare_rootfs_private_copy(&src, "tag-x", Some(&custom)).unwrap();
                    assert_eq!(copy, custom);
                    assert_eq!(fs::read(&copy).unwrap(), b"pristine rootfs");
                    // Original is untouched and the copy is a real file, not a symlink.
                    assert_eq!(fs::read(&src).unwrap(), b"pristine rootfs");
                    assert!(custom.is_file());

                    let _ = fs::remove_dir_all(&dir);
                }

                #[test]
                fn private_copy_rejects_path_identical_to_input() {
                    let dir = tmp("samepath");
                    let src = dir.join("src.ext4");
                    fs::write(&src, b"data").unwrap();
                    let error = prepare_rootfs_private_copy(&src, "tag-x", Some(&src)).unwrap_err();
                    assert!(error.message.contains("--rootfs-copy must differ"));
                    let _ = fs::remove_dir_all(&dir);
                }
            }
        }
    }
}
