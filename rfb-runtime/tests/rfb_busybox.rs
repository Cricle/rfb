//! Black-box tests for the `rfb-busybox` multi-call binary (`/bin/sh` and
//! applets in guest images). The binary is driven exactly as the guest runs
//! it: via `argv[0]` applet selection (`CARGO_BIN_EXE_rfb-busybox` + `arg0`).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
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
fn inline_hash_comment_is_stripped() {
    // POSIX: `#` after whitespace starts a comment; inside a word it is a
    // literal character.
    assert_eq!(stdout_of("echo hi # comment"), "hi\n");
    assert_eq!(stdout_of("echo a#b"), "a#b\n");
}

#[test]
fn devnull_redirect_discards_instead_of_inheriting() {
    // `> /dev/null` must give the child a real null fd. The old sentinel
    // conflated "/dev/null" with "no redirection" and inherited the captured
    // stdout — the discarded text leaked into the command's output.
    let out = sh("echo secret > /dev/null");
    assert!(
        out.stdout.is_empty(),
        "devnull-redirected stdout must be discarded, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The classic discard idiom: both streams must vanish.
    let out = sh("echo leaked > /dev/null 2>&1");
    assert!(
        out.stdout.is_empty() && out.stderr.is_empty(),
        "devnull+dup must discard both streams, got {:?}/{:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // `< /dev/null` gives an immediate EOF, so `cat` terminates.
    let out = sh("cat < /dev/null; echo done-$?");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "done-0\n");
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

#[test]
fn digit_inside_a_word_is_not_an_io_number() {
    // `echo a2>b` must print `a2` into file `b` (POSIX: a digit is an
    // IO_NUMBER only at the token's very start). The old parser broke the
    // word at `2` and misparsed it as an fd-2 redirection (`echo a 2> b`).
    let dir = temp_path("io-number-mid-word");
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("b");
    let _ = sh(&format!("cd {} && echo a2>b", dir.display()));
    let content = std::fs::read_to_string(&target).expect("redirect target b must exist");
    assert_eq!(content, "a2\n");

    // The token-start IO_NUMBER form keeps working: `2>` redirects stderr.
    let err_file = dir.join("err.log");
    let out = sh(&format!(
        "cd {} && echo ok 2>{}",
        dir.display(),
        err_file.display()
    ));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    let err = std::fs::read_to_string(&err_file).unwrap_or_default();
    assert!(err.is_empty(), "stderr redirect must create an empty file");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pipeline_stage_spawn_failure_reaps_started_stages() {
    // `sleep 97 | <unknown>` reports 127 immediately and must NOT leave the
    // already-spawned first stage running after the pipeline exits.
    let out = sh("sleep 97 | rfb-busybox-no-such-command-e2e");
    assert_eq!(out.status.code(), Some(127));

    // Give the kernel a beat to finish the reap, then scan /proc for any
    // surviving `sleep 97` the failed pipeline might have leaked.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut leaked = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => panic!("cannot scan /proc on this platform: {error}"),
    };
    for entry in entries.flatten() {
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let mut args = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty());
        // argv[0] is the resolved program path (`/usr/bin/sleep`), argv[1]
        // the operand: match the trailing basename, not the whole path.
        let is_leaked_sleep = matches!(
            (args.next(), args.next()),
            (Some(program), Some(operand))
                if program.ends_with(b"sleep") && operand == b"97"
        );
        if is_leaked_sleep {
            leaked.push(entry.path());
        }
    }
    assert!(
        leaked.is_empty(),
        "pipeline spawn failure must kill and reap started stages; leaked: {leaked:?}"
    );
}

#[test]
fn sh_file_mode_runs_a_script_file() {
    // `sh FILE`：kernel 的 shebang 机制调 `/bin/sh <脚本>`——没有这个模式
    // guest 里的 shebang 脚本永远无法执行。
    let script = temp_path("sh-file-mode.sh");
    std::fs::write(&script, "echo from-file\necho second\n").expect("write");
    let out = run_applet("sh", &[script.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-file\nsecond\n");
}

#[test]
fn shebang_script_executes_via_the_kernel() {
    // 直接 exec（kernel shebang → /bin/sh <文件>）：与 guest 里 bash 工具
    // 执行 /workspace/*.sh 的路径完全一致。
    let script = temp_path("shebang.sh");
    std::fs::write(&script, "#!/bin/sh\necho shebang-ok\n").expect("write");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let out = ProcCommand::new(&script).output().expect("exec script");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "shebang-ok\n");
}

#[test]
fn sh_missing_file_reports_127() {
    let out = run_applet("sh", &["/nonexistent/script.sh"]);
    assert_eq!(out.status.code(), Some(127));
}
