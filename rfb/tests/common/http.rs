//! Fake TCP servers for the SDK client tests.
//!
//! Two families, deliberately never mixed:
//! - [`blocking`] — plain `std::net` + `std::thread` servers. On some Windows
//!   environments a tokio task parked in a pending overlapped accept/read
//!   prevents IOCP completions from reaching the client runtime in the same
//!   process, wedging even explicit timeouts (measured in
//!   `client_facade.rs`); blocking server threads avoid overlapped
//!   server-side I/O entirely.
//! - [`mock_once`] — tokio one-shot servers for the forkd suites.

pub mod blocking {
    //! Blocking HTTP/1.1 fake forkd controller (one request per connection).

    use serde_json::Value;
    use std::io::{BufRead, BufReader, Read, Write};

    pub struct HttpReq {
        pub method: String,
        pub path: String,
        pub authorization: Option<String>,
        pub body: Value,
    }

    /// Serve HTTP/1.1 requests until the listener errors, one request per
    /// connection. Returns the bound `127.0.0.1:<port>` address.
    pub fn spawn_controller<F>(handler: F) -> String
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

    pub fn serve_one_http<F>(stream: &mut std::net::TcpStream, handler: &F) -> std::io::Result<()>
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
}

pub mod mock_once {
    //! Tokio one-shot HTTP mocks for the forkd suites (`forkd`,
    //! `forkd_controller`, `forkd_provider`): answer a single request, then
    //! stop.

    use serde_json::Value;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// One request captured off the wire (`capture` consumers parse `body`).
    #[derive(Debug)]
    pub struct CapturedRequest {
        pub method: String,
        pub path: String,
        pub body: String,
    }

    /// Serve exactly ONE HTTP request on an ephemeral port, reply
    /// `HTTP/1.1 <status_line>` + `body`, then stop. `delay` holds the
    /// response back after the request is read (timeout tests); when
    /// `capture` is provided the request head and body are pushed into it
    /// before the reply.
    ///
    /// The request-body read is capped at 1 MiB — the same guard
    /// `cluster.rs`'s controller read loop carries; keep the two in lockstep.
    /// Write errors are ignored: a timed-out client may already be gone.
    pub async fn mock_http(
        status_line: &str,
        body: String,
        delay: Option<Duration>,
        capture: Option<std::sync::mpsc::Sender<CapturedRequest>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock http");
        let addr = listener.local_addr().unwrap().to_string();
        let status_line = status_line.to_owned();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept mock http");
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .await
                .expect("read request line");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read header");
                if line == "\r\n" || line == "\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line
                    .strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
                {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            // 1 MiB request-body cap (see module doc).
            const MAX_BODY: usize = 1024 * 1024;
            let mut body_bytes = vec![0u8; content_length.min(MAX_BODY)];
            reader.read_exact(&mut body_bytes).await.ok();
            if let Some(capture) = &capture {
                let _ = capture.send(CapturedRequest {
                    method,
                    path,
                    body: String::from_utf8_lossy(&body_bytes).into_owned(),
                });
            }
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = reader.into_inner();
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
        (addr, handle)
    }

    /// [`mock_http`] with the `http://` prefix the `ForkdConfig` callers
    /// want (`forkd_provider.rs`'s `http_mock` shape, without the
    /// `Box::leak` dance — the body is owned).
    pub async fn mock_http_base(status_line: &str, body: String) -> String {
        let (addr, _handle) = mock_http(status_line, body, None, None).await;
        format!("http://{addr}")
    }

    /// Parse helper for captured bodies (`forkd.rs`'s memory-limit assertion).
    pub fn captured_json(captured: &CapturedRequest) -> Value {
        serde_json::from_str(captured.body.trim()).expect("captured request body is JSON")
    }
}
