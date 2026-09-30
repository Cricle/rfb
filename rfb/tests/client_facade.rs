#![cfg(all(feature = "forkd", feature = "zeroboot"))]
//! Unified facade acceptance tests (`sdk/UNIFIED_API.md` §9).
//!
//! All fake servers run on plain `std::net` + `std::thread` (blocking I/O):
//! on some Windows environments a tokio task parked in a pending overlapped
//! accept/read prevents IOCP completions from reaching the client runtime in
//! the same process, wedging even explicit timeouts. Blocking server threads
//! avoid overlapped server-side I/O entirely and keep the tests deterministic.
//!
//! Covers: fake forkd controller (hand-written HTTP), fake NDJSON guest, fake
//! ZBRT frame server; controller surface (incl. invalid base_url → Validation
//! and blank `FORKD_URL` → default), NDJSON multi-line + stream sessions, ZBRT
//! Hello-first handshake / control-connection reuse / stale reconnect / exec /
//! fs / stream / cancel / health, identical facade shapes on both transports,
//! and local validation fail-closed rules (incl. ZBRT argc > 255).

mod common;

use common::http::blocking::spawn_controller;
use common::ndjson::blocking::spawn_guest;
use common::zbrt::{spawn_zbrt, zcancelack, zerror, zexit, zfsresult, zhealthack, zout, FakeZbrt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rfb::client::{
    CreateOptions, DirEntry, GuestSandbox, GuestTransport, RfbClient, RfbError, StreamEventKind,
};
use rfb::protocol::{Fs, Kind, ZBRT_V1_CAPABILITIES};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fake forkd controller / guest / ZBRT servers live in `common::{http, ndjson,
// zbrt}` (blocking families; see the module docs for the Windows IOCP note).
// ---------------------------------------------------------------------------

fn sandbox_list_json(guest_addr: &str) -> String {
    json!([{
        "id": "sb1",
        "snapshot_tag": "snap",
        "netns": null,
        "created_at_unix": null,
        "guest_addr": guest_addr,
        "memory_limit_mib": null,
        "pid": null,
        "has_branched": false,
        "branch_count": 0
    }])
    .to_string()
}

/// Controller + sandbox facade pointed at `guest_addr` on the given transport.
async fn connect_fake(guest_addr: &str, transport: GuestTransport) -> (RfbClient, GuestSandbox) {
    let addr = guest_addr.to_owned();
    let controller = spawn_controller(move |req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/sandboxes") => (200, sandbox_list_json(&addr)),
        ("DELETE", "/v1/sandboxes/sb1") => (200, "{}".to_string()),
        _ => (404, "{\"error\":\"not found\"}".to_string()),
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5))
        .expect("client");
    let sandbox = client
        .connect_id_with("sb1", transport)
        .await
        .expect("connect");
    (client, sandbox)
}

// ---------------------------------------------------------------------------
// Controller tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn controller_list_and_create() {
    let captured = Arc::new(Mutex::new(None::<Value>));
    let capture = captured.clone();
    let controller = spawn_controller(move |req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/snapshots") => (
            200,
            "[{\"tag\":\"t1\",\"status\":\"ready\",\"bootable\":true}]".to_string(),
        ),
        ("POST", "/v1/sandboxes") => {
            *capture.lock().unwrap() = Some(req.body.clone());
            (200, sandbox_list_json("127.0.0.1:9"))
        }
        _ => (404, "{\"error\":\"not found\"}".to_string()),
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5))
        .expect("client");

    let snapshots = client.list_snapshots().await.unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].tag, "t1");
    assert_eq!(snapshots[0].status, "ready");
    assert!(snapshots[0].bootable);

    let options = CreateOptions {
        n: 2,
        per_child_netns: true,
        memory_limit_mib: Some(512),
        prewarm: true,
        live_fork: false,
        hugepages: true,
        transport: GuestTransport::Ndjson,
    };
    let sandboxes = client.create_sandbox("t1", options).await.unwrap();
    assert_eq!(sandboxes.len(), 1);
    assert_eq!(sandboxes[0].id(), "sb1");
    assert_eq!(sandboxes[0].snapshot_tag(), "snap");
    assert_eq!(sandboxes[0].guest_addr(), "127.0.0.1:9");

    let body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(body["snapshot_tag"], "t1");
    assert_eq!(body["n"], 2);
    assert_eq!(body["per_child_netns"], true);
    assert_eq!(body["memory_limit_mib"], 512);
    assert_eq!(body["prewarm"], true);
    assert_eq!(body["live_fork"], false);
    assert_eq!(body["hugepages"], true);
}

#[tokio::test]
async fn controller_snapshot_info_fallback_chain() {
    let legacy_hits = Arc::new(AtomicUsize::new(0));
    let hits = legacy_hits.clone();
    let controller = spawn_controller(move |req| match req.path.as_str() {
        "/v1/snapshots/old/info" => (404, "{\"error\":\"missing\"}".to_string()),
        "/v1/snapshots/old" => {
            hits.fetch_add(1, Ordering::SeqCst);
            (
                200,
                "{\"tag\":\"old\",\"status\":\"ready\",\"bootable\":true}".to_string(),
            )
        }
        "/v1/snapshots/ghost/info" | "/v1/snapshots/ghost" => {
            (404, "{\"error\":\"missing\"}".to_string())
        }
        _ => (404, "{\"error\":\"not found\"}".to_string()),
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5))
        .expect("client");

    let snapshot = client.snapshot("old").await.unwrap().expect("found");
    assert_eq!(snapshot.tag, "old");
    assert_eq!(
        legacy_hits.load(Ordering::SeqCst),
        1,
        "legacy fallback used"
    );

    assert!(
        client.snapshot("ghost").await.unwrap().is_none(),
        "double 404 = null"
    );
}

#[tokio::test]
async fn controller_error_mapping_and_bearer() {
    let controller = spawn_controller(|req| {
        assert_eq!(
            req.authorization.as_deref(),
            Some("Bearer tok"),
            "bearer header"
        );
        (500, "{\"error\":\"boom\"}".to_string())
    });
    let client = RfbClient::new(
        format!("http://{controller}"),
        Some("tok".to_owned()),
        Duration::from_secs(5),
    )
    .expect("client");
    let err = client.list_snapshots().await.unwrap_err();
    match err {
        RfbError::Http { status, message } => {
            assert_eq!(status, 500);
            assert_eq!(message, "boom");
        }
        other => panic!("expected Http error, got {other:?}"),
    }

    let controller = spawn_controller(|req| {
        assert!(req.authorization.is_none(), "no bearer without token");
        (500, "plain text".to_string())
    });
    let client =
        RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5)).unwrap();
    let err = client.list_snapshots().await.unwrap_err();
    match err {
        RfbError::Http { message, .. } => assert_eq!(message, "plain text"),
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[tokio::test]
async fn controller_delete_404_success_ping_and_ids() {
    let controller = spawn_controller(|req| match (req.method.as_str(), req.path.as_str()) {
        ("DELETE", "/v1/sandboxes/sb-1") => (404, "{\"error\":\"gone\"}".to_string()),
        ("POST", "/v1/sandboxes/sb-1/ping") => (200, "{\"alive\":true}".to_string()),
        _ => (500, "{\"error\":\"unexpected\"}".to_string()),
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5))
        .expect("client");

    // 404 on delete is success.
    client.delete_sandbox("sb-1").await.unwrap();
    // Ping returns the controller JSON value unchanged.
    let pong = client.ping_sandbox("sb-1").await.unwrap();
    assert_eq!(pong["alive"], true);
    // Invalid ids fail closed with Validation.
    assert!(matches!(
        client.ping_sandbox("bad id!").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        client.delete_sandbox("../etc").await,
        Err(RfbError::Validation(_))
    ));
}

#[tokio::test]
async fn wait_snapshot_ready_failed_and_timeout() {
    let state = Arc::new(AtomicUsize::new(0));
    let state2 = state.clone();
    let controller = spawn_controller(move |req| {
        assert_eq!(req.path, "/v1/snapshots");
        let n = state2.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            (
                200,
                "[{\"tag\":\"t\",\"status\":\"building\",\"bootable\":false}]".to_string(),
            )
        } else {
            (
                200,
                "[{\"tag\":\"t\",\"status\":\"ready\",\"bootable\":true}]".to_string(),
            )
        }
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5))
        .expect("client");
    let snapshot = client.wait_snapshot("t", 5).await.unwrap();
    assert_eq!(snapshot.status, "ready");
    assert!(snapshot.bootable);

    let controller = spawn_controller(|_| {
        (
            200,
            "[{\"tag\":\"t\",\"status\":\"failed\",\"bootable\":false}]".to_string(),
        )
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5))
        .expect("client");
    assert!(matches!(
        client.wait_snapshot("t", 5).await,
        Err(RfbError::Remote(_))
    ));

    let controller = spawn_controller(|_| {
        (
            200,
            "[{\"tag\":\"t\",\"status\":\"building\",\"bootable\":false}]".to_string(),
        )
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(5))
        .expect("client");
    // Deadline elapses while the controller keeps reporting `building`.
    assert!(matches!(
        client.wait_snapshot("t", 1).await,
        Err(RfbError::Transport(_))
    ));
}

#[test]
fn client_new_rejects_invalid_base_url_as_validation() {
    // Fail-closed before any request is built: non-URL, non-http(s) scheme,
    // missing host, and blank values are all Validation (UNIFIED_API.md §2).
    for bad in ["", "not a url", "ftp://controller", "http://"] {
        let err = match RfbClient::new(bad, None, Duration::from_secs(5)) {
            Err(err) => err,
            Ok(_) => panic!("expected Validation for base_url `{bad}`"),
        };
        assert!(
            matches!(err, RfbError::Validation(_)),
            "expected Validation for base_url `{bad}`, got {err:?}"
        );
    }
}

#[test]
fn client_from_env_blank_forkd_url_falls_back_to_default() {
    // No other test in this binary reads FORKD_URL/FORKD_TOKEN, so mutating
    // the process environment here stays race-free.
    std::env::set_var("FORKD_URL", "   ");
    std::env::set_var("FORKD_TOKEN", "tok");
    let result = RfbClient::from_env();
    std::env::remove_var("FORKD_URL");
    std::env::remove_var("FORKD_TOKEN");
    // A blank FORKD_URL falls back to the default (matching the other
    // language SDKs) instead of failing URL validation.
    result.expect("blank FORKD_URL must fall back to the default URL");
}

// ---------------------------------------------------------------------------
// Guest NDJSON tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn guest_exec_reads_lines_until_terminal() {
    let guest = spawn_guest(|conn| {
        let action = conn.recv().expect("exec request");
        assert_eq!(action["action"], "exec");
        assert_eq!(action["cwd"], "/");
        assert_eq!(action["args"], json!(["echo", "hi"]));
        // Non-terminal line first: the client must keep reading.
        conn.send(&json!({"stdout": "ignored"}));
        conn.send(&json!({"exit_code": 2, "out": "done", "timed_out": false}));
    });
    let (_client, sandbox) = connect_fake(&guest, GuestTransport::Ndjson).await;
    let result = sandbox.exec(&["echo", "hi"], "/", 60.0, b"").await.unwrap();
    assert_eq!(result.exit_code, 2);
    assert_eq!(result.stdout, b"done");
    assert_eq!(result.stdout_text(), "done");
    assert!(result.stderr.is_empty());
    assert!(!result.timed_out);
}

#[tokio::test]
async fn guest_error_line_raises_remote() {
    let guest = spawn_guest(|conn| {
        let _action = conn.recv().expect("exec request");
        conn.send(&json!({"error": "boom"}));
    });
    let (_client, sandbox) = connect_fake(&guest, GuestTransport::Ndjson).await;
    let err = sandbox.exec(&["false"], "/", 60.0, b"").await.unwrap_err();
    match err {
        RfbError::Remote(message) => assert_eq!(message, "boom"),
        other => panic!("expected Remote error, got {other:?}"),
    }
}

#[tokio::test]
async fn guest_stream_session_events_input_stop() {
    let guest = spawn_guest(|conn| {
        let action = conn.recv().expect("stream request");
        assert_eq!(action["action"], "stream");
        assert_eq!(action["args"], json!(["tail", "-f"]));
        conn.send(&json!({"started": true}));
        conn.send(&json!({"stdout": "a"}));
        let input = conn.recv().expect("stdin line");
        assert_eq!(input["in"], "x");
        conn.send(&json!({"stdout": "ax"}));
        let stop = conn.recv().expect("stop line");
        assert_eq!(stop["action"], "stop");
        conn.send(&json!({"exit_code": 0}));
    });
    let (_client, sandbox) = connect_fake(&guest, GuestTransport::Ndjson).await;
    let mut stream = sandbox
        .stream(&["tail", "-f"], Some("/"), Some(false), None)
        .await
        .unwrap();

    let started = stream.next_event().await.unwrap().unwrap();
    assert_eq!(started.kind, StreamEventKind::Started);
    let chunk = stream.next_event().await.unwrap().unwrap();
    assert_eq!(chunk.kind, StreamEventKind::Stdout);
    assert_eq!(chunk.data, b"a");
    stream.send_input("x").await.unwrap();
    let chunk = stream.next_event().await.unwrap().unwrap();
    assert_eq!(chunk.kind, StreamEventKind::Stdout);
    assert_eq!(chunk.data, b"ax");
    stream.stop().await.unwrap();
    let exit = stream.next_event().await.unwrap().unwrap();
    assert_eq!(exit.kind, StreamEventKind::Exit);
    assert_eq!(exit.code, Some(0));
    // Clean close after the terminal event.
    assert!(stream.next_event().await.unwrap().is_none());
    // send_input after terminal raises Remote; stop is idempotent.
    assert!(matches!(
        stream.send_input("y").await,
        Err(RfbError::Remote(_))
    ));
    stream.stop().await.unwrap();
}

#[tokio::test]
async fn guest_validation_fails_closed_before_send() {
    // The fake guest answers nothing; any wire traffic would hang the client
    // op until its timeout, so a fast Validation error proves the request
    // never left the client.
    let guest = spawn_guest(|_conn| {});
    let (_client, sandbox) = connect_fake(&guest, GuestTransport::Ndjson).await;

    assert!(matches!(
        sandbox.exec(&[] as &[&str], "/", 60.0, b"").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.exec(&["ls"], "C:\\x", 60.0, b"").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.exec(&["ls"], "/", 0.0, b"").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.exec(&["ls"], "/", -1.0, b"").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.ls("../etc").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(sandbox.ls("").await, Err(RfbError::Validation(_))));
    assert!(matches!(
        sandbox.find(".", "").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.grep(".", "").await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.eval("   ", None, None).await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.eval("1", None, Some(0.0)).await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.read("a.txt", None, Some(0)).await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.write("a.txt", &[0u8; 51201], false, None).await,
        Err(RfbError::Validation(_))
    ));
}

// ---------------------------------------------------------------------------
// Facade over both transports: identical shapes
// ---------------------------------------------------------------------------

fn expected_entries() -> Vec<DirEntry> {
    vec![
        DirEntry {
            name: "a.txt".to_owned(),
            is_dir: false,
            size: Some(3),
        },
        DirEntry {
            name: "sub".to_owned(),
            is_dir: true,
            size: None,
        },
    ]
}

#[tokio::test]
async fn facade_ndjson_identical_shapes() {
    let guest = spawn_guest(|conn| {
        let Some(action) = conn.recv() else { return };
        let reply = match action["action"].as_str() {
            Some("ping") => json!({"pong": true}),
            Some("exec") => json!({"exit_code": 0, "out": "hi", "timed_out": false}),
            Some("eval") => json!({"output": [104, 105], "status": 0, "timed_out": false}),
            Some("ls") => json!({
                "entries": [
                    {"name": "a.txt", "is_dir": false, "size": 3},
                    {"name": "sub", "is_dir": true}
                ],
                "truncated": false
            }),
            Some("find") => json!({"matches": ["a.txt"], "truncated": false}),
            Some("grep") => json!({
                "matches": [{"path": "a.txt", "line": 1, "text": "hi"}],
                "truncated": false
            }),
            Some("read") => json!({"data": [104, 105], "truncated": false, "total_bytes": 2}),
            Some("write") => json!({"bytes_written": 2}),
            _ => json!({"error": "unknown action"}),
        };
        conn.send(&reply);
    });
    let (client, sandbox) = connect_fake(&guest, GuestTransport::Ndjson).await;
    assert_eq!(sandbox.transport(), GuestTransport::Ndjson);

    assert!(sandbox.ping().await.unwrap());
    let exec = sandbox.exec(&["echo", "hi"], "/", 60.0, b"").await.unwrap();
    assert_eq!(exec.exit_code, 0);
    assert_eq!(exec.stdout, b"hi");
    assert!(!exec.timed_out);
    // eval output maps to stdout.
    let eval = sandbox.eval("1+1", None, None).await.unwrap();
    assert_eq!(eval.exit_code, 0);
    assert_eq!(eval.stdout, b"hi");
    assert!(eval.stderr.is_empty());
    assert_eq!(sandbox.ls(".").await.unwrap(), expected_entries());
    assert_eq!(sandbox.find(".", "*").await.unwrap(), vec!["a.txt"]);
    let matches = sandbox.grep(".", "hi").await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "a.txt");
    assert_eq!(matches[0].line, Some(1));
    assert_eq!(matches[0].column, None);
    assert_eq!(matches[0].text, "hi");
    let file = sandbox.read("a.txt", None, None).await.unwrap();
    assert_eq!(file.data, b"hi");
    assert!(!file.truncated);
    assert_eq!(file.total_bytes, Some(2));
    assert_eq!(sandbox.write("a.txt", b"hi", false, None).await.unwrap(), 2);
    sandbox.delete().await.unwrap();

    // connect() accepts the sandbox facade and the id string alike.
    let attached = client.connect(&sandbox).await.unwrap();
    assert_eq!(attached.id(), "sb1");
    let by_id = client.connect("sb1").await.unwrap();
    assert_eq!(by_id.id(), "sb1");
}

#[tokio::test]
async fn facade_zbrt_identical_shapes() {
    let server = spawn_zbrt(move |frame| {
        let rid = frame.request_id;
        match frame.kind {
            Kind::Execute => vec![zout(rid, 0, b"hi"), zexit(rid, 0)],
            Kind::Fs => {
                let fs = Fs::decode(&frame.payload).expect("Fs payload");
                let result = match fs.op {
                    1 => json!({
                        "entries": [
                            {"name": "a.txt", "is_dir": false, "size": 3},
                            {"name": "sub", "is_dir": true}
                        ],
                        "truncated": false
                    }),
                    2 => json!({"matches": ["a.txt"], "truncated": false}),
                    3 => json!({
                        "matches": [{"path": "a.txt", "line": 1, "text": "hi"}],
                        "truncated": false
                    }),
                    4 => json!({"data": [104, 105], "truncated": false, "total_bytes": 2}),
                    5 => json!({"bytes_written": 2}),
                    _ => {
                        return vec![zerror(rid, 2, "unsupported op")];
                    }
                };
                vec![zfsresult(rid, result)]
            }
            Kind::Health => vec![zhealthack(rid)],
            _ => vec![zerror(rid, 1, "unexpected kind")],
        }
    });
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;
    assert_eq!(sandbox.transport(), GuestTransport::Zbrt);

    assert!(sandbox.ping().await.unwrap());
    let exec = sandbox.exec(&["echo", "hi"], "/", 60.0, b"").await.unwrap();
    assert_eq!(exec.exit_code, 0);
    assert_eq!(exec.stdout, b"hi");
    assert!(exec.stderr.is_empty());
    // eval has no ZBRT opcode: it fails closed locally (see
    // eval_zbrt_fails_closed_without_sending_frames).
    assert!(matches!(
        sandbox.eval("1+1", None, None).await,
        Err(RfbError::Validation(_))
    ));
    assert_eq!(sandbox.ls(".").await.unwrap(), expected_entries());
    assert_eq!(sandbox.find(".", "*").await.unwrap(), vec!["a.txt"]);
    let matches = sandbox.grep(".", "hi").await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "a.txt");
    assert_eq!(matches[0].line, Some(1));
    assert_eq!(matches[0].text, "hi");
    let file = sandbox.read("a.txt", None, None).await.unwrap();
    assert_eq!(file.data, b"hi");
    assert!(!file.truncated);
    assert_eq!(file.total_bytes, Some(2));
    assert_eq!(sandbox.write("a.txt", b"hi", false, None).await.unwrap(), 2);
    sandbox.delete().await.unwrap();

    // ZBRT-only restrictions fail closed locally.
    assert!(matches!(
        sandbox.stream(&["tail"], None, Some(true), None).await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox
            .stream(&["tail"], None, None, Some(json!({"A": "1"})))
            .await,
        Err(RfbError::Validation(_))
    ));
}

#[tokio::test]
async fn zbrt_error_frame_raises_remote() {
    let server = spawn_zbrt(move |frame| vec![zerror(frame.request_id, 7, "kaboom")]);
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;
    let err = sandbox.exec(&["false"], "/", 60.0, b"").await.unwrap_err();
    match err {
        RfbError::Remote(message) => {
            assert!(message.contains("kaboom"), "message: {message}");
            assert!(message.contains("code 7"), "message: {message}");
        }
        other => panic!("expected Remote error, got {other:?}"),
    }
}

#[tokio::test]
async fn zbrt_stream_output_exit_and_cancel() {
    let server = spawn_zbrt(move |frame| {
        let rid = frame.request_id;
        match frame.kind {
            Kind::Execute => vec![zout(rid, 0, b"a"), zout(rid, 1, b"e")],
            Kind::Cancel => vec![zcancelack(rid), zexit(rid, -1)],
            _ => vec![zerror(rid, 1, "unexpected")],
        }
    });
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;
    let mut stream = sandbox
        .stream(&["tail", "-f"], Some("/"), None, None)
        .await
        .unwrap();

    let started = stream.next_event().await.unwrap().unwrap();
    assert_eq!(started.kind, StreamEventKind::Started);
    let chunk = stream.next_event().await.unwrap().unwrap();
    assert_eq!(chunk.kind, StreamEventKind::Stdout);
    assert_eq!(chunk.data, b"a");
    let chunk = stream.next_event().await.unwrap().unwrap();
    assert_eq!(chunk.kind, StreamEventKind::Stderr);
    assert_eq!(chunk.data, b"e");
    // stdin injection is unsupported over ZBRT before the terminal.
    assert!(matches!(
        stream.send_input("x").await,
        Err(RfbError::Remote(_))
    ));
    stream.stop().await.unwrap();
    let exit = stream.next_event().await.unwrap().unwrap();
    assert_eq!(exit.kind, StreamEventKind::Exit);
    assert_eq!(exit.code, Some(-1), "cancel maps to exit code -1");
    assert!(stream.next_event().await.unwrap().is_none());
    // Both idempotent after the terminal.
    assert!(matches!(
        stream.send_input("y").await,
        Err(RfbError::Remote(_))
    ));
    stream.stop().await.unwrap();
}

#[tokio::test]
async fn eval_zbrt_fails_closed_without_sending_frames() {
    // ZBRT v1 has no eval opcode and the reference guest maps Execute verbatim
    // onto `exec`: the old "facade convention" (argv=["eval", code]) surfaced
    // the guest's `eval: not found` exit code as a successful result. The
    // facade now rejects eval over ZBRT locally, with zero frames on the wire.
    let received = Arc::new(AtomicUsize::new(0));
    let counter = received.clone();
    let server = spawn_zbrt(move |frame| {
        counter.fetch_add(1, Ordering::SeqCst);
        vec![zexit(frame.request_id, 0)]
    });
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;
    assert!(matches!(
        sandbox.eval("1+1", Some("/workspace"), Some(5.0)).await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.eval("print(40+2)", None, None).await,
        Err(RfbError::Validation(_))
    ));

    // Validation still runs first: malformed code and a zero timeout are
    // rejected with the same error class, also without touching the wire.
    assert!(matches!(
        sandbox.eval("   ", None, None).await,
        Err(RfbError::Validation(_))
    ));
    assert!(matches!(
        sandbox.eval("1", None, Some(0.0)).await,
        Err(RfbError::Validation(_))
    ));
    assert_eq!(received.load(Ordering::SeqCst), 0, "no frames sent");
}

#[tokio::test]
async fn zbrt_hello_first_and_control_connection_reuse() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let server = FakeZbrt::new().with_log(log.clone()).spawn();
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;

    let exec = sandbox.exec(&["echo", "hi"], "/", 60.0, b"").await.unwrap();
    assert_eq!(exec.stdout, b"hi");
    assert!(sandbox.ping().await.unwrap());
    assert!(!sandbox.ls(".").await.unwrap().is_empty());
    assert!(!sandbox.ls(".").await.unwrap().is_empty());

    let log = log.lock().unwrap().clone();
    let caps = ZBRT_V1_CAPABILITIES.join(",");
    // exec keeps its own connection; ping + both ls calls share one control
    // connection (no extra Hello for the reuse).
    assert_eq!(
        log,
        vec![
            format!("conn0 hello client=rfb-sdk caps={caps}"),
            "conn0 execute".to_owned(),
            format!("conn1 hello client=rfb-sdk caps={caps}"),
            "conn1 health".to_owned(),
            "conn1 fs".to_owned(),
            "conn1 fs".to_owned(),
        ],
        "every connection must start with Hello; health/fs must reuse one control connection"
    );
}

#[tokio::test]
async fn zbrt_control_connection_reconnects_once_after_stale() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let server = FakeZbrt::new()
        .with_log(log.clone())
        .close_after_first_exchange()
        .spawn();
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;

    let entries = sandbox.ls(".").await.unwrap();
    assert_eq!(entries.len(), 1);
    // The server dropped the control connection after the first exchange; the
    // client must reconnect (fresh Hello) and retry this request once.
    let entries = sandbox.ls(".").await.unwrap();
    assert_eq!(entries.len(), 1);

    let log = log.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "conn0 hello client=rfb-sdk caps=execute,stream,deadline,health,cancel,filesystem"
                .to_owned(),
            "conn0 fs".to_owned(),
            "conn1 hello client=rfb-sdk caps=execute,stream,deadline,health,cancel,filesystem"
                .to_owned(),
            "conn1 fs".to_owned(),
        ],
        "stale control connection must be re-established with a fresh Hello"
    );
}

#[tokio::test]
async fn zbrt_handshake_failure_is_transport() {
    // A guest that answers the mandatory Hello with an Error frame makes the
    // connection unusable; the SDK classifies the failed handshake as
    // Transport.
    let addr = FakeZbrt::new().reject_handshake().spawn();
    let (_client, sandbox) = connect_fake(&addr, GuestTransport::Zbrt).await;
    let err = sandbox.exec(&["false"], "/", 60.0, b"").await.unwrap_err();
    assert!(
        matches!(err, RfbError::Transport(_)),
        "handshake failure must be Transport, got {err:?}"
    );
}

#[tokio::test]
async fn zbrt_argv_over_255_fails_closed_without_connecting() {
    // ZBRT v1 Execute carries argc in one byte: over-limit argv must be
    // rejected locally (Validation) with zero TCP connections.
    let connections = Arc::new(AtomicUsize::new(0));
    let addr = FakeZbrt::new()
        .count_connections(connections.clone())
        .spawn();
    let (_client, sandbox) = connect_fake(&addr, GuestTransport::Zbrt).await;

    let argv: Vec<String> = (0..256).map(|i| format!("a{i}")).collect();
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let err = sandbox.exec(&args, "/", 60.0, b"").await.unwrap_err();
    assert!(
        matches!(err, RfbError::Validation(_)),
        "argc>255 must be a local Validation error, got {err:?}"
    );
    // stream() runs the same local check.
    match sandbox.stream(&args, None, None, None).await {
        Err(RfbError::Validation(_)) => {}
        Err(err) => panic!("argc>255 stream must be a local Validation error, got {err:?}"),
        Ok(_) => panic!("argc>255 stream must be rejected"),
    }
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "zero TCP connections for an over-limit argv"
    );
}

/// P1-5: a control-connection read timeout means the request WAS delivered
/// and may still be executing on the guest — the client must drop the
/// connection but NOT resend the request (a retried non-idempotent Fs write,
/// e.g. an append, would run twice).
#[tokio::test]
async fn zbrt_control_connection_read_timeout_is_not_retried() {
    let deliveries = Arc::new(AtomicUsize::new(0));
    let addr = FakeZbrt::new()
        .count_deliveries(deliveries.clone())
        // Hold the connection open well past the 1s client timeout (a close
        // here would be a retryable connection failure).
        .stall_after_deliver(Duration::from_secs(5))
        .spawn();
    // 1s client/guest timeout so the read deadline fires quickly.
    let controller_addr = addr.clone();
    let controller = spawn_controller(move |req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/sandboxes") => (200, sandbox_list_json(&controller_addr)),
        _ => (404, "{\"error\":\"not found\"}".to_string()),
    });
    let client = RfbClient::new(format!("http://{controller}"), None, Duration::from_secs(1))
        .expect("client");
    let sandbox = client
        .connect_id_with("sb1", GuestTransport::Zbrt)
        .await
        .expect("connect");

    // append=true: replaying this write would corrupt the file.
    let result = sandbox.write("f.txt", b"x", true, None).await;
    match result {
        Err(RfbError::Transport(err)) => {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::TimedOut,
                "expected the read timeout to surface, got {err}"
            );
        }
        other => panic!("expected transport timeout, got {other:?}"),
    }
    assert_eq!(
        deliveries.load(Ordering::SeqCst),
        1,
        "a read timeout must NOT resend the request (P1-5)"
    );
}
