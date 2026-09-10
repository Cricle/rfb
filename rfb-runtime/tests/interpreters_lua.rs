//! Integration tests for the embedded `lua` multi-call entry. They spawn the
//! runtime binary with `argv[0]` overridden to `lua`, replicating the
//! `/bin/lua` hardlink exec performed by ZBRT `Execute`.

#![cfg(all(target_os = "linux", feature = "mlua"))]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn lua() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rfb-runtime"));
    // Replicates the hardlink exec: argv[0] is the applet name, not the path.
    cmd.arg0("lua");
    cmd
}

fn run_code(args: &[&str]) -> (Option<i32>, String, String) {
    let output = lua()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn rfb-runtime as lua");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn executes_dash_e_and_captures_stdout() {
    let (code, stdout, stderr) = run_code(&["-e", "print('rfb-lua-ok')"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("rfb-lua-ok"), "stdout: {stdout}");
}

#[test]
fn reads_chunk_from_stdin() {
    let mut child = lua()
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn lua stdin mode");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(b"print(6 * 7)\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("42"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn executes_chunk_file() {
    let dir = std::env::temp_dir().join("rfb-lua-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rfb_chunk_{}.lua", std::process::id()));
    std::fs::write(&path, "print('from-file')").unwrap();
    let (code, stdout, stderr) = run_code(&[path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("from-file"), "stdout: {stdout}");
}

#[test]
fn runtime_error_yields_exit_code_one() {
    let (code, _stdout, stderr) = run_code(&["-e", "error('rfb-boom')"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("rfb-boom"), "stderr: {stderr}");
}

#[test]
fn syntax_error_yields_exit_code_one() {
    let (code, _stdout, stderr) = run_code(&["-e", "function ("]);
    assert_eq!(code, Some(1));
    assert!(!stderr.is_empty());
}

#[test]
fn package_path_points_at_offline_module_root() {
    let (code, stdout, stderr) = run_code(&[
        "-e",
        "print(package.path:find('/usr/lib/lua/5.4', 1, true) ~= nil)",
    ]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("true"),
        "package.path missing offline root: {stdout}"
    );
}

#[test]
fn usage_errors_exit_with_code_two() {
    let (code, _stdout, stderr) = run_code(&["-e"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("usage: lua"), "stderr: {stderr}");
}

#[test]
fn missing_chunk_file_yields_exit_code_one() {
    let (code, _stdout, stderr) = run_code(&["/nonexistent/rfb_probe.lua"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("cannot open"), "stderr: {stderr}");
}
