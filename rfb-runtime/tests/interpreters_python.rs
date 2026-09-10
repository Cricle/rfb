//! Integration tests for the embedded `python3` multi-call entry. They spawn
//! the runtime binary with `argv[0]` overridden to `python3`, replicating the
//! `/bin/python3` hardlink exec performed by ZBRT `Execute`.

#![cfg(all(target_os = "linux", feature = "rustpython"))]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn python3() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rfb-runtime"));
    // Replicates the hardlink exec: argv[0] is the applet name, not the path.
    cmd.arg0("python3");
    cmd
}

fn run_code(args: &[&str]) -> (Option<i32>, String, String) {
    let output = python3()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn rfb-runtime as python3");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn executes_dash_c_and_captures_stdout() {
    let (code, stdout, stderr) = run_code(&["-c", "print('rfb-python-ok')"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("rfb-python-ok"), "stdout: {stdout}");
}

#[test]
fn reads_script_from_stdin() {
    let mut child = python3()
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn python3 stdin mode");
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
fn executes_script_file() {
    let dir = std::env::temp_dir().join("rfb-python3-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rfb_script_{}.py", std::process::id()));
    std::fs::write(&path, "print('from-file')").unwrap();
    let (code, stdout, stderr) = run_code(&[path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("from-file"), "stdout: {stdout}");
}

#[test]
fn uncaught_exception_yields_exit_code_one() {
    let (code, _stdout, stderr) = run_code(&["-c", "raise ValueError('rfb-boom')"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("ValueError"), "stderr: {stderr}");
}

#[test]
fn syntax_error_yields_exit_code_one() {
    let (code, _stdout, stderr) = run_code(&["-c", "def ("]);
    assert_eq!(code, Some(1));
    assert!(!stderr.is_empty());
}

#[test]
fn frozen_stdlib_import_works() {
    let (code, stdout, stderr) = run_code(&["-c", "import json, re, pathlib\nprint('imports-ok')"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("imports-ok"), "stdout: {stdout}");
}

#[test]
fn site_packages_dir_is_on_sys_path() {
    let (code, stdout, stderr) = run_code(&[
        "-c",
        "import sys\nprint(any('/usr/lib/python3/site-packages' in p for p in sys.path))",
    ]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("True"),
        "site-packages missing from sys.path: {stdout}"
    );
}

#[test]
fn usage_errors_exit_with_code_two() {
    let (code, _stdout, stderr) = run_code(&["-c"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("usage: python3"), "stderr: {stderr}");
}

#[test]
fn missing_script_file_yields_exit_code_one() {
    let (code, _stdout, stderr) = run_code(&["/nonexistent/rfb_probe.py"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("cannot open"), "stderr: {stderr}");
}
