//! Fake NDJSON-over-TCP guest servers.
//!
//! Two families, deliberately never mixed — see [`super::http`] for the
//! blocking-vs-tokio rationale:
//! - [`blocking`] — `std::net` + `std::thread` guest with a handler per
//!   connection (`client_facade.rs`).
//! - [`mock_once`] — tokio one-shot line server for the forkd suites.

pub mod blocking {
    //! Blocking NDJSON fake guest: handler runs per connection.

    use serde_json::Value;
    use std::io::{BufRead, BufReader, Write};

    pub struct GuestConn {
        reader: BufReader<std::net::TcpStream>,
        writer: std::net::TcpStream,
    }

    impl GuestConn {
        /// Blocking read of one JSON line; `None` on clean EOF.
        pub fn recv(&mut self) -> Option<Value> {
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

        pub fn send(&mut self, value: &Value) {
            let mut line = serde_json::to_vec(value).expect("serialize reply");
            line.push(b'\n');
            self.writer.write_all(&line).expect("write reply");
            self.writer.flush().expect("flush reply");
        }
    }

    /// Serve NDJSON connections until the listener errors; the handler runs
    /// per connection. Returns the bound `127.0.0.1:<port>` address.
    pub fn spawn_guest<F>(handler: F) -> String
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
}

pub mod mock_once {
    //! Tokio one-shot NDJSON line servers for the forkd suites: each
    //! connection gets one request line read and one response written.

    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    /// Serve exactly ONE connection: read one request line, write the single
    /// NDJSON `response`, close. Returns the bound `127.0.0.1:<port>` address
    /// and a task handle resolving to the parsed request (forkd_guest.rs's
    /// `server` shape).
    pub async fn mock_ndjson_once(response: String) -> (String, tokio::task::JoinHandle<Value>) {
        let (addr, handle) = mock_ndjson_lines(vec![response]).await;
        (
            addr,
            tokio::task::spawn(async move {
                let mut requests = handle.await.expect("mock ndjson server");
                requests.remove(0)
            }),
        )
    }

    /// Serve `responses.len()` sequential connections on one listener; the
    /// k-th connection gets `responses[k]`. Returns the address and a task
    /// handle resolving to the parsed request lines in order, so tests can
    /// assert the wire shape after the fact.
    pub async fn mock_ndjson_lines(
        responses: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock ndjson");
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (stream, _) = listener.accept().await.expect("accept mock ndjson");
                requests.push(serve_one(stream, &response).await);
            }
            requests
        });
        (addr, handle)
    }

    async fn serve_one(stream: TcpStream, response: &str) -> Value {
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        write.write_all(response.as_bytes()).await.unwrap();
        write.flush().await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }
}
