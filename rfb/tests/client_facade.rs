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
//! ZBRT frame server; controller surface, NDJSON multi-line + stream
//! sessions, ZBRT exec/fs/stream/cancel/health, identical facade shapes on
//! both transports, and local validation fail-closed rules.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rfb::client::{
    CreateOptions, DirEntry, GuestTransport, RfbClient, RfbError, Sandbox, StreamEventKind,
};
use rfb::protocol::{
    Error as ZbrtErrorFrame, Exit as ZbrtExit, Frame, Fs, Health, Kind, Output as ZbrtOutput,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fake forkd controller (blocking HTTP/1.1, one request per connection)
// ---------------------------------------------------------------------------

struct HttpReq {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}

fn spawn_controller<F>(handler: F) -> String
where
    F: Fn(HttpReq) -> (u16, String) + Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind controller");
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            if serve_one_http(&mut stream, &handler).is_err() {
                break;
            }
        }
    });
    addr
}

fn serve_one_http<F>(stream: &mut std::net::TcpStream, handler: &F) -> std::io::Result<()>
where
    F: Fn(HttpReq) -> (u16, String),
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = String::new();
    // Read the request head (request line + headers).
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        head.push_str(&line);
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut lines = head.split("\r\n").flat_map(|l| l.split('\n'));
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut authorization = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            if key == "authorization" {
                authorization = Some(value.to_string());
            }
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let (status, text) = handler(HttpReq {
        method,
        path,
        authorization,
        body,
    });
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Response",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        text.len(),
        text
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

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
async fn connect_fake(guest_addr: &str, transport: GuestTransport) -> (RfbClient, Sandbox) {
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
// Fake forkd guest (blocking NDJSON over TCP; handler runs per connection)
// ---------------------------------------------------------------------------

struct GuestConn {
    reader: BufReader<std::net::TcpStream>,
    writer: std::net::TcpStream,
}

impl GuestConn {
    /// Blocking read of one JSON line; `None` on clean EOF.
    fn recv(&mut self) -> Option<Value> {
        let mut line = Vec::new();
        let n = self.reader.read_until(b'\n', &mut line).ok()?;
        if n == 0 {
            return None;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            return self.recv();
        }
        Some(serde_json::from_slice(&line).expect("valid guest JSON line"))
    }

    fn send(&mut self, value: &Value) {
        let mut line = serde_json::to_vec(value).expect("serialize reply");
        line.push(b'\n');
        self.writer.write_all(&line).expect("write reply");
        self.writer.flush().expect("flush reply");
    }
}

fn spawn_guest<F>(handler: F) -> String
where
    F: Fn(&mut GuestConn) + Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind guest");
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let writer = match stream.try_clone() {
                Ok(w) => w,
                Err(_) => break,
            };
            let mut conn = GuestConn {
                reader: BufReader::new(stream),
                writer,
            };
            handler(&mut conn);
        }
    });
    addr
}

// ---------------------------------------------------------------------------
// Fake ZBRT frame server (blocking frame loop; handler runs per frame)
// ---------------------------------------------------------------------------

fn zframe(kind: Kind, request_id: [u8; 16], payload: Vec<u8>) -> Frame {
    Frame {
        kind,
        flags: 0,
        request_id,
        payload,
    }
}

fn zout(request_id: [u8; 16], stream: u8, data: &[u8]) -> Frame {
    zframe(
        Kind::Output,
        request_id,
        ZbrtOutput {
            stream,
            data: data.to_vec(),
        }
        .encode()
        .unwrap(),
    )
}

fn zexit(request_id: [u8; 16], code: i32) -> Frame {
    zframe(
        Kind::Exit,
        request_id,
        ZbrtExit { code, signal: None }.encode().unwrap(),
    )
}

fn zerror(request_id: [u8; 16], code: u32, message: &str) -> Frame {
    zframe(
        Kind::Error,
        request_id,
        ZbrtErrorFrame {
            code,
            message: message.to_owned(),
        }
        .encode()
        .unwrap(),
    )
}

fn zfsresult(request_id: [u8; 16], result: Value) -> Frame {
    zframe(Kind::FsResult, request_id, result.to_string().into_bytes())
}

fn zhealthack(request_id: [u8; 16]) -> Frame {
    zframe(
        Kind::HealthAck,
        request_id,
        Health {
            healthy: true,
            message: Some("ready".to_owned()),
        }
        .encode()
        .unwrap(),
    )
}

fn zcancelack(request_id: [u8; 16]) -> Frame {
    zframe(Kind::CancelAck, request_id, Vec::new())
}

fn spawn_zbrt<F>(handler: F) -> String
where
    F: Fn(Frame) -> Vec<Frame> + Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind zbrt");
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            loop {
                // Blocking ZBRT frame read: 28-byte header + payload.
                let mut header = [0u8; 28];
                if read_exact_blocking(&mut stream, &mut header).is_err() {
                    break;
                }
                if &header[..4] != b"ZBRT" || header[4] != 1 {
                    break;
                }
                let len = u32::from_be_bytes(header[24..28].try_into().unwrap()) as usize;
                let mut payload = vec![0u8; len];
                if read_exact_blocking(&mut stream, &mut payload).is_err() {
                    break;
                }
                let kind = match header[5] {
                    1 => Kind::Hello,
                    2 => Kind::HelloAck,
                    3 => Kind::Execute,
                    4 => Kind::Output,
                    5 => Kind::Exit,
                    6 => Kind::Cancel,
                    7 => Kind::CancelAck,
                    8 => Kind::Fs,
                    9 => Kind::FsResult,
                    10 => Kind::Health,
                    11 => Kind::HealthAck,
                    12 => Kind::Error,
                    13 => Kind::Result,
                    _ => break,
                };
                let frame = Frame {
                    kind,
                    flags: 0,
                    request_id: header[8..24].try_into().unwrap(),
                    payload,
                };
                for reply in handler(frame) {
                    let mut bytes = Vec::new();
                    if reply.encode(&mut bytes).is_err() {
                        break;
                    };
                    if stream.write_all(&bytes).is_err() {
                        break;
                    }
                    if stream.flush().is_err() {
                        break;
                    }
                }
            }
        }
    });
    addr
}

fn read_exact_blocking(stream: &mut std::net::TcpStream, buf: &mut [u8]) -> std::io::Result<()> {
    let mut read = 0;
    while read < buf.len() {
        let n = stream.read(&mut buf[read..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof",
            ));
        }
        read += n;
    }
    Ok(())
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
    // eval output maps to stdout (Execute turn with the `eval` op convention).
    let eval = sandbox.eval("1+1", None, None).await.unwrap();
    assert_eq!(eval.exit_code, 0);
    assert_eq!(eval.stdout, b"hi");
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

// ---------------------------------------------------------------------------
// Shared eval-over-ZBRT conformance vectors
// (sdk/shared/conformance/eval_zbrt_vectors.json)
// ---------------------------------------------------------------------------

const EVAL_GOLDEN_RID: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Re-encode a received frame with its request id normalized to the shared
/// vector's golden id, so wire bytes compare byte-for-byte regardless of the
/// client's random per-request UUID.
fn normalized_frame_hex(frame: &Frame) -> String {
    let mut bytes = Vec::new();
    frame.encode(&mut bytes).expect("re-encode frame");
    bytes[8..24].copy_from_slice(&EVAL_GOLDEN_RID);
    bytes_to_hex(&bytes)
}

#[tokio::test]
async fn eval_zbrt_matches_shared_vector() {
    // EVAL_ZBRT_BASIC: eval("1+1", cwd="/workspace", timeout_s=5) →
    // Execute(argv=["eval","1+1"], cwd, stdin empty, timeout_ms=5000).
    let sent = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = sent.clone();
    let server = spawn_zbrt(move |frame| {
        let hex = normalized_frame_hex(&frame);
        captured.lock().unwrap().push(hex);
        let rid = frame.request_id;
        match frame.kind {
            Kind::Execute => vec![zout(rid, 0, &hex_to_bytes("32")), zexit(rid, 0)],
            _ => vec![zerror(rid, 1, "unexpected kind")],
        }
    });
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;
    let result = sandbox
        .eval("1+1", Some("/workspace"), Some(5.0))
        .await
        .unwrap();
    assert_eq!(
        sent.lock().unwrap().as_slice(),
        ["5a42525401030000000102030405060708090a0b0c0d0e0f0000002702000000046576616c00000003312b31010000000a2f776f726b73706163650000000000001388"],
        "EVAL_ZBRT_BASIC wire bytes"
    );
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, hex_to_bytes("32"));
    assert!(result.stderr.is_empty());
    assert!(!result.timed_out);

    // EVAL_ZBRT_DEFAULTS: eval("print(40+2)") — cwd flag 0, timeout_ms 0.
    let sent = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = sent.clone();
    let server = spawn_zbrt(move |frame| {
        let hex = normalized_frame_hex(&frame);
        captured.lock().unwrap().push(hex);
        let rid = frame.request_id;
        match frame.kind {
            Kind::Execute => vec![zout(rid, 0, &hex_to_bytes("3432")), zexit(rid, 0)],
            _ => vec![zerror(rid, 1, "unexpected kind")],
        }
    });
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;
    let result = sandbox.eval("print(40+2)", None, None).await.unwrap();
    assert_eq!(
        sent.lock().unwrap().as_slice(),
        ["5a42525401030000000102030405060708090a0b0c0d0e0f0000002102000000046576616c0000000b7072696e742834302b3229000000000000000000"],
        "EVAL_ZBRT_DEFAULTS wire bytes"
    );
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, hex_to_bytes("3432"));

    // EVAL_ZBRT_VALIDATION_REJECTED + EVAL_ZBRT_TIMEOUT_ZERO_REJECTED:
    // both fail closed locally with zero frames on the wire.
    let received = Arc::new(AtomicUsize::new(0));
    let counter = received.clone();
    let server = spawn_zbrt(move |frame| {
        counter.fetch_add(1, Ordering::SeqCst);
        vec![zexit(frame.request_id, 0)]
    });
    let (_client, sandbox) = connect_fake(&server, GuestTransport::Zbrt).await;
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
