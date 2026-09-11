//! Black-box tests for the `rfb-busybox` multi-call binary (`/bin/sh` and
//! applets in guest images). The binary is driven exactly as the guest runs
//! it: via `argv[0]` applet selection (`CARGO_BIN_EXE_rfb-busybox` + `arg0`).

#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::process::Command as ProcCommand;

/// The real busybox binary: `cargo test` exports `CARGO_BIN_EXE_*` for
/// integration tests and builds the bin before running them.
fn busybox_bin() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rfb-busybox") {
        return std::path::PathBuf::from(path);
    }
    let exe = std::env::current_exe().expect("current exe");
    let deps = exe.parent().expect("deps dir");
    let target = deps.parent().expect("target dir");
    target.join("rfb-busybox")
}

fn run_applet(name: &str, args: &[&str]) -> std::process::Output {
    let mut command = ProcCommand::new(busybox_bin());
    command.arg0(name);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("spawn applet")
}

fn sh(script: &str) -> std::process::Output {
    run_applet("sh", &["-c", script])
}

fn stdout_of(script: &str) -> String {
    let out = sh(script);
    assert!(
        out.status.success(),
        "script failed: {script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rfb-busybox-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
}

#[test]
fn echo_builtin_and_quotes() {
    assert_eq!(stdout_of("echo hello world"), "hello world\n");
    assert_eq!(stdout_of("echo 'a  b' \"c  d\""), "a  b c  d\n");
    assert_eq!(stdout_of("echo -n no-newline"), "no-newline");
    assert_eq!(stdout_of("echo 'literal $?'"), "literal $?\n");
}

#[test]
fn sequence_and_exit_status() {
    assert!(sh("false; true").status.success());
    assert_eq!(sh("true; false").status.code(), Some(1));
    assert_eq!(sh("exit 7").status.code(), Some(7));
    assert_eq!(sh("true; exit 9").status.code(), Some(9));
}

#[test]
fn conditional_chains_are_left_associative() {
    assert_eq!(stdout_of("false || echo rescued"), "rescued\n");
    assert_eq!(stdout_of("true && echo gated"), "gated\n");
    let out = sh("false && echo hidden");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    // ((false || echo a) && echo b): both echo.
    assert_eq!(stdout_of("false || echo a && echo b"), "a\nb\n");
}

#[test]
fn dollar_question_expands_previous_status() {
    assert_eq!(stdout_of("false; echo $?"), "1\n");
    assert_eq!(stdout_of("true; echo $?"), "0\n");
}

#[test]
fn stderr_redirect_and_merge_into_file() {
    let out = sh("echo err >&2");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    assert!(out.stdout.is_empty());
    let file = temp_path("merged.txt");
    // `2>&1` before `> file`: stderr joins stdout's inherited stream,
    // then stdout moves to the file — POSIX order semantics.
    let _ = sh(&format!("echo out > {} 2>&1", file.display()));
    let merged = std::fs::read_to_string(&file).unwrap_or_default();
    assert!(
        merged.contains("out"),
        "stdout must reach the file: {merged:?}"
    );
}

#[test]
fn redirection_creates_and_appends_files() {
    let file = temp_path("log.txt");
    let _ = sh(&format!("echo one > {}", file.display()));
    let _ = sh(&format!("echo two >> {}", file.display()));
    let content = std::fs::read_to_string(&file).expect("log content");
    assert_eq!(content, "one\ntwo\n");
}

#[test]
fn pipelines_connect_stages() {
    let out = sh("echo piped | cat");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "piped\n");
    let out = sh("echo piped | cat | cat");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "piped\n");
}

#[test]
fn stderr_does_not_flow_into_pipeline() {
    let out = sh("echo err >&2 | cat");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    let out = sh("echo err 2>&1 | cat");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "err\n");
}

#[test]
fn unknown_command_reports_127() {
    let out = sh("definitely-not-a-command-e2e");
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}

#[test]
fn syntax_errors_report_2() {
    assert_eq!(sh("echo 'unterminated").status.code(), Some(2));
    assert_eq!(sh("echo a &&").status.code(), Some(2));
    assert_eq!(sh("|").status.code(), Some(2));
    assert_eq!(sh("echo a &&& echo b").status.code(), Some(2));
}

#[test]
fn exit_code_of_spawned_program_propagates() {
    assert_eq!(sh("/bin/false").status.code(), Some(1));
    assert_eq!(sh("/bin/true").status.code(), Some(0));
    assert_eq!(sh("/bin/sh -c 'exit 3'").status.code(), Some(3));
}

#[test]
fn cd_changes_directory_for_later_segments() {
    let out = sh("cd /; pwd");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/\n");
    let out = sh("cd /definitely-missing-dir-e2e");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn sleep_applet_validates_operand() {
    let out = run_applet("sleep", &["nonsense"]);
    assert_eq!(out.status.code(), Some(1));
    let started = std::time::Instant::now();
    let out = run_applet("sleep", &["0.05"]);
    assert!(out.status.success());
    assert!(started.elapsed() >= std::time::Duration::from_millis(40));
}

#[test]
fn unknown_applet_reports_127() {
    let out = run_applet("definitely-not-an-applet", &[]);
    assert_eq!(out.status.code(), Some(127));
}
