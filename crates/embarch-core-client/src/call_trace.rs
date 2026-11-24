//! What a caller can cite of Core's call log (`embarch-core` decision 122):
//! for each call to a lease-gated route made inside a [`scope`], the entry
//! Core stamped on its answer in `X-Embarch-Call-Seq` / `X-Embarch-Call-Hash`.
//!
//! Additive and off by default. Nothing is collected outside a [`scope`], so
//! `embarch-ui` and the other users of this crate are unchanged. The scope is
//! a task-local, so two tool calls running at once in one process do not see
//! each other's entries.

use std::future::Future;
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// The response headers a gated route's answer carries (Core's `call_log.rs`).
pub const SEQ_HEADER: &str = "x-embarch-call-seq";
pub const HASH_HEADER: &str = "x-embarch-call-hash";
/// Present on a folded repeat: which repeat of the first entry this is.
pub const REPEAT_HEADER: &str = "x-embarch-call-repeat";

/// A JSON request body this small is kept in the entry, a larger one is not
/// (it is a firmware image or a study). Core keeps its own at the same size.
pub const SENT_CAP_BYTES: usize = 4096;

/// One call to a gated route, as this client made it and Core answered it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CallEntry {
    pub method: String,
    pub path: String,
    /// The HTTP status of the answer (a refusal is `409`, say).
    pub status: u16,
    /// Core's entry for the call; `None` against a Core that sent no header.
    pub seq: Option<u64>,
    pub hash: Option<String>,
    /// A folded repeat of an identical read: which repeat of entry `seq`.
    pub repeat: Option<u64>,
    /// The JSON body sent, when it was JSON and within [`SENT_CAP_BYTES`].
    pub sent: Option<serde_json::Value>,
    /// The query string sent, if any.
    pub query: Option<String>,
}

type Sink = Arc<Mutex<Vec<CallEntry>>>;

tokio::task_local! {
    static SINK: Sink;
}

/// Runs `fut` collecting the entries of every gated call it makes, and returns
/// its output with them in the order the calls were made.
pub async fn scope<F: Future>(fut: F) -> (F::Output, Vec<CallEntry>) {
    let sink: Sink = Arc::default();
    let out = SINK.scope(sink.clone(), fut).await;
    let entries = sink.lock().map(|g| g.clone()).unwrap_or_default();
    (out, entries)
}

/// The entries collected so far in the enclosing [`scope`]; empty outside one.
pub fn snapshot() -> Vec<CallEntry> {
    SINK.try_with(|s| s.lock().map(|g| g.clone()).unwrap_or_default()).unwrap_or_default()
}

/// Whether a [`scope`] is open, so a caller can skip work only a scope needs.
pub(crate) fn active() -> bool {
    SINK.try_with(|_| ()).is_ok()
}

/// Notes one call; a no-op outside a scope.
pub(crate) fn note(entry: CallEntry) {
    let _ = SINK.try_with(|s| {
        if let Ok(mut g) = s.lock() {
            g.push(entry);
        }
    });
}

/// What the request looked like before it was sent: method, path, query and
/// the JSON body if there is a small one.
pub(crate) struct Sent {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub body: Option<serde_json::Value>,
}

/// Reads `request` without consuming it. `None` for a body that cannot be
/// cloned (a stream), which is read as "nothing to cite".
pub(crate) fn peek(request: &reqwest::RequestBuilder) -> Option<Sent> {
    let built = request.try_clone()?.build().ok()?;
    let body = built
        .body()
        .and_then(|b| b.as_bytes())
        .filter(|b| !b.is_empty() && b.len() <= SENT_CAP_BYTES)
        .and_then(|b| serde_json::from_slice(b).ok());
    Some(Sent {
        method: built.method().as_str().to_string(),
        path: built.url().path().to_string(),
        query: built.url().query().map(str::to_string),
        body,
    })
}

/// The entry for an answer, from the headers Core stamped on it.
pub(crate) fn entry(sent: Sent, status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> CallEntry {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    CallEntry {
        method: sent.method,
        path: sent.path,
        status: status.as_u16(),
        seq: text(SEQ_HEADER).and_then(|s| s.parse().ok()),
        hash: text(HASH_HEADER),
        repeat: text(REPEAT_HEADER).and_then(|s| s.parse().ok()),
        sent: sent.body,
        query: sent.query,
    }
}
