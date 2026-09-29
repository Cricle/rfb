//! Path policy tests: workspace confinement, read-only host roots, and the
//! failure modes (escape / not-allowed / read-only) the executor relies on.

use rfb_runtime::policy::{PathPolicy, PolicyError};
use std::path::PathBuf;

fn policy() -> PathPolicy {
    PathPolicy::new("/vm/workspace", vec!["/host/sources".into()])
}

#[test]
fn workspace_path_accepts_relative_and_nested_paths() {
    let p = policy();
    assert_eq!(
        p.workspace_path("src/main.rs").unwrap(),
        PathBuf::from("/vm/workspace/src/main.rs")
    );
    assert_eq!(
        p.workspace_path("a/b/c/d.txt").unwrap(),
        PathBuf::from("/vm/workspace/a/b/c/d.txt")
    );
    assert_eq!(
        p.workspace_path(".").unwrap(),
        PathBuf::from("/vm/workspace")
    );
}

#[test]
fn workspace_path_rejects_parent_components_and_mixed_escapes() {
    let p = policy();
    for escape in ["..", "../etc", "a/../../etc", "sub/../..", "a/b/../../.."] {
        assert!(
            matches!(p.workspace_path(escape), Err(PolicyError::Escape)),
            "{escape:?} must be rejected as an escape"
        );
    }
}

#[test]
fn workspace_path_rejects_absolute_paths() {
    let p = policy();
    for absolute in ["/etc/passwd", "/vm/workspace2/file", "//"] {
        assert!(
            matches!(p.workspace_path(absolute), Err(PolicyError::Escape)),
            "{absolute:?} must be rejected"
        );
    }
}

#[test]
fn empty_relative_path_resolves_to_the_workspace_root() {
    let p = policy();
    assert_eq!(
        p.workspace_path("").unwrap(),
        PathBuf::from("/vm/workspace")
    );
}

#[test]
fn workspace_does_not_leak_into_other_roots() {
    // A nested path that merely *starts* with the workspace prefix but escapes
    // past its boundary is still confined.
    let p = PathPolicy::new("/vm/workspace", vec![PathBuf::from("/vm")]);
    assert_eq!(
        p.workspace_path("file.txt").unwrap(),
        PathBuf::from("/vm/workspace/file.txt")
    );
    // /vm/workspace2 is a sibling, not inside the workspace root.
    let p2 = PathPolicy::new("/vm/workspace", Vec::new());
    assert!(matches!(
        p2.workspace_path("/vm/workspace2/x"),
        Err(PolicyError::Escape)
    ));
}

#[test]
fn host_read_path_resolves_inside_the_requested_root() {
    let p = policy();
    assert_eq!(
        p.host_read_path(0, "README.md").unwrap(),
        PathBuf::from("/host/sources/README.md")
    );
    assert_eq!(
        p.host_read_path(0, "nested/file.toml").unwrap(),
        PathBuf::from("/host/sources/nested/file.toml")
    );
}

#[test]
fn host_read_path_rejects_out_of_range_roots() {
    let p = policy();
    assert!(matches!(
        p.host_read_path(1, "README"),
        Err(PolicyError::NotAllowed)
    ));
    assert!(matches!(
        p.host_read_path(100, "README"),
        Err(PolicyError::NotAllowed)
    ));
    assert!(matches!(
        p.host_read_path(0, "../secret"),
        Err(PolicyError::Escape)
    ));
    assert!(matches!(
        p.host_read_path(0, "a/../../secret"),
        Err(PolicyError::Escape)
    ));
}

#[test]
fn host_paths_with_no_read_only_roots_are_never_allowed() {
    let p = PathPolicy::new("/vm/workspace", Vec::new());
    assert!(matches!(
        p.host_read_path(0, "README"),
        Err(PolicyError::NotAllowed)
    ));
}

#[test]
fn can_write_host_is_always_read_only() {
    let p = policy();
    for path in ["README.md", "/host/sources/README.md", "anything"] {
        assert!(matches!(p.can_write_host(path), Err(PolicyError::ReadOnly)));
    }
}

#[test]
fn policy_error_messages_are_stable() {
    assert_eq!(PolicyError::Escape.to_string(), "path escapes allowed root");
    assert_eq!(
        PolicyError::ReadOnly.to_string(),
        "write is not allowed for read-only host path"
    );
    assert_eq!(
        PolicyError::NotAllowed.to_string(),
        "path is not inside an allowed root"
    );
}

#[test]
fn policy_fields_are_publically_inspectable() {
    let p = policy();
    assert_eq!(p.workspace_root, PathBuf::from("/vm/workspace"));
    assert_eq!(p.read_only_host_roots, vec![PathBuf::from("/host/sources")]);
}

#[cfg(unix)]
#[test]
fn workspace_path_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;
    let base = std::env::temp_dir().join(format!("rfb-policy-symlink-{}", std::process::id()));
    let workspace = base.join("workspace");
    let secret = base.join("secret");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("key"), b"leaked").unwrap();
    // Guest created a symlink inside the workspace pointing outside.
    symlink(&secret, workspace.join("link")).unwrap();
    // Also a symlinked FILE inside the workspace.
    symlink(secret.join("key"), workspace.join("key-link")).unwrap();

    let p = PathPolicy::new(&workspace, Vec::new());
    // Reading through the directory symlink must fail closed.
    assert!(matches!(
        p.workspace_path("link/key"),
        Err(PolicyError::Escape)
    ));
    // Reading through the file symlink must fail closed too.
    assert!(matches!(
        p.workspace_path("key-link"),
        Err(PolicyError::Escape)
    ));
    // A plain existing file inside the workspace still resolves.
    std::fs::write(workspace.join("ok.txt"), b"fine").unwrap();
    assert!(p.workspace_path("ok.txt").is_ok());
    // A not-yet-existing file under an existing directory still resolves
    // (writes must keep working).
    assert!(p.workspace_path("sub/new.txt").is_ok());
    let _ = std::fs::remove_dir_all(&base);
}
