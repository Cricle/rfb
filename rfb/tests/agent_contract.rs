//! Agent-token contract tests: the REAL Rust guest client
//! (`rfb::forkd_guest::ForkdGuestClient`) drives the in-process agent
//! (sources included via `#[path]`). Lives in `rfb/tests/` because the
//! client side is the SDK crate — a dev-dependency in the other direction
//! would violate the runtime-must-not-depend-on-sdk boundary
//! (scripts/check-boundary.sh).
#![cfg(feature = "forkd")]

#[allow(dead_code)]
#[path = "../../rfb-runtime/src/agent/mod.rs"]
mod agent;
// agent/builtin.rs re-exports `crate::builtin`; include the same source under
// the same path so that import resolves inside this test crate too.
#[allow(dead_code)]
#[path = "../../rfb-runtime/src/builtin.rs"]
mod builtin;
// agent/search.rs resolves find patterns via `crate::glob`; include the same
// source under the same path so that import resolves inside this test crate.
#[allow(dead_code)]
#[path = "../../rfb-runtime/src/glob.rs"]
mod glob;
// agent/stream.rs decodes live output chunks via `crate::utf8_boundary`.
#[allow(dead_code)]
#[path = "../../rfb-runtime/src/utf8_boundary.rs"]
mod utf8_boundary;
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use rfb::forkd_guest::{ForkdGuestClient, ForkdGuestError};

/// Connect with retries on `PermissionDenied`. On Windows the ephemeral
/// source-port allocator can sweep into a Hyper-V excluded range, failing
/// otherwise-valid loopback connects with WSAEACCES (10013) in bursts; a
/// retry allocates a different source port.
async fn connect_retry(addr: &'static str) -> TcpStream {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => return stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => panic!("connect {addr}: {error}"),
        }
    }
}

/// Spawn the agent against a temporary workspace: CI runners run as a
/// non-root user that cannot create `/workspace`, so every test points
/// `RFB_AGENT_WORKSPACE` at one shared temp dir (initialized once).
///
/// Agents are pinned to open access via `run_with_token(.., None)` so ambient
/// or parallel `FORKD_AGENT_TOKEN` mutations (see the auth round-trip tests
/// below) can never flip an unrelated contract test into token-gated mode.
fn spawn_agent(addr: &'static str) -> tokio::task::JoinHandle<std::io::Result<()>> {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = std::env::temp_dir().join("rfb-agent-contract-tests");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("RFB_AGENT_WORKSPACE", &dir);
    });
    tokio::spawn(agent::run_with_token(addr, None))
}

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
    let main = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../rfb-runtime/src/main.rs"),
    )
    .unwrap();
    assert!(main.contains("Some(\"forkd-agent\")"));
    assert!(main.contains("forkd-init.sh"));
    assert!(main.contains("RFB_RUNTIME_MODE"));
    assert!(main.contains("agent::run"));
    assert!(main.contains("Some(\"vsock\")"));
}

#[tokio::test]
async fn ping_and_unknown_actions_are_ndjson_responses() {
    let task = spawn_agent("127.0.0.1:18888");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18888").await;
    assert_eq!(
        request(&mut stream, json!({"action":"ping"})).await["pong"],
        true
    );
    let error = request(&mut stream, json!({"action":"not-a-real-action"})).await;
    assert!(error["error"].as_str().unwrap().contains("unknown action"));
    task.abort();
}

/// Real find contract over the agent NDJSON wire (PROTOCOL.md §2.5a): find
/// semantics are glob *name* matching with a single source across every
/// transport/backend — a pattern without `*` is a full-name exact match,
/// never a substring contains() ("note" must not hit note.txt).
#[tokio::test]
async fn find_uses_glob_name_matching_not_substring() {
    // Seed the shared agent workspace with the probe file.
    let dir = std::env::temp_dir().join("rfb-agent-contract-tests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), b"x").unwrap();
    let task = spawn_agent("127.0.0.1:18911");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18911").await;
    let literal = request(
        &mut stream,
        json!({"action":"find","path":".","pattern":"note","max_results":100}),
    )
    .await;
    assert!(
        literal["matches"].as_array().is_some_and(|m| m.is_empty()),
        "literal 'note' must not substring-match note.txt: {literal}"
    );
    let exact = request(
        &mut stream,
        json!({"action":"find","path":".","pattern":"note.txt","max_results":100}),
    )
    .await;
    assert_eq!(exact["matches"], json!(["note.txt"]), "got {exact}");
    let globbed = request(
        &mut stream,
        json!({"action":"find","path":".","pattern":"*.txt","max_results":100}),
    )
    .await;
    let matches = globbed["matches"].as_array().expect("find matches");
    assert!(
        matches.iter().any(|m| m.as_str() == Some("note.txt")),
        "glob *.txt must match note.txt: {globbed}"
    );
    task.abort();
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
    let task = spawn_agent("127.0.0.1:18892");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18892").await;
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
    let task = spawn_agent("127.0.0.1:18895");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18895").await;
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
    let task = spawn_agent("127.0.0.1:18896");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = connect_retry("127.0.0.1:18896").await;
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
    let task = spawn_agent("127.0.0.1:18894");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = connect_retry("127.0.0.1:18894").await;
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
    let task = spawn_agent("127.0.0.1:18897");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = connect_retry("127.0.0.1:18897").await;
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
    let task = spawn_agent("127.0.0.1:18893");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = connect_retry("127.0.0.1:18893").await;
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
    assert_eq!(terminal["done"], true);
    // PROTOCOL.md §2.4: a timeout is a normal terminal outcome — a string
    // `error` key would make the host raise Remote instead of exposing
    // `timed_out` to callers.
    assert!(
        terminal.get("error").is_none(),
        "timeout frames must not carry the error key: {terminal}"
    );
    task.abort();
}

#[tokio::test]
async fn exec_cwd_rejects_workspace_escape() {
    let task = spawn_agent("127.0.0.1:18891");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18891").await;
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
    let task = spawn_agent("127.0.0.1:18889");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18889").await;
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
    let task = spawn_agent("127.0.0.1:18887");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18887").await;
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
    let task = spawn_agent("127.0.0.1:18901");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18901").await;
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
    let task = spawn_agent("127.0.0.1:18902");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18902").await;
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
/// carry `pong`, `numpy_version`, `pid`, `agent_lang`, `warmup_ready`, `path`,
/// and the additive `protocol_version` marker. The Rust replacement reports
/// honest values for fields only the Python/Node interpreter can produce.
#[tokio::test]
async fn ping_golden_contract_matches_official_field_set() {
    let task = spawn_agent("127.0.0.1:18903");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18903").await;
    let ping = request(&mut stream, json!({"action":"ping"})).await;
    assert_eq!(ping["pong"], true);
    assert_eq!(ping["numpy_version"], "not-installed");
    assert_eq!(ping["agent_lang"], "rust");
    assert_eq!(ping["warmup_ready"], false);
    assert_eq!(ping["protocol_version"], 1);
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
    // The field set is exactly the official six plus the additive
    // `protocol_version` marker (no silent additions).
    let golden = json!({
        "pong": true,
        "numpy_version": "not-installed",
        "pid": ping["pid"].clone(),
        "agent_lang": "rust",
        "warmup_ready": false,
        "path": ping["path"].clone(),
        "protocol_version": 1,
    });
    assert_eq!(ping, golden);
    task.abort();
}

/// Official forkd exec response contract: `stdout`/`stderr` must be provided
/// (the Python agent's field names) in addition to the RFB-internal `out`/`err`
/// aliases, along with `exit_code`, `error`, and the terminal `timed_out` flag.
#[tokio::test]
async fn exec_builtin_golden_response_matches_frozen_contract() {
    let task = spawn_agent("127.0.0.1:18904");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18904").await;
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
    let task = spawn_agent("127.0.0.1:18905");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18905").await;
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
    let task = spawn_agent("127.0.0.1:18906");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = connect_retry("127.0.0.1:18906").await;
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
    let task = spawn_agent("127.0.0.1:18907");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = connect_retry("127.0.0.1:18907").await;
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
    let task = spawn_agent("127.0.0.1:18908");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18908").await;
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
    let task = spawn_agent("127.0.0.1:18909");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let mut stream = connect_retry("127.0.0.1:18909").await;
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
    // PROTOCOL.md §2.4: no string `error` on the timeout terminal — the host
    // classifies any `error` as a fatal Remote failure and would swallow
    // `timed_out`.
    assert!(
        result.get("error").is_none(),
        "timeout response must not carry the error key: {result}"
    );
    task.abort();
}

/// P1-2 (agent side): live stream output chunks are decoded with incremental
/// UTF-8 boundary handling — a multi-byte character straddling an 8 KiB pipe
/// read must not surface as U+FFFD in the concatenated stream.
#[cfg(unix)]
#[tokio::test]
async fn stream_multibyte_output_never_splits_across_8kib_chunks() {
    let task = spawn_agent("127.0.0.1:18910");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let stream = connect_retry("127.0.0.1:18910").await;
    let (mut read, mut write) = stream.into_split();
    let script = "i=0; while [ $i -lt 4095 ]; do printf 'é'; i=$((i+1)); done; i=0; while [ $i -lt 2000 ]; do printf '你'; i=$((i+1)); done";
    write
        .write_all(
            serde_json::to_string(&json!({
                "action":"stream", "args":["/bin/sh", "-c", script]
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
    let mut output = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let frame: Value = serde_json::from_str(line.trim()).unwrap();
        if frame.get("exit_code").is_some() {
            assert_eq!(frame["exit_code"], 0);
            break;
        }
        if let Some(chunk) = frame.get("out").and_then(Value::as_str) {
            output.push_str(chunk);
        }
    }
    let expected = format!("{}{}", "é".repeat(4095), "你".repeat(2000));
    assert_eq!(
        output, expected,
        "live stream must be byte-identical to the child output"
    );
    assert!(
        !output.contains('\u{FFFD}'),
        "a chunk boundary must not split a codepoint"
    );
    task.abort();
}

/// P1-9: when the agent enforces the agent token, the Rust guest client must
/// complete the auth handshake before its first business frame. The agent is
/// spawned with an explicit token via `run_with_token` (never through the
/// env-reading `run`), so only the client side consults `FORKD_AGENT_TOKEN` —
/// mirroring production, where the token reaches the agent through its own
/// deployment env.
static TOKEN_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_agent_token(token: Option<&str>) {
    match token {
        Some(token) => std::env::set_var(rfb::forkd_guest::AGENT_TOKEN_ENV, token),
        None => std::env::remove_var(rfb::forkd_guest::AGENT_TOKEN_ENV),
    }
}

#[tokio::test]
async fn guest_client_completes_agent_token_auth_and_execs() {
    let _guard = TOKEN_ENV_LOCK.lock().await;
    let task = tokio::spawn(agent::run_with_token(
        "127.0.0.1:18912",
        Some("contract-token"),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let client = ForkdGuestClient::new("127.0.0.1:18912");
    set_agent_token(Some("contract-token"));
    let result = client
        .exec_in(
            "/workspace",
            vec!["/missing/bin/echo".into(), "hello".into(), "world".into()],
            10,
        )
        .await;
    set_agent_token(None);
    let result = result.unwrap();
    assert_eq!(result["out"], "hello world\n");
    assert_eq!(result["exit_code"], 0);
    task.abort();
}

/// The stream path runs the same per-connection handshake before the business
/// frame, and the buffered reader built during auth is carried into the
/// stream (no bytes lost between the handshake and the started event).
#[tokio::test]
async fn guest_client_stream_completes_agent_token_auth() {
    let _guard = TOKEN_ENV_LOCK.lock().await;
    let task = tokio::spawn(agent::run_with_token(
        "127.0.0.1:18913",
        Some("contract-token"),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let client = ForkdGuestClient::new("127.0.0.1:18913");
    set_agent_token(Some("contract-token"));
    let mut stream = client
        .stream(
            vec!["/no/such/echo".into(), "auth-stream".into()],
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let mut output = String::new();
    let mut exited = false;
    while let Some(event) = stream.next_event().await.unwrap() {
        if let Some(chunk) = event.get("out").and_then(Value::as_str) {
            output.push_str(chunk);
        }
        if event.get("exit_code").is_some() {
            exited = true;
            break;
        }
    }
    set_agent_token(None);
    assert_eq!(output, "auth-stream\n");
    assert!(exited, "stream must reach a terminal exit frame");
    task.abort();
}

/// Rejections map onto the client's remote-error classification, matching the
/// Python SDK's `guest agent auth failed` wording: a missing token and a wrong
/// token both surface as `ForkdGuestError::Remote`, not a transport failure.
#[tokio::test]
async fn guest_client_auth_rejections_map_to_remote_errors() {
    let _guard = TOKEN_ENV_LOCK.lock().await;
    let task = tokio::spawn(agent::run_with_token(
        "127.0.0.1:18914",
        Some("contract-token"),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let client = ForkdGuestClient::new("127.0.0.1:18914");
    // No token configured (clear any ambient value first): the client sends
    // no auth frame, and the agent rejects the ping with its own error —
    // surfaced through the normal remote mapping.
    set_agent_token(None);
    let missing = client.ping().await.unwrap_err();
    assert!(
        matches!(missing, ForkdGuestError::Remote(ref message)
            if message.contains("authentication required")),
        "missing token must surface the agent's auth rejection: {missing}"
    );
    // Wrong token: the agent answers {"action":"auth","ok":false,...}.
    set_agent_token(Some("wrong-token"));
    let wrong = client.ping().await.unwrap_err();
    set_agent_token(None);
    assert!(
        matches!(wrong, ForkdGuestError::Remote(ref message)
            if message == "guest agent auth failed: authentication failed"),
        "wrong token must be a remote auth failure: {wrong}"
    );
    task.abort();
}
