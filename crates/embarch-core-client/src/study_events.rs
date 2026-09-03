//! Consuming `GET /study/{study_id}/events` — Core's live-push companion to
//! polling `GET /study/{study_id}`.
//!
//! # What Core sends
//!
//! One JSON [`StudyEvent`] per SSE frame, filtered to the `study_id` in the
//! URL, pushed the instant Core processes it (`embarch-core/interfaces.md`,
//! and `embarch-core/decisions/studies.md`: "nothing about a result is held
//! back until the study finishes, at any layer"). Two frames are *not* a
//! `StudyEvent`:
//!
//! - **`event: lagged`**, whose data is a decimal count. Core emits this
//!   deliberately when a subscriber falls behind its broadcast channel's
//!   buffer, in preference to silently skipping messages. It is a fact about
//!   this client, not a fault in the stream, and is surfaced as
//!   [`StudyStreamItem::Lagged`] — never as an error.
//! - **`event: encode-error`**, if Core ever fails to serialise an event it
//!   already holds. Reported as [`StudyStreamItem::Unrecognized`] alongside
//!   any frame this build cannot make sense of.
//!
//! # Why there is no reconnect
//!
//! Core's SSE handler subscribes to a `tokio::sync::broadcast` channel and
//! reads no `Last-Event-ID`. A reconnect would therefore resume at "now"
//! with a hole of unknown size and no way to detect it — the precise thing
//! `lagged` exists to stop happening quietly. So a dropped stream falls back
//! to **polling `GET /study/{study_id}`**, which reads the authoritative
//! record rather than a live tap, and cannot silently skip anything.
//! [`CoreClient::follow_study`] is that fallback, and it is why a
//! disconnected stream is a mode change rather than a failed call.
//!
//! # Why the stream never ends on its own
//!
//! The broadcast channel is process-wide and outlives any one study, so Core
//! holds the connection open indefinitely (with a keep-alive comment on
//! `axum`'s 15 s default). A follower must therefore decide for itself when
//! it is done: [`CoreClient::follow_study`] stops on a terminal
//! `StatusChanged`, and — because a study that finished *before* the
//! subscribe would otherwise never produce one — polls the status once
//! immediately after opening the stream.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use embarch_study_designer::{GattTranscriptEntry, Sample, StepResult};
use serde::{Deserialize, Serialize};

use crate::client::CoreClient;
use crate::sse::{SseDecoder, SseFrame};

/// Core's own `StudyEvent` (`embarch-core/src/study.rs`), deserialize side.
///
/// A mirror rather than a shared type because Core's copy lives in Core's
/// binary crate and is `Serialize`-only; lifting it into
/// `embarch-study-designer` would be a cross-repo wire change, which is not
/// a single sub-project's to make. The cost of the mirror is that a variant
/// added to Core and not here is not decoded — which is exactly why an
/// unknown `kind` is [`StudyStreamItem::Unrecognized`] and not an error.
/// `Serialize` as well as `Deserialize` so a consumer can hand an event
/// straight back out — the CLI's `--json` line and the MCP tool's event
/// array both re-emit Core's own shape verbatim rather than a lossy
/// re-rendering of it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum StudyEvent {
    /// A step finished. Carries the same `StepResult` Core just appended to
    /// `events.json`.
    StepCompleted {
        study_id: String,
        step_index: u32,
        result: Box<StepResult>,
    },
    /// One batch of samples off a declared tap, keyed by the tap's index and
    /// name in `Study.streams`.
    SampleBatch {
        study_id: String,
        stream_id: u8,
        stream_name: String,
        samples: Vec<Sample>,
    },
    /// One GATT transcript entry.
    ///
    /// Present in Core and **absent from `embarch-core/interfaces.md`'s
    /// summary of this route**, which names only the other three. Decoded
    /// here because Core emits it.
    GattTranscript {
        study_id: String,
        step_index: u32,
        entry: Box<GattTranscriptEntry>,
    },
    /// The job's own status changed — `"completed"` or `"failed"`.
    StatusChanged {
        study_id: String,
        status: String,
        reason: Option<String>,
    },
}

impl StudyEvent {
    pub fn study_id(&self) -> &str {
        match self {
            StudyEvent::StepCompleted { study_id, .. }
            | StudyEvent::SampleBatch { study_id, .. }
            | StudyEvent::GattTranscript { study_id, .. }
            | StudyEvent::StatusChanged { study_id, .. } => study_id,
        }
    }

    /// The `kind` tag, for a caller rendering the event without matching on
    /// every variant.
    pub fn kind(&self) -> &'static str {
        match self {
            StudyEvent::StepCompleted { .. } => "StepCompleted",
            StudyEvent::SampleBatch { .. } => "SampleBatch",
            StudyEvent::GattTranscript { .. } => "GattTranscript",
            StudyEvent::StatusChanged { .. } => "StatusChanged",
        }
    }
}

/// A study status Core will not move off by itself.
pub fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

/// One thing that came off the SSE stream.
#[derive(Debug, Clone)]
pub enum StudyStreamItem {
    Event(StudyEvent),
    /// Core dropped `missed` events for this subscriber because it could not
    /// keep up. **Not an error.** The events are gone from the live tap; the
    /// study's own record on disk is unaffected, so the complete answer is
    /// still `GET /study/{id}` once it finishes.
    Lagged { missed: u64 },
    /// A frame this build could not turn into a [`StudyEvent`] — an unknown
    /// `event:` name, an unknown `kind`, or data that is not the JSON this
    /// expects. Forwarded rather than dropped or fatal: a newer Core that
    /// grows a variant should degrade to "there was an event I did not
    /// understand", not to a broken client.
    Unrecognized {
        event: String,
        data: String,
        reason: String,
    },
}

/// Why an SSE stream stopped producing items.
#[derive(Debug, Clone)]
pub struct StreamEnd {
    pub reason: String,
    /// Whether bytes of an undispatched frame were still buffered — i.e.
    /// the connection was cut mid-frame rather than between frames.
    pub truncated: bool,
}

/// [`StudyEventStream::next`]'s two outcomes, as a type, so that "the stream
/// ended" cannot be mistaken for an error by a caller that only checks
/// `Result`.
#[derive(Debug, Clone)]
pub enum StudyStreamNext {
    Item(StudyStreamItem),
    Ended(StreamEnd),
}

/// An open subscription to one study's events.
pub struct StudyEventStream {
    response: reqwest::Response,
    decoder: SseDecoder,
    idle_timeout: Duration,
    ended: Option<StreamEnd>,
}

impl StudyEventStream {
    /// The next item, or the end of the stream.
    ///
    /// Never returns `Err` for a transport problem — a dropped socket, a
    /// stalled peer and a clean close are all [`StudyStreamNext::Ended`],
    /// because the caller's correct response to all three is the same and it
    /// is not "fail".
    pub async fn next(&mut self) -> StudyStreamNext {
        if let Some(end) = &self.ended {
            return StudyStreamNext::Ended(end.clone());
        }
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => {
                    if let Some(item) = interpret(&frame) {
                        return StudyStreamNext::Item(item);
                    }
                    continue;
                }
                Ok(None) => {}
                Err(e) => return self.end(e.to_string()),
            }

            // The idle budget is measured on *any* byte arriving, which
            // `axum`'s keep-alive comment satisfies every 15 s by its own
            // default. So this fires only on a peer that has genuinely
            // stopped talking, and the default leaves room for three missed
            // keep-alives before it does.
            let chunk = match tokio::time::timeout(self.idle_timeout, self.response.chunk()).await {
                Err(_) => {
                    return self.end(format!(
                        "no bytes from embarch-core for {:?} (not even a keep-alive)",
                        self.idle_timeout
                    ))
                }
                Ok(Err(e)) => return self.end(format!("the event stream's connection failed: {e}")),
                Ok(Ok(None)) => return self.end("embarch-core closed the event stream".to_string()),
                Ok(Ok(Some(chunk))) => chunk,
            };
            self.decoder.push(&chunk);
        }
    }

    fn end(&mut self, reason: String) -> StudyStreamNext {
        let truncated = self.decoder.has_partial();
        self.decoder.finish();
        let end = StreamEnd { reason, truncated };
        self.ended = Some(end.clone());
        StudyStreamNext::Ended(end)
    }
}

/// One SSE frame to one [`StudyStreamItem`], or `None` for a frame that
/// carries nothing (which the decoder already filters, but which is cheap to
/// be safe about).
fn interpret(frame: &SseFrame) -> Option<StudyStreamItem> {
    match frame.event_type() {
        // Core's default-typed frames are the events themselves.
        "message" => match serde_json::from_str::<StudyEvent>(&frame.data) {
            Ok(event) => Some(StudyStreamItem::Event(event)),
            Err(e) => Some(StudyStreamItem::Unrecognized {
                event: "message".to_string(),
                data: frame.data.clone(),
                reason: format!("not a StudyEvent this build understands: {e}"),
            }),
        },
        "lagged" => match frame.data.trim().parse::<u64>() {
            Ok(missed) => Some(StudyStreamItem::Lagged { missed }),
            // Core writes `n.to_string()` for a `u64`, so this cannot
            // happen against today's Core; reported rather than assumed
            // away, and still not an error — a lagged frame whose count is
            // unreadable still means events were lost.
            Err(_) => Some(StudyStreamItem::Unrecognized {
                event: "lagged".to_string(),
                data: frame.data.clone(),
                reason: "a lagged frame whose count is not a number — events were still lost"
                    .to_string(),
            }),
        },
        other => Some(StudyStreamItem::Unrecognized {
            event: other.to_string(),
            data: frame.data.clone(),
            reason: format!("embarch-core sent an SSE frame typed '{other}', which this build does not handle"),
        }),
    }
}

/// How [`CoreClient::follow_study`] behaves.
#[derive(Debug, Clone)]
pub struct FollowOptions {
    /// Give up on the live stream if no byte at all arrives in this long.
    /// Must comfortably exceed Core's keep-alive interval (`axum`'s 15 s
    /// default) or a quiet study looks like a dead socket.
    pub idle_timeout: Duration,
    /// Cadence once fallen back to polling.
    pub poll_interval: Duration,
    /// Overall budget. `None` follows until the study reaches a terminal
    /// status, which is what a CLI user watching a run wants and what an
    /// MCP tool must never do.
    pub deadline: Option<Duration>,
}

impl Default for FollowOptions {
    fn default() -> FollowOptions {
        FollowOptions {
            idle_timeout: Duration::from_secs(45),
            poll_interval: Duration::from_secs(2),
            deadline: None,
        }
    }
}

/// Which transport a [`FollowItem::Transport`] is announcing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowMode {
    /// The live SSE stream.
    Live,
    /// Polling `GET /study/{id}` — either because the stream would not open
    /// or because it dropped.
    Polling,
}

impl FollowMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FollowMode::Live => "live",
            FollowMode::Polling => "polling",
        }
    }
}

/// What a follower emits to its caller.
#[derive(Debug, Clone)]
pub enum FollowItem {
    Event(StudyEvent),
    Lagged {
        missed: u64,
    },
    Unrecognized {
        event: String,
        data: String,
        reason: String,
    },
    /// The transport changed, and why. Emitted once on opening the stream
    /// and once on every fallback, so a log of a follow always says which
    /// mechanism produced the lines around it.
    Transport {
        mode: FollowMode,
        detail: String,
    },
    /// A polled snapshot, emitted only when something in it changed.
    Polled {
        status: String,
        current_step: Option<u32>,
        total_steps: Option<u32>,
        reason: Option<String>,
    },
}

impl FollowItem {
    /// This item as one JSON object, with a `type` discriminator.
    ///
    /// Lives here rather than in either consumer because there are two —
    /// `embarch-api`'s `study-status --follow` writes one of these per line
    /// and its `study_watch` MCP tool returns an array of them — and two
    /// hand-written renderings of the same items would drift. The shape is
    /// the contract both of those surfaces publish.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            FollowItem::Event(event) => serde_json::json!({
                "type": "event",
                // Core's own frame, unchanged, `kind` tag and all.
                "event": serde_json::to_value(event)
                    .unwrap_or(serde_json::Value::Null),
            }),
            FollowItem::Lagged { missed } => serde_json::json!({
                "type": "lagged",
                "missed": missed,
                "note": LAGGED_NOTE,
            }),
            FollowItem::Unrecognized { event, data, reason } => serde_json::json!({
                "type": "unrecognized",
                "event": event,
                "reason": reason,
                "data": data,
            }),
            FollowItem::Transport { mode, detail } => serde_json::json!({
                "type": "transport",
                "mode": mode.as_str(),
                "detail": detail,
            }),
            FollowItem::Polled { status, current_step, total_steps, reason } => serde_json::json!({
                "type": "polled",
                "status": status,
                "current_step": current_step,
                "total_steps": total_steps,
                "reason": reason,
            }),
        }
    }
}

/// What a `lagged` frame actually means for the caller, said once so both
/// surfaces say the same thing.
///
/// The second half is the part that matters and the part an agent will
/// otherwise get wrong: lagging costs you the *live* copy of some events,
/// and costs the study's own record nothing.
pub const LAGGED_NOTE: &str = "embarch-core dropped events for this subscriber because it \
     could not keep up. This is not a stream error and the study is unaffected — the events \
     are missing from this live feed only. GET /study/{id} and /study/{id}/steps still hold \
     the complete record.";

/// How a follow finished.
#[derive(Debug, Clone, Default)]
pub struct FollowOutcome {
    /// `"completed"`/`"failed"` if the study reached one within the budget.
    pub terminal_status: Option<String>,
    pub reason: Option<String>,
    /// Total events Core told us it dropped. Non-zero means the emitted
    /// event list has holes, and `GET /study/{id}` is the complete record.
    pub lagged_events: u64,
    /// Whether the follow ever fell back to polling.
    pub used_polling: bool,
    /// Whether the deadline expired before a terminal status.
    pub timed_out: bool,
}

impl CoreClient {
    /// Open a subscription to `GET /study/{study_id}/events`.
    ///
    /// Deliberately sets **no** request timeout, unlike every other method
    /// on this client: `study_timeout_secs` is a bound on a request that
    /// answers, and `reqwest`'s per-request timeout covers the body too, so
    /// applying it here would cut a healthy stream off after 30 s. The
    /// bound that replaces it is [`FollowOptions::idle_timeout`], applied
    /// per read.
    pub async fn open_study_events(
        &self,
        study_id: &str,
        idle_timeout: Duration,
    ) -> Result<StudyEventStream> {
        let url = format!("{}/study/{study_id}/events", self.base_url().await?);
        let response = self
            .http()
            .get(url)
            .bearer_auth(self.bearer_token())
            .header("Accept", "text/event-stream")
            .send()
            .await
            .context("could not open embarch-core's study event stream")?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no response body>".to_string());
            return Err(anyhow!(
                "embarch-core returned {status} for the event stream: {body}"
            ));
        }

        Ok(StudyEventStream {
            response,
            decoder: SseDecoder::new(),
            idle_timeout,
            ended: None,
        })
    }

    /// Follow a study to its end, live where possible and by polling where
    /// not, handing each item to `on_item` as it happens.
    ///
    /// A synchronous `FnMut` rather than a `Stream` because both callers
    /// want a side effect per item — the CLI prints a line, the MCP tool
    /// pushes onto a `Vec` — and neither wants this crate to grow a
    /// `futures` dependency to express that.
    ///
    /// The order of the first two steps is load-bearing: **subscribe, then
    /// poll.** Polling first would leave a gap in which an event could be
    /// broadcast and missed; subscribing first means the initial poll can
    /// only ever tell us something the stream will also tell us, and its
    /// real job is catching the study that was already over before we
    /// arrived — for which no `StatusChanged` will ever come.
    pub async fn follow_study<F>(
        &self,
        study_id: &str,
        options: &FollowOptions,
        mut on_item: F,
    ) -> Result<FollowOutcome>
    where
        F: FnMut(FollowItem),
    {
        let started = Instant::now();
        let remaining = || -> Option<Duration> {
            options
                .deadline
                .map(|budget| budget.saturating_sub(started.elapsed()))
        };
        let mut outcome = FollowOutcome::default();

        let stream = match self
            .open_study_events(study_id, options.idle_timeout)
            .await
        {
            Ok(stream) => {
                on_item(FollowItem::Transport {
                    mode: FollowMode::Live,
                    detail: format!("subscribed to GET /study/{study_id}/events"),
                });
                Some(stream)
            }
            Err(e) => {
                // The one failure that is genuinely fatal is "there is no
                // such study", and polling reports that far better than
                // this does — so even a refused subscription falls through
                // rather than failing the call.
                on_item(FollowItem::Transport {
                    mode: FollowMode::Polling,
                    detail: format!("the event stream would not open ({e:#}); polling instead"),
                });
                outcome.used_polling = true;
                None
            }
        };

        // The already-finished case. A failure here is not fatal: it may
        // just mean Core is briefly busy, and the live stream — if we have
        // one — is still the better source.
        match self.get_study_status(study_id).await {
            Ok(snapshot) => {
                if is_terminal_status(&snapshot.status) {
                    on_item(FollowItem::Polled {
                        status: snapshot.status.clone(),
                        current_step: snapshot.current_step,
                        total_steps: snapshot.total_steps,
                        reason: snapshot.reason.clone(),
                    });
                    outcome.terminal_status = Some(snapshot.status);
                    outcome.reason = snapshot.reason;
                    return Ok(outcome);
                }
            }
            Err(e) => {
                if stream.is_none() {
                    return Err(e).with_context(|| {
                        format!(
                            "the event stream would not open and neither would \
                             GET /study/{study_id}"
                        )
                    });
                }
                tracing::debug!("initial status poll for {study_id} failed, following live: {e:#}");
            }
        }

        if let Some(mut stream) = stream {
            loop {
                if let Some(left) = remaining() {
                    if left.is_zero() {
                        outcome.timed_out = true;
                        return Ok(outcome);
                    }
                }
                let next = match remaining() {
                    Some(left) => match tokio::time::timeout(left, stream.next()).await {
                        Ok(next) => next,
                        Err(_) => {
                            outcome.timed_out = true;
                            return Ok(outcome);
                        }
                    },
                    None => stream.next().await,
                };

                match next {
                    StudyStreamNext::Item(StudyStreamItem::Event(event)) => {
                        let terminal = match &event {
                            StudyEvent::StatusChanged { status, reason, .. }
                                if is_terminal_status(status) =>
                            {
                                Some((status.clone(), reason.clone()))
                            }
                            _ => None,
                        };
                        on_item(FollowItem::Event(event));
                        if let Some((status, reason)) = terminal {
                            outcome.terminal_status = Some(status);
                            outcome.reason = reason;
                            return Ok(outcome);
                        }
                    }
                    StudyStreamNext::Item(StudyStreamItem::Lagged { missed }) => {
                        outcome.lagged_events = outcome.lagged_events.saturating_add(missed);
                        on_item(FollowItem::Lagged { missed });
                    }
                    StudyStreamNext::Item(StudyStreamItem::Unrecognized {
                        event,
                        data,
                        reason,
                    }) => on_item(FollowItem::Unrecognized {
                        event,
                        data,
                        reason,
                    }),
                    StudyStreamNext::Ended(end) => {
                        let cut = if end.truncated {
                            " mid-frame"
                        } else {
                            ""
                        };
                        on_item(FollowItem::Transport {
                            mode: FollowMode::Polling,
                            detail: format!(
                                "the live stream ended{cut} ({}); polling GET /study/{study_id} \
                                 instead — embarch-core keeps no replay, so polling the record \
                                 is the only way not to skip anything silently",
                                end.reason
                            ),
                        });
                        outcome.used_polling = true;
                        break;
                    }
                }
            }
        }

        // Polling fallback. Every path that reaches here has already told
        // the caller it is polling and why.
        let mut last: Option<(String, Option<u32>)> = None;
        loop {
            if let Some(left) = remaining() {
                if left.is_zero() {
                    outcome.timed_out = true;
                    return Ok(outcome);
                }
            }
            let snapshot = self.get_study_status(study_id).await.with_context(|| {
                format!("polling GET /study/{study_id} failed after the event stream ended")
            })?;
            let key = (snapshot.status.clone(), snapshot.current_step);
            if last.as_ref() != Some(&key) {
                on_item(FollowItem::Polled {
                    status: snapshot.status.clone(),
                    current_step: snapshot.current_step,
                    total_steps: snapshot.total_steps,
                    reason: snapshot.reason.clone(),
                });
                last = Some(key);
            }
            if is_terminal_status(&snapshot.status) {
                outcome.terminal_status = Some(snapshot.status);
                outcome.reason = snapshot.reason;
                return Ok(outcome);
            }
            let wait = match remaining() {
                Some(left) => options.poll_interval.min(left),
                None => options.poll_interval,
            };
            tokio::time::sleep(wait).await;
        }
    }
}
