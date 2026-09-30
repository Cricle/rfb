//! Blocking fake ZBRT frame servers for the SDK client tests (`std::net` +
//! `std::thread`; see [`super::http`] for why this family must not be tokio).

use rfb::protocol::{
    Error as ZbrtErrorFrame, Exit as ZbrtExit, Frame, Health, Hello, HelloAck, Kind,
    Output as ZbrtOutput, ZBRT_V1_CAPABILITIES,
};
use serde_json::json;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn zframe(kind: Kind, request_id: [u8; 16], payload: Vec<u8>) -> Frame {
    Frame {
        kind,
        flags: 0,
        request_id,
        payload,
    }
}

pub fn zout(request_id: [u8; 16], stream: u8, data: &[u8]) -> Frame {
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

pub fn zexit(request_id: [u8; 16], code: i32) -> Frame {
    zframe(
        Kind::Exit,
        request_id,
        ZbrtExit { code, signal: None }.encode().unwrap(),
    )
}

pub fn zerror(request_id: [u8; 16], code: u32, message: &str) -> Frame {
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

pub fn zfsresult(request_id: [u8; 16], result: serde_json::Value) -> Frame {
    zframe(Kind::FsResult, request_id, result.to_string().into_bytes())
}

pub fn zhealthack(request_id: [u8; 16]) -> Frame {
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

pub fn zcancelack(request_id: [u8; 16]) -> Frame {
    zframe(Kind::CancelAck, request_id, Vec::new())
}

pub fn hello_capabilities() -> Vec<String> {
    ZBRT_V1_CAPABILITIES
        .iter()
        .map(|cap| (*cap).to_owned())
        .collect()
}

/// The standard HelloAck this fake family answers every Hello with.
fn hello_ack_frame(request_id: [u8; 16]) -> Frame {
    zframe(
        Kind::HelloAck,
        request_id,
        HelloAck {
            server: "rfb-zeroboot-guest".to_owned(),
            capabilities: hello_capabilities(),
        }
        .encode()
        .unwrap(),
    )
}

pub fn read_exact_blocking(
    stream: &mut std::net::TcpStream,
    buf: &mut [u8],
) -> std::io::Result<()> {
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

/// Generic fake ZBRT guest: the handler runs per frame on a per-connection
/// thread; the mandatory Hello handshake is auto-acknowledged here so
/// per-test handlers only see business frames.
pub fn spawn_zbrt<F>(handler: F) -> String
where
    F: Fn(Frame) -> Vec<Frame> + Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind zbrt");
    let addr = listener.local_addr().unwrap().to_string();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            // One thread per connection: the SDK client keeps the control
            // connection open (connection reuse) while opening additional
            // per-turn connections, so a serial accept loop would deadlock
            // the second connection.
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
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
                    // Mandatory ZBRT handshake (PROTOCOL.md §3.4): the SDK client
                    // sends Hello first on every connection; the fake guest
                    // auto-acknowledges it here so per-test handlers only see
                    // business frames.
                    let replies = if frame.kind == Kind::Hello {
                        let hello = Hello::decode(&frame.payload).expect("Hello payload");
                        assert_eq!(hello.client, "rfb-sdk", "hello client id");
                        assert_eq!(
                            hello.capabilities,
                            hello_capabilities(),
                            "hello capabilities"
                        );
                        vec![hello_ack_frame(frame.request_id)]
                    } else {
                        handler(frame)
                    };
                    write_replies(&mut stream, &replies);
                }
            });
        }
    });
    addr
}

fn write_replies(stream: &mut std::net::TcpStream, replies: &[Frame]) {
    for reply in replies {
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

/// Behaviour knobs for the canned-response [`FakeZbrt`]. Every field maps to
/// one observed test behaviour; unset knobs keep the default canned loop.
#[derive(Clone, Default)]
pub struct FakeZbrtOptions {
    /// Log one line per Hello/business frame so tests can assert the exact
    /// connection topology ("conn0 hello client=…", "conn0 fs", …).
    pub log: Option<Arc<Mutex<Vec<String>>>>,
    /// Answer the first Hello with an Error frame instead of a HelloAck
    /// (failed-handshake classification).
    pub reject_handshake: bool,
    /// Drop the connection right after its first business reply
    /// (stale-connection semantics: the client must transparently reconnect
    /// once and retry the request).
    pub close_after_first_exchange: bool,
    /// Count accepted connections (argc>255 test asserts zero).
    pub count_connections: Option<Arc<AtomicUsize>>,
    /// Count business frames read off the wire (P1-5 delivery counting).
    pub count_deliveries: Option<Arc<AtomicUsize>>,
    /// After reading the first business frame, hold the connection open this
    /// long WITHOUT replying (read-timeout test: the request WAS delivered
    /// and a close would look retryable).
    pub stall_after_deliver: Option<Duration>,
}

/// Canned-response fake ZBRT server unifying the recording server and the
/// inline handshake/counting/stall servers of `client_facade.rs`. Replies:
/// Hello → HelloAck (logged), Execute → Output+Exit, Fs → canned `ls`
/// FsResult, Health → HealthAck.
#[derive(Default)]
pub struct FakeZbrt {
    pub options: FakeZbrtOptions,
}

impl FakeZbrt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_log(mut self, log: Arc<Mutex<Vec<String>>>) -> Self {
        self.options.log = Some(log);
        self
    }

    pub fn reject_handshake(mut self) -> Self {
        self.options.reject_handshake = true;
        self
    }

    pub fn close_after_first_exchange(mut self) -> Self {
        self.options.close_after_first_exchange = true;
        self
    }

    pub fn count_connections(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.options.count_connections = Some(counter);
        self
    }

    pub fn count_deliveries(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.options.count_deliveries = Some(counter);
        self
    }

    pub fn stall_after_deliver(mut self, stall: Duration) -> Self {
        self.options.stall_after_deliver = Some(stall);
        self
    }

    /// Bind and serve until the listener errors. Returns the bound
    /// `127.0.0.1:<port>` address.
    pub fn spawn(self) -> String {
        let options = self.options;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind zbrt");
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let conn_idx = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                // One thread per connection: the client holds the control
                // connection open while exec opens its own, so a serial accept
                // loop would deadlock the second connection.
                let conn = conn_idx.fetch_add(1, Ordering::SeqCst);
                if let Some(counter) = &options.count_connections {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                let options = options.clone();
                std::thread::spawn(move || {
                    serve_connection(stream, conn, &options);
                });
            }
        });
        addr
    }
}

fn serve_connection(mut stream: std::net::TcpStream, conn: usize, options: &FakeZbrtOptions) {
    let mut exchanges = 0usize;
    loop {
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
        let Ok(kind) = Kind::parse(header[5]) else {
            break;
        };
        let rid: [u8; 16] = header[8..24].try_into().unwrap();
        let log = |line: String| {
            if let Some(log) = &options.log {
                log.lock().unwrap().push(line);
            }
        };
        let replies = if kind == Kind::Hello {
            if options.reject_handshake {
                let reply = zerror(rid, 1, "protocol handshake required");
                write_replies(&mut stream, &[reply]);
                break;
            }
            let hello = Hello::decode(&payload).expect("Hello payload");
            log(format!(
                "conn{conn} hello client={} caps={}",
                hello.client,
                hello.capabilities.join(",")
            ));
            vec![hello_ack_frame(rid)]
        } else {
            if let Some(deliveries) = &options.count_deliveries {
                deliveries.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(stall) = options.stall_after_deliver {
                // Hold the connection open well past the client timeout
                // (a close here would be a retryable connection failure).
                std::thread::sleep(stall);
                break;
            }
            exchanges += 1;
            match kind {
                Kind::Execute => {
                    log(format!("conn{conn} execute"));
                    vec![zout(rid, 0, b"hi"), zexit(rid, 0)]
                }
                Kind::Fs => {
                    log(format!("conn{conn} fs"));
                    vec![zfsresult(
                        rid,
                        json!({
                            "entries": [{"name": "a.txt", "is_dir": false, "size": 3}],
                            "truncated": false
                        }),
                    )]
                }
                Kind::Health => {
                    log(format!("conn{conn} health"));
                    vec![zhealthack(rid)]
                }
                _ => break,
            }
        };
        write_replies(&mut stream, &replies);
        if options.close_after_first_exchange && exchanges >= 1 {
            break;
        }
    }
}
