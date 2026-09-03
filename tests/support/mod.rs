//! A mock embarch-core, for the tests that pin `embarch-core-client`'s HTTP
//! behaviour without a live Core.
//!
//! # Why this is hand-rolled rather than `wiremock`/`httpmock`
//!
//! Two of the three invariants these tests exist for cannot be expressed
//! against a well-behaved mock framework at all:
//!
//! - **Per-endpoint timeout independence** needs a server that accepts the
//!   TCP connection and then *never answers*. A framework's "delay" helper
//!   is close, but the thing under test is a `reqwest` per-request timeout,
//!   and the honest fixture for it is a socket that goes quiet.
//! - **Plain-text body on a non-2xx** needs a response that is deliberately
//!   *not* JSON, with a `Content-Type` to match — the exact shape axum's
//!   `IntoResponse for (StatusCode, String)` emits, which is what Core
//!   actually returns and what a JSON-first client would swallow.
//!
//! Both are a few lines of `tokio::net`, which this crate already depends on
//! with `features = ["full"]`. A mock-HTTP crate would be a new dependency
//! (and a new transitive tree) bought for less control than this gives.
//!
//! The server speaks the small subset of HTTP/1.1 `reqwest` needs: it reads
//! one request per connection, answers with `Connection: close`, and closes.
//! `reqwest` handles that without complaint and without connection reuse,
//! which keeps request/connection accounting one-to-one.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request the mock actually received, as it arrived on the wire.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    /// Request target as sent: path plus query string.
    pub target: String,
    /// Header names lowercased; values trimmed.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The target with any `?query` cut off.
    pub fn path(&self) -> &str {
        match self.target.split_once('?') {
            Some((path, _)) => path,
            None => &self.target,
        }
    }

    /// `(METHOD, path)`, the pair the bearer sweep matches on.
    pub fn route(&self) -> (String, String) {
        (self.method.clone(), self.path().to_string())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// What the mock does once it has read a request.
#[derive(Debug, Clone)]
pub enum Behavior {
    /// Answer every request identically.
    Reply {
        status: u16,
        reason: String,
        content_type: String,
        body: String,
    },
    /// Accept the connection, read the request, record it, and then never
    /// answer — the fixture for "this endpoint's own timeout is what fires".
    BlackHole,
}

impl Behavior {
    /// The shape Core's error responses actually take: axum's
    /// `IntoResponse for (StatusCode, String)`, which is `text/plain` and
    /// never JSON.
    pub fn plain_text_error(status: u16, reason: &str, body: &str) -> Behavior {
        Behavior::Reply {
            status,
            reason: reason.to_string(),
            content_type: "text/plain; charset=utf-8".to_string(),
            body: body.to_string(),
        }
    }
}

pub struct MockCore {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockCore {
    /// Binds an ephemeral loopback port and starts serving. The server task
    /// lives as long as the test's runtime; there is nothing to shut down.
    pub async fn start(behavior: Behavior) -> MockCore {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("could not bind a loopback port for the mock Core");
        let addr = listener
            .local_addr()
            .expect("bound listener has no local address");
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let sink = Arc::clone(&sink);
                let behavior = behavior.clone();
                tokio::spawn(serve_one(stream, behavior, sink));
            }
        });

        MockCore {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .expect("mock Core request log poisoned")
            .clone()
    }
}

async fn serve_one(
    mut stream: TcpStream,
    behavior: Behavior,
    sink: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let Ok(request) = read_request(&mut stream).await else {
        return;
    };
    sink.lock()
        .expect("mock Core request log poisoned")
        .push(request);

    match behavior {
        Behavior::Reply {
            status,
            reason,
            content_type,
            body,
        } => {
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\n\
                 Content-Type: {content_type}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
        // Hold the socket open, unanswered, until the test's runtime drops
        // this task. Dropping `stream` here instead would close the
        // connection and the client would fail fast on a reset rather than
        // on its own timeout, which is the thing being measured.
        Behavior::BlackHole => std::future::pending::<()>().await,
    }
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<RecordedRequest> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    let head_end = loop {
        if let Some(index) = find(&buf, b"\r\n\r\n") {
            break index + 4;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before a complete request head arrived",
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let target = request_line.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buf[head_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(RecordedRequest {
        method,
        target,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
