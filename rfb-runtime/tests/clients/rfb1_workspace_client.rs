#[cfg(unix)]
#[path = "../../src/codec.rs"]
#[allow(dead_code)]
mod codec;
#[cfg(unix)]
#[path = "../../src/resources.rs"]
#[allow(dead_code)]
mod resources;
#[cfg(unix)]
#[path = "../../src/session.rs"]
#[allow(dead_code)]
mod session;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(unix)]
use codec::{read_frame_blocking, write_frame_blocking, FrameCodec, MessageType};
#[cfg(unix)]
use session::{ControlMessage, FileReadRequest, FileWriteRequest, RuntimeMessage, SessionRequest};
#[cfg(unix)]
use std::env;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
fn fail(message: impl AsRef<str>) -> ! {
    eprintln!("FAIL: {}", message.as_ref());
    std::process::exit(1);
}

#[cfg(unix)]
fn send(stream: &mut UnixStream, codec: &FrameCodec, sequence: u64, request: &ControlMessage) {
    let message_type = match request {
        ControlMessage::Hello { .. } => MessageType::Hello,
        ControlMessage::Capabilities { .. } => MessageType::Capabilities,
        ControlMessage::StartTurn(_) => MessageType::StartTurn,
        ControlMessage::Cancel { .. } => MessageType::CancelTurn,
        ControlMessage::ReadWorkspaceFile(_) => MessageType::ReadWorkspaceFile,
        ControlMessage::WriteWorkspaceFile(_) => MessageType::WriteWorkspaceFile,
        ControlMessage::ReadHostFile(_) => MessageType::ReadHostFile,
        ControlMessage::Shutdown => MessageType::Shutdown,
    };
    write_frame_blocking(stream, codec, message_type, sequence, request)
        .unwrap_or_else(|error| fail(format!("write sequence {sequence}: {error}")));
}

#[cfg(unix)]
fn receive(stream: &mut UnixStream, codec: &FrameCodec, sequence: u64) -> RuntimeMessage {
    let (frame, message): (_, RuntimeMessage) = read_frame_blocking(stream, codec)
        .unwrap_or_else(|error| fail(format!("read response sequence {sequence}: {error}")));
    if frame.sequence != sequence {
        fail(format!(
            "response sequence {} != {sequence}",
            frame.sequence
        ));
    }
    message
}

#[cfg(unix)]
fn expect(stream: &mut UnixStream, codec: &FrameCodec, sequence: u64, expected: RuntimeMessage) {
    let actual = receive(stream, codec, sequence);
    if actual != expected {
        fail(format!(
            "unexpected response at sequence {sequence}: {actual:?}, expected {expected:?}"
        ));
    }
}

#[cfg(unix)]
fn connect_uds(uds: &str, port: &str, deadline: std::time::Instant) -> io::Result<UnixStream> {
    loop {
        match UnixStream::connect(uds) {
            Ok(mut stream) => {
                stream.write_all(format!("CONNECT {port}\n").as_bytes())?;
                let mut handshake = Vec::new();
                loop {
                    let mut byte = [0u8; 1];
                    stream.read_exact(&mut byte)?;
                    handshake.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                if handshake.starts_with(b"OK ") {
                    return Ok(stream);
                }
            }
            Err(_error) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(error) => return Err(error),
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "vsock CONNECT timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(unix)]
fn main() -> io::Result<()> {
    let uds = env::args()
        .nth(1)
        .unwrap_or_else(|| fail("usage: rfb1_workspace_client PATH [PORT] [TIMEOUT]"));
    let port = env::args().nth(2).unwrap_or_else(|| "5000".into());
    let timeout = env::args()
        .nth(3)
        .unwrap_or_else(|| "15".into())
        .parse::<u64>()
        .unwrap_or(15);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    let mut stream = connect_uds(&uds, &port, deadline).expect("primary vsock connect");
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let codec = FrameCodec::default();

    send(
        &mut stream,
        &codec,
        0,
        &ControlMessage::Hello {
            protocol_version: 1,
        },
    );
    expect(
        &mut stream,
        &codec,
        0,
        RuntimeMessage::HelloAck {
            protocol_version: 1,
        },
    );
    send(
        &mut stream,
        &codec,
        1,
        &ControlMessage::Capabilities {
            session_per_vm: true,
            writable_workspace: true,
        },
    );
    expect(
        &mut stream,
        &codec,
        1,
        RuntimeMessage::Capabilities {
            session_per_vm: true,
            writable_workspace: true,
        },
    );

    let content = b"RFB1_WORKSPACE_FILE_OK".to_vec();
    send(
        &mut stream,
        &codec,
        2,
        &ControlMessage::WriteWorkspaceFile(FileWriteRequest {
            request_id: "write-1".into(),
            path: "nested/result.txt".into(),
            content: content.clone(),
        }),
    );
    expect(
        &mut stream,
        &codec,
        2,
        RuntimeMessage::WriteAck {
            request_id: "write-1".into(),
            path: "nested/result.txt".into(),
        },
    );
    send(
        &mut stream,
        &codec,
        3,
        &ControlMessage::ReadWorkspaceFile(FileReadRequest {
            request_id: "read-1".into(),
            path: "nested/result.txt".into(),
            max_bytes: 1024,
        }),
    );
    expect(
        &mut stream,
        &codec,
        3,
        RuntimeMessage::FileContent {
            request_id: "read-1".into(),
            path: "nested/result.txt".into(),
            content,
        },
    );

    send(
        &mut stream,
        &codec,
        4,
        &ControlMessage::StartTurn(SessionRequest {
            session_id: "acceptance-session".into(),
            request_id: "turn-1".into(),
            prompt: r#"{"op":"exec","args":["./.rfb-acceptance-tool","RFB1_WORKSPACE_OK"]}"#.into(),
        }),
    );
    let mut kinds = Vec::new();
    loop {
        match receive(&mut stream, &codec, 4) {
            RuntimeMessage::Event(event) => {
                if event.session_id != "acceptance-session" {
                    fail("event session id mismatch");
                }
                if event.kind == "terminal.output"
                    && !event
                        .payload
                        .windows(b"RFB1_WORKSPACE_OK".len())
                        .any(|w| w == b"RFB1_WORKSPACE_OK")
                {
                    fail("terminal output missing tool marker");
                }
                kinds.push(event.kind.clone());
                if event.kind == "turn.completed" {
                    break;
                }
            }
            other => fail(format!("StartTurn returned non-event: {other:?}")),
        }
    }
    if kinds.first().map(String::as_str) != Some("turn.started")
        || !kinds.iter().any(|kind| kind == "terminal.output")
    {
        fail(format!("missing StartTurn terminal events: {kinds:?}"));
    }

    // Cross-connection in-flight cancel: connection B cancels a long-running
    // turn that connection A started. The shared RuntimeService must route the
    // Cancel to A's worker flag, terminate the process group, and emit exactly
    // one `turn.cancelled` terminal to A.
    let uds_a = uds.clone();
    let port_a = port.clone();
    let deadline_a = std::time::Instant::now() + Duration::from_secs(timeout);
    let thread_a = std::thread::spawn(move || -> io::Result<()> {
        let mut stream = connect_uds(&uds_a, &port_a, deadline_a).expect("connection A connect");
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let codec_a = FrameCodec::default();
        send(
            &mut stream,
            &codec_a,
            0,
            &ControlMessage::Hello {
                protocol_version: 1,
            },
        );
        expect(
            &mut stream,
            &codec_a,
            0,
            RuntimeMessage::HelloAck {
                protocol_version: 1,
            },
        );
        send(
            &mut stream,
            &codec_a,
            5,
            &ControlMessage::StartTurn(SessionRequest {
                session_id: "acceptance-session".into(),
                request_id: "turn-cancel".into(),
                prompt: r#"{"op":"exec","args":["./.rfb-sleep-tool"],"timeout_secs":120}"#.into(),
            }),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if std::time::Instant::now() >= deadline {
                fail("cancel did not interrupt the in-flight turn within 15s");
            }
            match receive(&mut stream, &codec_a, 5) {
                RuntimeMessage::Event(event) => match event.kind.as_str() {
                    "turn.started" | "terminal.output" => {}
                    "turn.cancelled" => break,
                    "turn.completed" => fail("canceled turn incorrectly completed"),
                    other => fail(format!("unexpected event during cancel: {other}")),
                },
                other => fail(format!("cancel returned non-event: {other:?}")),
            }
        }
        Ok(())
    });

    // Let A's StartTurn land, then cancel it from connection B. Retry a few
    // times with small gaps so the Cancel reliably lands after A claimed the
    // turn (a too-early Cancel would be rejected as "not active").
    std::thread::sleep(Duration::from_millis(300));
    let mut stream_b = connect_uds(&uds, &port, deadline).expect("connection B connect");
    stream_b.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream_b.set_write_timeout(Some(Duration::from_secs(10)))?;
    let codec_b = FrameCodec::default();
    send(
        &mut stream_b,
        &codec_b,
        0,
        &ControlMessage::Hello {
            protocol_version: 1,
        },
    );
    expect(
        &mut stream_b,
        &codec_b,
        0,
        RuntimeMessage::HelloAck {
            protocol_version: 1,
        },
    );
    for attempt in 0..5u32 {
        send(
            &mut stream_b,
            &codec_b,
            1,
            &ControlMessage::Cancel {
                session_id: "acceptance-session".into(),
                request_id: "turn-cancel".into(),
            },
        );
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    // Connection B sees no inline response for a routed cancel; it receives the
    // next unsolicited event stream only if the implementation broadcasts. In
    // the current design the terminal is delivered to A, so B simply closes.
    drop(stream_b);
    thread_a.join().expect("connection A thread panicked")?;

    // Repeat cancel on the primary connection is idempotent (cached terminal).
    send(
        &mut stream,
        &codec,
        7,
        &ControlMessage::Cancel {
            session_id: "acceptance-session".into(),
            request_id: "turn-cancel".into(),
        },
    );
    match receive(&mut stream, &codec, 7) {
        RuntimeMessage::Event(event) if event.kind == "turn.cancelled" => {}
        RuntimeMessage::Error { message, .. } if message.contains("not active") => {}
        other => fail(format!("repeat cancel did not stay idempotent: {other:?}")),
    }

    send(&mut stream, &codec, 8, &ControlMessage::Shutdown);
    expect(&mut stream, &codec, 8, RuntimeMessage::ShutdownAck);
    println!("PASS: RFB1 Hello/Capabilities, workspace write/read, StartTurn terminal events, cross-connection in-flight Cancel, Shutdown");
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("RFB1 workspace client requires Unix");
}
