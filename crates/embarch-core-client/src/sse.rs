//! A byte-fed `text/event-stream` decoder — no HTTP, no async, no I/O.
//!
//! Split out from [`crate::study_events`] deliberately: the interesting
//! failure modes of an SSE consumer are all framing (a frame split across two
//! TCP reads, a `\r\n` straddling that split, a keep-alive comment between
//! two real frames, a multi-line `data:`), and every one of them is a pure
//! function of a byte sequence. Pushing bytes in and pulling frames out means
//! those cases are tested by handing this a `&[u8]`, with no socket and no
//! runtime — the same posture `embarch-core`'s own `validate_study` takes
//! toward its handler.
//!
//! This is the [WHATWG SSE][spec] wire format, restricted to what
//! `embarch-core` actually emits: `axum`'s `Sse` writes `event:`, `data:`
//! and `id:` fields and a `:` keep-alive comment, and nothing else.
//! `retry:` is parsed to the extent of being ignored, since this client does
//! not reconnect (see `study_events`' module docs for why).
//!
//! [spec]: https://html.spec.whatwg.org/multipage/server-sent-events.html

/// One dispatched SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` field, or `None` for the spec's default (`message`).
    /// Kept as `None` rather than materialised as `"message"` so a caller
    /// can tell "Core named this frame" from "Core did not" — which is
    /// exactly the difference between an `event: lagged` frame and a normal
    /// `StudyEvent`.
    pub event: Option<String>,
    /// The `data:` field(s), joined with `\n` and with the trailing newline
    /// removed, per spec.
    pub data: String,
    /// The `id:` field. Core never sets one — recorded rather than dropped
    /// so that a future Core growing `Last-Event-ID` replay does not need
    /// this parser changed to notice it.
    pub id: Option<String>,
}

impl SseFrame {
    /// The frame's event type as the spec defines it: the `event:` field if
    /// present, else `message`.
    pub fn event_type(&self) -> &str {
        self.event.as_deref().unwrap_or("message")
    }
}

/// A single line longer than this aborts the stream.
///
/// Nothing `embarch-core` sends comes close: the largest frame is a
/// `StepCompleted`, bounded by `embarch-study-designer`'s
/// `MAX_PAYLOAD_LEN`/`MAX_DISCOVERED_SERVICES` at tens of KB. The cap exists
/// because a peer that never sends a newline would otherwise grow this
/// buffer without bound, and "the client ate all the memory" is a worse
/// failure than "the client gave up on the stream" — the latter falls back
/// to polling, which is a complete answer.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Feed it bytes, take out frames.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes received and not yet resolved into a complete line.
    buf: Vec<u8>,
    /// Fields accumulated for the frame currently being built.
    event: Option<String>,
    data: String,
    id: Option<String>,
    /// Whether any `data:` field has been seen for the frame being built.
    /// Tracked separately from `data.is_empty()` because `data:` with an
    /// empty value is a real, dispatchable frame and an empty accumulator is
    /// not.
    saw_data: bool,
    /// Set once a line exceeded [`MAX_LINE_BYTES`]; the decoder is spent.
    overflowed: bool,
}

impl SseDecoder {
    pub fn new() -> SseDecoder {
        SseDecoder::default()
    }

    /// Add received bytes. Call [`SseDecoder::next_frame`] until it returns
    /// `None` before reading more.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` when the buffered bytes do not yet
    /// contain one.
    ///
    /// `Err` means the stream is unusable (a line past [`MAX_LINE_BYTES`]),
    /// not that a frame was malformed — a frame this parser does not
    /// understand is still handed back, with its unknown fields dropped, per
    /// the spec's "ignore the field" rule.
    pub fn next_frame(&mut self) -> Result<Option<SseFrame>, SseError> {
        loop {
            if self.overflowed {
                return Err(SseError::LineTooLong);
            }
            let Some(line) = self.take_line()? else {
                return Ok(None);
            };

            // A blank line dispatches whatever has accumulated. Per spec a
            // frame with no `data:` field at all is *not* dispatched — that
            // is what makes `axum`'s `:` keep-alive (a comment, then a blank
            // line) invisible here rather than a stream of empty frames.
            if line.is_empty() {
                let dispatched = self.take_frame();
                if let Some(frame) = dispatched {
                    return Ok(Some(frame));
                }
                continue;
            }

            // A line starting with `:` is a comment. `axum`'s keep-alive is
            // exactly this, every 15 s by its own default, and its only job
            // here is to prove the socket is alive — which it does simply by
            // arriving, since the idle budget is measured on bytes received.
            if line.starts_with(':') {
                continue;
            }

            let (field, value) = match line.split_once(':') {
                // Spec: a single leading space after the colon is part of
                // the delimiter, not the value. Exactly one.
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                // A line with no colon is a field with an empty value.
                None => (line.as_str(), ""),
            };

            match field {
                "event" => self.event = Some(value.to_string()),
                "data" => {
                    if self.saw_data {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                    self.saw_data = true;
                }
                "id" => self.id = Some(value.to_string()),
                // `retry:` is a reconnection hint and this client does not
                // reconnect; every other field name the spec says to ignore.
                _ => {}
            }
        }
    }

    /// Dispatch whatever is still buffered, for a stream that ended without
    /// a final blank line.
    ///
    /// Per spec the trailing incomplete frame is discarded, and that is what
    /// this does for a *partial* one; it exists so a caller can ask, once,
    /// after the socket closed, and get `None` rather than having to reason
    /// about whether it should.
    pub fn finish(&mut self) -> Option<SseFrame> {
        // A frame whose blank line never arrived is incomplete by
        // definition — the bytes after the last `\n\n` may be half a JSON
        // document. Handing that to `serde_json` would report a parse error
        // for what is really a truncated stream, so it is dropped and the
        // truncation is reported as the disconnect it is.
        self.buf.clear();
        self.event = None;
        self.data.clear();
        self.id = None;
        self.saw_data = false;
        None
    }

    /// Whether anything at all is buffered — a partial frame at the moment
    /// the socket closed, which is worth telling the caller about because it
    /// distinguishes "Core finished cleanly" from "the connection was cut
    /// mid-frame".
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty() || self.saw_data || self.event.is_some()
    }

    /// Build and reset, or `None` if no `data:` field was seen.
    fn take_frame(&mut self) -> Option<SseFrame> {
        let event = self.event.take();
        let id = self.id.take();
        let data = std::mem::take(&mut self.data);
        let saw_data = std::mem::take(&mut self.saw_data);
        if !saw_data {
            return None;
        }
        Some(SseFrame { event, data, id })
    }

    /// One line, terminator consumed, or `None` if the buffer holds no
    /// complete line yet.
    ///
    /// The `\r`-at-the-end case is the one worth naming: a chunk ending in a
    /// bare `\r` is ambiguous — it is either a CR terminator or the first
    /// half of a CRLF — so it is held back rather than guessed at. Guessing
    /// would turn one `\r\n` split across two TCP reads into a spurious
    /// blank line, and a spurious blank line dispatches a frame early.
    fn take_line(&mut self) -> Result<Option<String>, SseError> {
        let end = self
            .buf
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r');
        let Some(end) = end else {
            if self.buf.len() > MAX_LINE_BYTES {
                self.overflowed = true;
                return Err(SseError::LineTooLong);
            }
            return Ok(None);
        };

        let terminator = if self.buf[end] == b'\r' {
            match self.buf.get(end + 1) {
                Some(b'\n') => 2,
                // Ambiguous: wait for the byte that settles it.
                None => return Ok(None),
                Some(_) => 1,
            }
        } else {
            1
        };

        // Per line rather than per buffer, so a UTF-8 character split across
        // two TCP reads is reassembled before it is decoded — the bytes were
        // held in `buf` until the line was complete.
        let line = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf.drain(..end + terminator);
        Ok(Some(line))
    }
}

/// The one way this decoder gives up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseError {
    /// A single line exceeded [`MAX_LINE_BYTES`].
    LineTooLong,
}

impl std::fmt::Display for SseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SseError::LineTooLong => write!(
                f,
                "an SSE line exceeded {MAX_LINE_BYTES} bytes without a newline"
            ),
        }
    }
}

impl std::error::Error for SseError {}
