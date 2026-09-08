// forkd snapshot path helpers: snapshots data-directory resolution and the
// private rootfs copy that keeps `forkd snapshot` rw boots from mutating the
// artifact original. Extracted from `snapshot.rs` so the pure path logic can
// be exercised from `tests/cli_forkd_snapshot_paths.rs` via an `include!`
// seam. Seam contract: this file must start with plain `//` comments (no
// `//!` inner docs) and only reference `crate::cli::error` (and std); any
// other `crate::` path breaks the test shim.

use crate::cli::error::{io, validation, CliError};
use std::fs;
use std::path::{Path, PathBuf};

/// Compute the forkd snapshots data directory from environment values.
///
/// Mirrors the official `forkd` CLI: `$XDG_DATA_HOME/forkd/snapshots`, falling
/// back to `$HOME/.local/share/forkd/snapshots` when `XDG_DATA_HOME` is unset
/// or empty. Returns `None` when neither a usable XDG data home nor a home can
/// be established. Pure — testable without touching the process environment.
fn forkd_snapshots_dir(
    xdg_data_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(value) = xdg_data_home {
        if !value.is_empty() {
            return Some(PathBuf::from(value).join("forkd").join("snapshots"));
        }
    }
    let home = home?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("forkd")
            .join("snapshots"),
    )
}

/// Read the forkd snapshots data directory from the process environment.
/// Also the on-disk observation root for snapshot-bind provenance.
pub(crate) fn forkd_snapshots_root() -> Option<PathBuf> {
    forkd_snapshots_dir(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Copy `rootfs` to `target` (creating the parent directory first) and return
/// the copy path. This gives `forkd snapshot` a private, writable rootfs so its
/// rw boot never mutates the artifact original.
fn copy_rootfs_to(rootfs: &Path, target: &Path) -> Result<PathBuf, CliError> {
    let parent = target.parent().ok_or_else(|| {
        validation(format!(
            "rootfs copy has no parent directory: {}",
            target.display()
        ))
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| io(format!("create_dir_all {}: {error}", parent.display())))?;
    fs::copy(rootfs, target).map_err(|error| {
        io(format!(
            "copy rootfs {} -> {}: {error}",
            rootfs.display(),
            target.display()
        ))
    })?;
    Ok(target.to_path_buf())
}

/// Copy `rootfs` to `<snapshots_dir>/<tag>/rootfs.ext4` (creating the directory
/// first) and return the copy path.
fn copy_rootfs_private(
    rootfs: &Path,
    snapshots_dir: &Path,
    tag: &str,
) -> Result<PathBuf, CliError> {
    copy_rootfs_to(rootfs, &snapshots_dir.join(tag).join("rootfs.ext4"))
}

/// Create the private rootfs copy for `tag` and return the path forkd should
/// boot from. `explicit` (from `--rootfs-copy`/`FORKD_ROOTFS_COPY`) is used
/// verbatim when given; otherwise the copy lives inside the forkd snapshots
/// data directory (see [`forkd_snapshots_root`]). Fails closed when no data
/// home can be resolved, so the artifact original is never handed to a rw boot
/// unknowingly.
pub(crate) fn prepare_rootfs_private_copy(
    rootfs: &Path,
    tag: &str,
    explicit: Option<&Path>,
) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        if path == rootfs {
            return Err(validation(
                "--rootfs-copy must differ from the input rootfs path",
            ));
        }
        return copy_rootfs_to(rootfs, path);
    }
    let Some(snapshots) = forkd_snapshots_root() else {
        return Err(validation(
            "cannot create private rootfs copy: neither XDG_DATA_HOME nor HOME is set",
        ));
    };
    copy_rootfs_private(rootfs, &snapshots, tag)
}
