//! A mock embarch-core, for the tests that pin `embarch-core-client`'s HTTP
//! behaviour without a live Core.
//!
//! Each file under `tests/` compiles its own copy of this module, so any
//! item only one of them needs looks unused to the others — hence the
//! blanket `dead_code` allow rather than a per-item one. It is a shared
//! harness, and a harness is allowed to offer more than one caller uses.
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

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// Answer differently per route. Added 2026-09-02 for the SSE suite,
    /// which is the first thing here to make *two* kinds of request in one
    /// call: `follow_study` opens `/study/{id}/events` and then polls
    /// `/study/{id}`, and the whole point of those tests is what happens
    /// when those two disagree.
    ///
    /// Matched by path suffix, first match wins.
    Router {
        routes: Vec<(String, Behavior)>,
        otherwise: Box<Behavior>,
    },
    /// A `text/event-stream` response, written as real HTTP/1.1 chunked
    /// framing: each entry in `chunks` is flushed as its own chunk with
    /// `gap` between them, then the connection ends per `then`.
    ///
    /// Chunked rather than "no framing headers, terminated by close" so that
    /// [`StreamTail::Cut`] is a *detectably* truncated body rather than a
    /// clean end-of-stream — which is the difference the fallback path
    /// reports and therefore the difference these tests need to be able to
    /// stage.
    EventStream {
        chunks: Vec<Vec<u8>>,
        gap: Duration,
        then: StreamTail,
    },
    /// The n-th request *to this path* gets the n-th behavior; the last one
    /// repeats forever. The fixture for a study that is still running the
    /// first time it is polled and finished the third.
    Sequence(Vec<Behavior>),
}

/// How a [`Behavior::EventStream`] ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTail {
    /// Write chunked's terminating `0\r\n\r\n` and close — a clean end of
    /// body, which the client sees as "embarch-core closed the stream".
    Close,
    /// Drop the socket with no terminator — a real disconnect, which the
    /// client sees as a transport error.
    Cut,
    /// Never end. What a healthy Core does, since its broadcast channel
    /// outlives any one study.
    Hold,
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

    /// A `200` with a JSON body — what `GET /study/{id}` answers.
    pub fn json_ok(body: serde_json::Value) -> Behavior {
        Behavior::Reply {
            status: 200,
            reason: "OK".to_string(),
            content_type: "application/json".to_string(),
            body: body.to_string(),
        }
    }
}

/// One SSE frame, byte-for-byte as `axum` writes it.
///
/// Reproduced from `axum::response::sse::Event::field` rather than invented:
/// it emits `name`, `:`, one space, the value, `\n` per field, and one more
/// `\n` to finalize. Getting this exactly right is the difference between a
/// test that pins embarch-core's wire format and one that pins a guess about
/// it — the guess would pass just as green.
pub fn sse_frame(event: Option<&str>, data: &str) -> Vec<u8> {
    let mut out = String::new();
    if let Some(event) = event {
        out.push_str(&format!("event: {event}\n"));
    }
    for line in data.split('\n') {
        out.push_str(&format!("data: {line}\n"));
    }
    out.push('\n');
    out.into_bytes()
}

/// `axum`'s `KeepAlive::default()` comment frame, verbatim
/// (`axum::response::sse::KeepAlive::DEFAULT_KEEP_ALIVE` is `b":\n\n"`).
pub fn sse_keep_alive() -> Vec<u8> {
    b":\n\n".to_vec()
}

pub struct MockCore {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

/// How many requests each path has already had, so [`Behavior::Sequence`]
/// can advance. Per path rather than per server: a `follow_study` test
/// scripts `/study/{id}` as a sequence while `/study/{id}/events` is
/// something else entirely, and a single counter would let one consume the
/// other's turns.
type SequenceCounters = Arc<Mutex<HashMap<String, usize>>>;

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
        let counters: SequenceCounters = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let sink = Arc::clone(&sink);
                let counters = Arc::clone(&counters);
                let behavior = behavior.clone();
                tokio::spawn(serve_one(stream, behavior, sink, counters));
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
    counters: SequenceCounters,
) {
    let Ok(request) = read_request(&mut stream).await else {
        return;
    };
    let path = request.path().to_string();
    sink.lock()
        .expect("mock Core request log poisoned")
        .push(request);

    let behavior = resolve(behavior, &path, &counters);

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
        Behavior::EventStream { chunks, gap, then } => {
            let head = "HTTP/1.1 200 OK\r\n\
                        Content-Type: text/event-stream\r\n\
                        Cache-Control: no-cache\r\n\
                        Transfer-Encoding: chunked\r\n\r\n";
            if stream.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for chunk in chunks {
                if gap > Duration::ZERO {
                    tokio::time::sleep(gap).await;
                }
                let framed = format!("{:x}\r\n", chunk.len());
                if stream.write_all(framed.as_bytes()).await.is_err() {
                    return;
                }
                if stream.write_all(&chunk).await.is_err() {
                    return;
                }
                if stream.write_all(b"\r\n").await.is_err() {
                    return;
                }
                let _ = stream.flush().await;
            }
            match then {
                StreamTail::Close => {
                    let _ = stream.write_all(b"0\r\n\r\n").await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                }
                // Drop the socket without chunked's terminator. `hyper`
                // surfaces that as an error on the body, not as a clean end
                // — which is exactly the "the connection went away
                // mid-study" case.
                StreamTail::Cut => drop(stream),
                StreamTail::Hold => std::future::pending::<()>().await,
            }
        }
        // Both are containers; `resolve` has already reduced them.
        Behavior::Router { .. } | Behavior::Sequence(..) => unreachable!(),
    }
}

/// Reduce a possibly-nested [`Behavior`] to the leaf that answers this
/// request.
fn resolve(behavior: Behavior, path: &str, counters: &SequenceCounters) -> Behavior {
    match behavior {
        Behavior::Router { routes, otherwise } => {
            let picked = routes
                .into_iter()
                .find(|(suffix, _)| path.ends_with(suffix.as_str()))
                .map(|(_, behavior)| behavior)
                .unwrap_or(*otherwise);
            resolve(picked, path, counters)
        }
        Behavior::Sequence(steps) => {
            assert!(!steps.is_empty(), "an empty Behavior::Sequence answers nothing");
            let picked = {
                let mut seen = counters.lock().expect("mock Core sequence counters poisoned");
                let count = seen.entry(path.to_string()).or_insert(0);
                let index = (*count).min(steps.len() - 1);
                *count += 1;
                steps[index].clone()
            };
            resolve(picked, path, counters)
        }
        leaf => leaf,
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
