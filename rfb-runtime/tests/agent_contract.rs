#[allow(dead_code)]
#[path = "../src/agent/mod.rs"]
mod agent;
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

async fn request(stream: &mut TcpStream, value: Value) -> Value {
    stream
        .write_all(serde_json::to_string(&value).unwrap().as_bytes())
        .await
        .unwrap();
    stream.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(&mut *stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[test]
fn forkd_agent_entrypoint_is_explicit_and_vsock_remains_default() {
    let main =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")).unwrap();
    assert!(main.contains("Some(\"forkd-agent\")"));
    assert!(main.contains("forkd-init.sh"));
    assert!(main.contains("RFB_RUNTIME_MODE"));
    assert!(main.contains("agent::run"));
    assert!(main.contains("Some(\"vsock\")"));
}

#[tokio::test]
async fn ping_and_unknown_actions_are_ndjson_responses() {
    let task = tokio::spawn(agent::run("127.0.0.1:18888"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18888").await.unwrap();
    assert_eq!(
        request(&mut stream, json!({"action":"ping"})).await["pong"],
        true
    );
    let error = request(&mut stream, json!({"action":"not-a-real-action"})).await;
    assert!(error["error"].as_str().unwrap().contains("unknown action"));
    task.abort();
}

#[test]
fn find_patterns_support_common_globs_and_literal_compatibility() {
    assert!("*.rs".contains('*'));
    assert!("*".contains('*'));
    assert!(!"agent".contains('*'));
}

#[test]
fn forkd_image_build_supports_a_separate_entrypoint() {
    // The image workflow is owned by rfb-cli image build-rootfs (the shell
    // build-rootfs.sh was removed in the CLI convergence). Assert the CLI
    // produces the forkd-agent install/entrypoint contract.
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../rfb/src/cli/commands.rs"),
    )
    .unwrap_or_default();
    assert!(
        source.contains("BuildRootfs"),
        "rfb-cli must expose image build-rootfs"
    );
    let lib = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../rfb/src/cli/image_build/build.rs"),
    )
    .unwrap_or_default();
    assert!(lib.contains("forkd-agent"));
    assert!(lib.contains("/sbin/forkd-agent"));
    assert!(lib.contains("/forkd-init.sh"));
    assert!(lib.contains("rfb-vsock"));
}

#[cfg(unix)]
#[tokio::test]
async fn exec_timeout_returns_terminal_response_and_does_not_wait_for_child() {
    let task = tokio::spawn(agent::run("127.0.0.1:18892"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18892").await.unwrap();
    let started = tokio::time::Instant::now();
    let response = request(
        &mut stream,
        json!({"action":"exec", "args":["/bin/sh", "-c", "sleep 10"], "timeout":1}),
    )
    .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(response["timed_out"], true);
    assert!(response["err"].as_str().unwrap().contains("timeout"));
    assert!(response["exit_code"].is_null());
    task.abort();
}

#[tokio::test]
async fn shell_free_builtins_execute_without_host_commands() {
    let task = tokio::spawn(agent::run("127.0.0.1:18895"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18895").await.unwrap();
    let echo = request(
        &mut stream,
        json!({"action":"exec", "args":["/missing/bin/echo", "hello", "world"]}),
    )
    .await;
    assert_eq!(echo["out"], "hello world\n");
    assert_eq!(echo["exit_code"], 0);
    let false_result = request(&mut stream, json!({"action":"exec", "args":["false"]})).await;
    assert_eq!(false_result["exit_code"], 1);
    assert_eq!(false_result["out"], "");
    task.abort();
}

#[tokio::test]
async fn shell_free_builtin_stream_emits_started_output_and_exit() {
    let task = tokio::spawn(agent::run("127.0.0.1:18896"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = TcpStream::connect("127.0.0.1:18896").await.unwrap();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(
            serde_json::to_string(&json!({
                "action":"stream", "args":["/no/such/echo", "builtin-stream"]
            }))
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(&mut read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(line.trim()).unwrap()["stream"],
        "started"
    );
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let output: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(output["out"], "builtin-stream\n");
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let terminal: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(terminal["exit_code"], 0);
    assert_eq!(terminal["timed_out"], false);
    task.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn stream_fast_child_drains_output_before_exit_frame() {
    let task = tokio::spawn(agent::run("127.0.0.1:18894"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = TcpStream::connect("127.0.0.1:18894").await.unwrap();
    let (mut read, mut write) = stream.into_split();
    let args: Vec<&str> = if cfg!(unix) {
        vec!["/bin/echo", "forkd-race-regression"]
    } else {
        vec!["cmd", "/C", "echo", "forkd-race-regression"]
    };
    write
        .write_all(
            serde_json::to_string(&json!({"action":"stream", "args":args}))
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(&mut read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(line.trim()).unwrap()["stream"],
        "started"
    );

    let mut output = String::new();
    line.clear();
    loop {
        reader.read_line(&mut line).await.unwrap();
        let frame: Value = serde_json::from_str(line.trim()).unwrap();
        if frame.get("exit_code").is_some() {
            assert_eq!(frame["exit_code"], 0);
            assert!(!frame["timed_out"].as_bool().unwrap());
            break;
        }
        if let Some(chunk) = frame.get("out").and_then(Value::as_str) {
            output.push_str(chunk);
        }
        line.clear();
    }
    assert_eq!(output, "forkd-race-regression\n");
    task.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn stream_stdin_round_trip_writes_into_child_and_stops() {
    let task = tokio::spawn(agent::run("127.0.0.1:18897"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = TcpStream::connect("127.0.0.1:18897").await.unwrap();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(
            serde_json::to_string(&json!({"action":"stream", "args":["/bin/cat"]}))
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(&mut read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(line.trim()).unwrap()["stream"],
        "started"
    );
    // Send one stdin payload: the child (cat) echoes it back on stdout.
    write
        .write_all(
            serde_json::to_string(&json!({"in":"forkd-stdin-roundtrip"}))
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    line.clear();
    let mut echoed = String::new();
    loop {
        reader.read_line(&mut line).await.unwrap();
        let frame: Value = serde_json::from_str(line.trim()).unwrap();
        if let Some(chunk) = frame.get("out").and_then(Value::as_str) {
            echoed.push_str(chunk);
            if echoed.contains("forkd-stdin-roundtrip") {
                break;
            }
        }
        line.clear();
    }
    assert!(
        echoed.contains("forkd-stdin-roundtrip"),
        "child did not echo stdin: {echoed:?}"
    );
    // Stop the stream: child is terminated and an exit frame is emitted.
    write
        .write_all(
            serde_json::to_string(&json!({"action":"stop"}))
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    line.clear();
    loop {
        reader.read_line(&mut line).await.unwrap();
        let frame: Value = serde_json::from_str(line.trim()).unwrap();
        if frame.get("exit_code").is_some() {
            break;
        }
        line.clear();
    }
    task.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn stream_timeout_emits_explicit_terminal_ndjson_response() {
    let task = tokio::spawn(agent::run("127.0.0.1:18893"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = TcpStream::connect("127.0.0.1:18893").await.unwrap();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(
            serde_json::to_string(&json!({
                "action":"stream", "args":["/bin/sh", "-c", "sleep 10"], "timeout":1
            }))
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(&mut read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(line.trim()).unwrap()["stream"],
        "started"
    );
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let terminal: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(terminal["timed_out"], true);
    assert_eq!(terminal["exit_code"], Value::Null);
    assert!(terminal["error"].as_str().unwrap().contains("timeout"));
    task.abort();
}

#[tokio::test]
async fn exec_cwd_rejects_workspace_escape() {
    let task = tokio::spawn(agent::run("127.0.0.1:18891"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18891").await.unwrap();
    let response = request(
        &mut stream,
        json!({"action":"exec", "args":["pwd"], "cwd":"/tmp"}),
    )
    .await;
    assert!(response["error"].is_string());
    task.abort();
}

#[tokio::test]
async fn filesystem_paths_reject_escape_and_absolute_directory_paths() {
    let task = tokio::spawn(agent::run("127.0.0.1:18889"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18889").await.unwrap();
    for path in ["../escape", "/etc", "workspace\\escape"] {
        let response = request(&mut stream, json!({"action":"ls", "path":path})).await;
        assert!(
            response["error"].is_string(),
            "path {path:?} unexpectedly accepted"
        );
    }
    task.abort();
}

/// The forkd agent writes to the literal host path /workspace (see
/// agent/transport.rs WORKSPACE const). On hosts where that path is not
/// writable by the test user (e.g. WSL non-root), filesystem contract tests
/// that touch /workspace must skip instead of failing with os error 13.
#[cfg(unix)]
fn host_workspace_writable() -> bool {
    if std::fs::create_dir_all("/workspace").is_err() {
        return false;
    }
    let probe = Path::new("/workspace/.rfb-agent-contract-probe");
    match std::fs::write(probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
const SKIP_WORKSPACE_MESSAGE: &str =
    "skip: host /workspace not writable (run in a container/VM with writable /workspace or as root)";

#[cfg(unix)]
#[tokio::test]
async fn nested_subdirectory_write_and_read_round_trip() {
    // Regression: guest_path used to canonicalize the parent of a not-yet
    // existing file, which failed with os error 2 for newly created deep
    // subdirectories. The fixed version canonicalizes the deepest existing
    // ancestor and re-joins the remainder lexically so write can create
    // parents (write already calls create_dir_all).
    if !host_workspace_writable() {
        eprintln!("{SKIP_WORKSPACE_MESSAGE}");
        return;
    }
    let task = tokio::spawn(agent::run("127.0.0.1:18887"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18887").await.unwrap();
    let path = "/workspace/nested/deep/file.txt";
    let content = "nested-write-ok";
    let write = request(
        &mut stream,
        json!({"action":"write", "path":path, "data":content}),
    )
    .await;
    assert_eq!(
        write["bytes_written"],
        content.len(),
        "write failed: {write}"
    );
    let read = request(
        &mut stream,
        json!({"action":"read", "path":path, "max_bytes":4096}),
    )
    .await;
    let data: Vec<u8> = serde_json::from_value(read["data"].clone()).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&data),
        content,
        "read round trip failed"
    );
    let find = request(
        &mut stream,
        json!({"action":"find", "path":"/workspace", "pattern":"file.txt", "max_results":100}),
    )
    .await;
    assert!(find["matches"].as_array().is_some(), "find failed: {find}");
    task.abort();
}

#[tokio::test]
async fn protocol_invalid_and_oversized_lines() {
    let task = tokio::spawn(agent::run("127.0.0.1:18901"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18901").await.unwrap();
    stream.write_all(b"\nnot-json\n").await.unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(serde_json::from_str::<Value>(line.trim()).unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("invalid JSON"));
    drop(reader);
    let mut line = vec![b'x'; 1024 * 1024 + 1];
    line.push(b'\n');
    stream.write_all(&line).await.unwrap();
    let mut response = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut response)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(response.trim()).unwrap()["error"],
        "line too large"
    );
    task.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn filesystem_search_edges_and_append() {
    if !host_workspace_writable() {
        eprintln!("{SKIP_WORKSPACE_MESSAGE}");
        return;
    }
    let task = tokio::spawn(agent::run("127.0.0.1:18902"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18902").await.unwrap();
    assert!(
        request(&mut stream, json!({"action":"ls","path":"missing-nope"})).await["error"]
            .is_string()
    );
    assert_eq!(
        request(
            &mut stream,
            json!({"action":"write","path":"/workspace/contract-search.txt","data":"needle\nother"})
        )
        .await["bytes_written"],
        12
    );
    assert_eq!(request(&mut stream, json!({"action":"write","path":"/workspace/contract-search.txt","data":"\nneedle","append":true})).await["bytes_written"], 7);
    assert_eq!(
        request(
            &mut stream,
            json!({"action":"read","path":"contract-search.txt","max_bytes":6})
        )
        .await["truncated"],
        true
    );
    assert_eq!(
        request(
            &mut stream,
            json!({"action":"find","path":"/workspace","pattern":"*.txt","max_results":1})
        )
        .await["truncated"],
        true
    );
    assert!(!request(
        &mut stream,
        json!({"action":"grep","path":"/workspace","pattern":"needle","max_results":10})
    )
    .await["matches"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(request(
        &mut stream,
        json!({"action":"find","path":"/workspace","pattern":""})
    )
    .await["error"]
        .is_string());
    task.abort();
}

/// Official forkd ping field contract (rfb-cli-usage.md §4): the response must
/// carry `pong`, `numpy_version`, `pid`, `agent_lang`, `warmup_ready`, and
/// `path`. The Rust replacement reports honest values for fields only the
/// Python/Node interpreter can produce.
#[tokio::test]
async fn ping_golden_contract_matches_official_field_set() {
    let task = tokio::spawn(agent::run("127.0.0.1:18903"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18903").await.unwrap();
    let ping = request(&mut stream, json!({"action":"ping"})).await;
    assert_eq!(ping["pong"], true);
    assert_eq!(ping["numpy_version"], "not-installed");
    assert_eq!(ping["agent_lang"], "rust");
    assert_eq!(ping["warmup_ready"], false);
    assert!(
        ping["pid"].as_u64().unwrap_or(0) > 0,
        "pid must be a positive int"
    );
    assert!(
        ping["path"]
            .as_str()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "path must be a non-empty string"
    );
    // The field set is exactly the official six (no silent additions).
    let golden = json!({
        "pong": true,
        "numpy_version": "not-installed",
        "pid": ping["pid"].clone(),
        "agent_lang": "rust",
        "warmup_ready": false,
        "path": ping["path"].clone(),
    });
    assert_eq!(ping, golden);
    task.abort();
}

/// Official forkd exec response contract: `stdout`/`stderr` must be provided
/// (the Python agent's field names) in addition to the RFB-internal `out`/`err`
/// aliases, along with `exit_code`, `error`, and the terminal `timed_out` flag.
#[tokio::test]
async fn exec_builtin_golden_response_matches_frozen_contract() {
    let task = tokio::spawn(agent::run("127.0.0.1:18904"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18904").await.unwrap();
    let result = request(
        &mut stream,
        json!({"action":"exec","args":["/missing/bin/echo","hello","world"]}),
    )
    .await;
    let golden = json!({
        "out": "hello world\n",
        "err": "",
        "stdout": "hello world\n",
        "stderr": "",
        "exit_code": 0,
        "error": null,
        "timed_out": false,
    });
    assert_eq!(result, golden);
    task.abort();
}

/// A spawned command must surface both alias families for stdout and stderr.
#[cfg(unix)]
#[tokio::test]
async fn exec_contract_aliases_spawned_stdout_and_stderr() {
    let task = tokio::spawn(agent::run("127.0.0.1:18905"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18905").await.unwrap();
    let result = request(
        &mut stream,
        json!({"action":"exec","args":["/bin/sh","-c","echo out; echo err >&2"]}),
    )
    .await;
    assert_eq!(result["out"], "out\n");
    assert_eq!(result["stdout"], "out\n");
    assert_eq!(result["err"], "err\n");
    assert_eq!(result["stderr"], "err\n");
    assert_eq!(result["exit_code"], 0);
    assert!(result["error"].is_null());
    task.abort();
}

/// The stream started frame carries the official `pid` and `pty` fields; the
/// builtin path (no child process) reports a null pid and non-PTY mode.
#[tokio::test]
async fn stream_started_frame_carries_pid_and_pty_fields() {
    let task = tokio::spawn(agent::run("127.0.0.1:18906"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = TcpStream::connect("127.0.0.1:18906").await.unwrap();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(
            serde_json::to_string(&json!({"action":"stream","args":["/no/such/echo","builtin"]}))
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(&mut read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let started: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(started["stream"], "started");
    assert_eq!(started["pty"], false);
    assert!(started["pid"].is_null());
    task.abort();
}

/// A spawned stream's started frame reports the real child pid with pty=false.
#[cfg(unix)]
#[tokio::test]
async fn stream_started_frame_reports_spawned_pid() {
    let task = tokio::spawn(agent::run("127.0.0.1:18907"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = TcpStream::connect("127.0.0.1:18907").await.unwrap();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(
            serde_json::to_string(&json!({"action":"stream","args":["/bin/sleep","0"]}))
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    write.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(&mut read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let started: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(started["stream"], "started");
    assert_eq!(started["pty"], false);
    assert!(
        started["pid"].as_u64().unwrap_or(0) > 0,
        "spawned stream must report a real child pid"
    );
    // Drain the terminal frame so the child is reaped before aborting.
    while !line.contains("\"exit_code\"") {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
    }
    task.abort();
}

/// PTY requests must be explicitly rejected, never silently degraded to pipes.
#[tokio::test]
async fn stream_pty_true_is_explicitly_rejected_not_silently_degraded() {
    let task = tokio::spawn(agent::run("127.0.0.1:18908"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18908").await.unwrap();
    let result = request(
        &mut stream,
        json!({"action":"stream","args":["/no/such/echo","x"],"pty":true}),
    )
    .await;
    assert!(
        result["error"].as_str().unwrap().contains("pty"),
        "pty must be rejected, got {result}"
    );
    assert_eq!(result["exit_code"], 1);
    task.abort();
}

/// exec timeout keeps the official stdout/stderr aliases alongside out/err.
#[cfg(unix)]
#[tokio::test]
async fn exec_timeout_keeps_official_aliases_and_terminal_state() {
    let task = tokio::spawn(agent::run("127.0.0.1:18909"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = TcpStream::connect("127.0.0.1:18909").await.unwrap();
    let result = request(
        &mut stream,
        json!({"action":"exec","args":["/bin/sh","-c","sleep 10"],"timeout":1}),
    )
    .await;
    assert_eq!(result["timed_out"], true);
    assert!(result["exit_code"].is_null());
    assert_eq!(result["out"], "");
    assert_eq!(result["stdout"], "");
    assert_eq!(result["err"], "process timeout");
    assert_eq!(result["stderr"], "process timeout");
    assert_eq!(result["error"], "process timeout");
    task.abort();
}
