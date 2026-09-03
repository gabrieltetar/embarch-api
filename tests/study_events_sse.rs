//! `GET /study/{id}/events` — the SSE client, against a mock embarch-core.
//!
//! # What these tests can and cannot prove
//!
//! Everything here runs against `support::MockCore` on a loopback socket, so
//! it is hardware-free and deterministic. That covers the whole host side:
//! framing, `lagged`, an unknown frame, a dropped connection, the polling
//! fallback, and the ordering that makes an already-finished study return
//! instead of hanging.
//!
//! It does **not** prove interoperability with the real Core. The frames the
//! mock writes are reproduced from `axum`'s own `Event::field`
//! (`name`, `:`, one space, value, `\n`; one more `\n` to finalize) and from
//! `KeepAlive::DEFAULT_KEEP_ALIVE` (`b":\n\n"`), and the JSON bodies from
//! `embarch-core`'s `StudyEvent` — but a fixture is a copy of a wire format,
//! not the wire format. `tasks/api/001-sse-client.md` records the
//! hardware-verification debt that closes that gap.
//!
//! In particular **`lagged` is provoked here by writing the frame, not by
//! outrunning a real broadcast channel.** Making Core actually lag needs a
//! study producing samples faster than a subscriber drains them, which needs
//! a board. What is proven here is the half that was actually at risk: that
//! the frame is recognised, reported as a fact rather than an error, counted,
//! and that the stream keeps going after one.

mod support;

use std::time::{Duration, Instant};

use embarch_core_client::sse::{SseDecoder, SseFrame};
use embarch_core_client::{
    CoreClient, CoreConfig, FollowItem, FollowMode, FollowOptions, StudyEvent,
};
use serde_json::json;
use support::{sse_frame, sse_keep_alive, Behavior, MockCore, StreamTail};

const TOKEN: &str = "mocked-core-token-3f9a1c";
const STUDY: &str = "9f2c40aa11";

fn config(base_url: &str) -> CoreConfig {
    serde_json::from_value(json!({ "token": TOKEN, "base_url": base_url }))
        .expect("test CoreConfig did not deserialize")
}

/// Options with a short deadline and a fast poll, so a test that must wait
/// for the fallback finishes in milliseconds rather than seconds.
fn fast_options(deadline_ms: u64) -> FollowOptions {
    FollowOptions {
        idle_timeout: Duration::from_millis(500),
        poll_interval: Duration::from_millis(10),
        deadline: Some(Duration::from_millis(deadline_ms)),
    }
}

/// A `StepCompleted` frame's JSON, as `embarch-core` serializes it.
/// `gatt_services`/`security_level`/`protocol` carry `#[serde(default)]` and
/// are omitted here deliberately — a real Core sends them, and a client that
/// only decoded the fully-populated shape would be pinned to today's Core.
fn step_completed(step_index: u32, name: &str) -> String {
    json!({
        "kind": "StepCompleted",
        "study_id": STUDY,
        "step_index": step_index,
        "result": { "step_name": name, "outcome": "Pass", "captured_data": null },
    })
    .to_string()
}

fn status_changed(status: &str, reason: Option<&str>) -> String {
    json!({
        "kind": "StatusChanged",
        "study_id": STUDY,
        "status": status,
        "reason": reason,
    })
    .to_string()
}

fn sample_batch(stream_name: &str, count: usize) -> String {
    let samples: Vec<_> = (0..count)
        .map(|i| json!({ "rx_utc_ms": 1_000 + i as u64, "value": 1.5, "unit": "Milliamps", "channel_id": 0 }))
        .collect();
    json!({
        "kind": "SampleBatch",
        "study_id": STUDY,
        "stream_id": 0,
        "stream_name": stream_name,
        "samples": samples,
    })
    .to_string()
}

/// `GET /study/{id}` with the given status.
fn poll_reply(status: &str) -> Behavior {
    Behavior::json_ok(json!({
        "status": status,
        "current_step": 1,
        "total_steps": 3,
        "result": null,
        "reason": null,
    }))
}

/// The two routes a follow touches, wired to one behavior each.
fn routed(events: Behavior, poll: Behavior) -> Behavior {
    Behavior::Router {
        // `/events` first: `/study/{id}/events` also ends with nothing that
        // would match the bare study path, but ordering it first makes the
        // intent unambiguous rather than dependent on that.
        routes: vec![("/events".to_string(), events)],
        otherwise: Box::new(poll),
    }
}

fn stream(chunks: Vec<Vec<u8>>, then: StreamTail) -> Behavior {
    Behavior::EventStream {
        chunks,
        gap: Duration::from_millis(1),
        then,
    }
}

// ---------------------------------------------------------------------------
// The decoder, with no socket at all
// ---------------------------------------------------------------------------

/// A frame arriving one byte at a time still decodes to exactly one frame.
///
/// This is the failure a naive `split("\n\n")` over each TCP read has, and it
/// is not hypothetical: a `SampleBatch` is comfortably larger than one
/// segment.
#[test]
fn a_frame_split_across_single_byte_reads_decodes_once() {
    let bytes = sse_frame(None, &status_changed("completed", None));
    let mut decoder = SseDecoder::new();
    let mut frames = Vec::new();

    for byte in &bytes {
        decoder.push(&[*byte]);
        while let Some(frame) = decoder.next_frame().expect("decoder gave up") {
            frames.push(frame);
        }
    }

    assert_eq!(frames.len(), 1, "expected exactly one frame, got {frames:?}");
    assert_eq!(frames[0].event_type(), "message");
    assert_eq!(frames[0].data, status_changed("completed", None));
}

/// A `\r\n` straddling two reads must not look like a blank line.
///
/// A blank line dispatches a frame, so guessing that a trailing `\r` is a
/// terminator would cut a frame in half and hand `serde_json` a truncated
/// document — reported as a parse error for what is really a framing bug.
#[test]
fn a_crlf_split_across_two_reads_is_one_terminator() {
    let mut decoder = SseDecoder::new();
    decoder.push(b"data: one\r");
    assert!(
        decoder.next_frame().expect("decoder gave up").is_none(),
        "a lone trailing \\r is ambiguous and must be held back"
    );
    decoder.push(b"\ndata: two\r\n\r\n");

    let frame = decoder
        .next_frame()
        .expect("decoder gave up")
        .expect("a complete frame should have been dispatched");
    assert_eq!(
        frame.data, "one\ntwo",
        "two data: fields join with a newline, per spec"
    );
}

/// `axum`'s keep-alive is a comment plus a blank line. It must produce no
/// frame at all — a client that emitted an empty event every 15 s would look
/// like a study doing something when it is idle.
#[test]
fn keep_alive_comments_produce_no_frames() {
    let mut decoder = SseDecoder::new();
    decoder.push(&sse_keep_alive());
    decoder.push(&sse_keep_alive());
    assert!(decoder.next_frame().expect("decoder gave up").is_none());

    decoder.push(&sse_frame(None, "{}"));
    let frame = decoder.next_frame().expect("decoder gave up");
    assert_eq!(
        frame,
        Some(SseFrame {
            event: None,
            data: "{}".to_string(),
            id: None
        })
    );
}

/// The exact bytes Core writes for a lagged frame decode to the lagged
/// event type. Pinned separately from the end-to-end test because this is
/// the one assertion that is really about `axum`'s output format.
#[test]
fn a_lagged_frame_decodes_as_the_lagged_event_type() {
    let mut decoder = SseDecoder::new();
    decoder.push(b"event: lagged\ndata: 7\n\n");
    let frame = decoder
        .next_frame()
        .expect("decoder gave up")
        .expect("a lagged frame is a complete frame");
    assert_eq!(frame.event_type(), "lagged");
    assert_eq!(frame.data, "7");
}

// ---------------------------------------------------------------------------
// `lagged`, end to end
// ---------------------------------------------------------------------------

/// The requirement this whole task turns on: `event: lagged` is a reported
/// fact, the stream continues past it, and the call succeeds.
#[tokio::test]
async fn a_lagged_frame_is_reported_and_the_stream_continues() {
    let mock = MockCore::start(routed(
        stream(
            vec![
                sse_frame(None, &step_completed(0, "connect")),
                // Core's own `Event::default().event("lagged").data(n.to_string())`.
                sse_frame(Some("lagged"), "12"),
                sse_frame(None, &step_completed(1, "write")),
                sse_frame(None, &status_changed("completed", None)),
            ],
            StreamTail::Hold,
        ),
        poll_reply("running"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let mut items = Vec::new();
    let outcome = core
        .follow_study(STUDY, &fast_options(5_000), |item| items.push(item))
        .await
        .expect("a lagged frame must not fail the call");

    let lagged: Vec<u64> = items
        .iter()
        .filter_map(|item| match item {
            FollowItem::Lagged { missed } => Some(*missed),
            _ => None,
        })
        .collect();
    assert_eq!(lagged, vec![12], "the lagged count must be reported as-is");
    assert_eq!(outcome.lagged_events, 12);

    let steps: Vec<u32> = items
        .iter()
        .filter_map(|item| match item {
            FollowItem::Event(StudyEvent::StepCompleted { step_index, .. }) => Some(*step_index),
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        vec![0, 1],
        "the step after the lagged frame must still arrive"
    );

    assert_eq!(outcome.terminal_status.as_deref(), Some("completed"));
    assert!(
        !outcome.used_polling,
        "nothing here should have needed the fallback"
    );
}

/// A lagged frame whose count is unreadable still means events were lost, so
/// it is surfaced rather than dropped — and still is not an error.
#[tokio::test]
async fn a_malformed_lagged_frame_is_surfaced_not_dropped() {
    let mock = MockCore::start(routed(
        stream(
            vec![
                sse_frame(Some("lagged"), "lots"),
                sse_frame(None, &status_changed("completed", None)),
            ],
            StreamTail::Hold,
        ),
        poll_reply("running"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let mut items = Vec::new();
    core.follow_study(STUDY, &fast_options(5_000), |item| items.push(item))
        .await
        .expect("still not an error");

    assert!(
        items.iter().any(|item| matches!(
            item,
            FollowItem::Unrecognized { event, .. } if event == "lagged"
        )),
        "an unreadable lagged count must still be reported: {items:?}"
    );
}

// ---------------------------------------------------------------------------
// Disconnect, and the polling fallback
// ---------------------------------------------------------------------------

/// A stream cut mid-study falls back to polling and still reaches the study's
/// real outcome. The call must not fail: a lost stream is a lost *shortcut*.
#[tokio::test]
async fn a_dropped_stream_falls_back_to_polling() {
    let mock = MockCore::start(routed(
        stream(
            vec![sse_frame(None, &step_completed(0, "connect"))],
            StreamTail::Cut,
        ),
        // The first answer is the follow's own opening poll; the study
        // finishes two polls later.
        Behavior::Sequence(vec![
            poll_reply("running"),
            poll_reply("running"),
            poll_reply("completed"),
        ]),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let mut items = Vec::new();
    let outcome = core
        .follow_study(STUDY, &fast_options(5_000), |item| items.push(item))
        .await
        .expect("a dropped stream must not fail the call");

    assert!(outcome.used_polling, "the fallback should have engaged");
    assert_eq!(outcome.terminal_status.as_deref(), Some("completed"));
    assert!(!outcome.timed_out);

    // The transition is announced, not silent: a caller reading a follow's
    // output has to be able to see which mechanism produced the lines around
    // it, because polling reports strictly less.
    // The live half must actually have happened first, or this test would
    // pass just as well against a client that never opened a stream at all.
    let live_step = items.iter().position(|item| {
        matches!(
            item,
            FollowItem::Event(StudyEvent::StepCompleted { step_index: 0, .. })
        )
    });
    let fell_back = items.iter().position(|item| {
        matches!(
            item,
            FollowItem::Transport {
                mode: FollowMode::Polling,
                ..
            }
        )
    });
    assert!(
        live_step.is_some(),
        "the step pushed before the cut must have arrived live: {items:?}"
    );
    assert!(
        fell_back.is_some(),
        "the fallback must announce itself: {items:?}"
    );
    assert!(
        live_step < fell_back,
        "the live event must precede the fallback: {items:?}"
    );
    assert!(
        items
            .iter()
            .any(|item| matches!(item, FollowItem::Polled { status, .. } if status == "completed")),
        "polling must have produced the terminal status: {items:?}"
    );
}

/// A clean end-of-stream is the same case as a cut one: keep going by
/// polling. Core has no reason to close early, which is exactly why a client
/// that treated it as "the study is over" would be wrong.
#[tokio::test]
async fn a_cleanly_closed_stream_also_falls_back_to_polling() {
    let mock = MockCore::start(routed(
        stream(vec![sse_frame(None, &step_completed(0, "connect"))], StreamTail::Close),
        Behavior::Sequence(vec![poll_reply("running"), poll_reply("failed")]),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let outcome = core
        .follow_study(STUDY, &fast_options(5_000), |_| {})
        .await
        .expect("a closed stream must not fail the call");

    assert!(outcome.used_polling);
    assert_eq!(outcome.terminal_status.as_deref(), Some("failed"));
}

/// A refused subscription — an old Core, a proxy, a route that is not there
/// — falls back too, rather than failing before polling was ever tried.
#[tokio::test]
async fn a_refused_stream_falls_back_to_polling() {
    let mock = MockCore::start(routed(
        Behavior::plain_text_error(404, "Not Found", "no such route"),
        poll_reply("completed"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let mut items = Vec::new();
    let outcome = core
        .follow_study(STUDY, &fast_options(5_000), |item| items.push(item))
        .await
        .expect("a refused stream must not fail the call");

    assert!(outcome.used_polling);
    assert_eq!(outcome.terminal_status.as_deref(), Some("completed"));
    assert!(
        items.iter().any(|item| matches!(
            item,
            FollowItem::Transport { mode: FollowMode::Polling, detail } if detail.contains("404")
        )),
        "the reason the stream was refused belongs in the output: {items:?}"
    );
}

/// Neither mechanism available is the one real failure, and it must report as
/// one rather than as a silent empty follow.
#[tokio::test]
async fn a_follow_with_no_stream_and_no_poll_fails() {
    let mock = MockCore::start(Behavior::plain_text_error(
        404,
        "Not Found",
        "unknown study_id",
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let error = core
        .follow_study(STUDY, &fast_options(2_000), |_| {})
        .await
        .expect_err("nothing answered; this is a genuine failure");
    let text = format!("{error:#}");
    assert!(
        text.contains(STUDY),
        "the failure must name the study: {text}"
    );
}

// ---------------------------------------------------------------------------
// Ordering, budgets, and forward compatibility
// ---------------------------------------------------------------------------

/// Core holds the stream open forever, so a study that finished *before* the
/// subscribe emits no `StatusChanged` and would hang a follower that only
/// listened. The opening poll is what stops that, and this is the test that
/// would catch it being removed or moved.
#[tokio::test]
async fn an_already_finished_study_returns_without_waiting_for_an_event() {
    let mock = MockCore::start(routed(
        stream(vec![sse_keep_alive()], StreamTail::Hold),
        poll_reply("completed"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let started = Instant::now();
    let outcome = core
        // No deadline at all: if the opening poll is not consulted, this
        // hangs rather than fails, and the test times out loudly.
        .follow_study(
            STUDY,
            &FollowOptions {
                idle_timeout: Duration::from_secs(30),
                poll_interval: Duration::from_millis(10),
                deadline: None,
            },
            |_| {},
        )
        .await
        .expect("an already-finished study is answerable");

    assert_eq!(outcome.terminal_status.as_deref(), Some("completed"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it should return on the opening poll, not on a timeout"
    );
}

/// The subscription must carry the bearer token like every other route. The
/// stream is the one call that does not go through `CoreClient::send`, which
/// is where that is applied for everything else — so it is also the one that
/// could quietly stop sending it.
#[tokio::test]
async fn the_event_stream_request_carries_the_bearer_token() {
    let mock = MockCore::start(routed(
        stream(vec![sse_frame(None, &status_changed("completed", None))], StreamTail::Hold),
        poll_reply("running"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    core.follow_study(STUDY, &fast_options(5_000), |_| {})
        .await
        .expect("follow");

    let events_request = mock
        .requests()
        .into_iter()
        .find(|request| request.path().ends_with("/events"))
        .expect("the event stream should have been requested");
    assert_eq!(
        events_request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(
        events_request.header("accept"),
        Some("text/event-stream"),
        "the request should say what it expects back"
    );
}

/// A `kind` this build does not know must degrade to "there was an event I
/// did not understand", not to a broken client. Core's event enum has grown
/// once already (`GattTranscript`, which `embarch-core/interfaces.md` still
/// does not list) and there is no reason to think it is finished.
#[tokio::test]
async fn an_unknown_event_kind_does_not_break_the_stream() {
    let mock = MockCore::start(routed(
        stream(
            vec![
                sse_frame(
                    None,
                    &json!({ "kind": "SomethingCoreLearnedLater", "study_id": STUDY }).to_string(),
                ),
                sse_frame(None, &step_completed(4, "after the unknown one")),
                sse_frame(None, &status_changed("completed", None)),
            ],
            StreamTail::Hold,
        ),
        poll_reply("running"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let mut items = Vec::new();
    let outcome = core
        .follow_study(STUDY, &fast_options(5_000), |item| items.push(item))
        .await
        .expect("an unknown kind must not fail the call");

    assert!(
        items
            .iter()
            .any(|item| matches!(item, FollowItem::Unrecognized { .. })),
        "the unknown frame should be surfaced: {items:?}"
    );
    assert!(
        items.iter().any(|item| matches!(
            item,
            FollowItem::Event(StudyEvent::StepCompleted { step_index: 4, .. })
        )),
        "the event after it must still arrive: {items:?}"
    );
    assert_eq!(outcome.terminal_status.as_deref(), Some("completed"));
}

/// `SampleBatch` and `GattTranscript` are the bulk variants, and the MCP tool
/// counts rather than lists them. Decoding one at all is what that counting
/// depends on.
#[tokio::test]
async fn a_sample_batch_decodes_with_its_tap_name() {
    let mock = MockCore::start(routed(
        stream(
            vec![
                sse_frame(None, &sample_batch("rail-3v3", 3)),
                sse_frame(None, &status_changed("completed", None)),
            ],
            StreamTail::Hold,
        ),
        poll_reply("running"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let mut items = Vec::new();
    core.follow_study(STUDY, &fast_options(5_000), |item| items.push(item))
        .await
        .expect("follow");

    let batch = items
        .iter()
        .find_map(|item| match item {
            FollowItem::Event(StudyEvent::SampleBatch {
                stream_name,
                samples,
                ..
            }) => Some((stream_name.clone(), samples.len())),
            _ => None,
        })
        .expect("the sample batch should have decoded");
    assert_eq!(batch, ("rail-3v3".to_string(), 3));
}

/// A study that never finishes must stop at the deadline and say so, rather
/// than following forever. This is the bound that makes the MCP tool safe to
/// call at all.
#[tokio::test]
async fn a_follow_stops_at_its_deadline() {
    let mock = MockCore::start(routed(
        stream(vec![sse_keep_alive()], StreamTail::Hold),
        poll_reply("running"),
    ))
    .await;

    let core = CoreClient::new(&config(mock.base_url())).expect("client");
    let started = Instant::now();
    let outcome = core
        .follow_study(STUDY, &fast_options(250), |_| {})
        .await
        .expect("hitting the deadline is not an error");

    assert!(outcome.timed_out);
    assert!(outcome.terminal_status.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline should be what stopped it"
    );
}

/// The follow's items render to one JSON object each, with a `type` that
/// tells the two kinds of incompleteness apart. Both surfaces publish this
/// shape, so it is worth pinning where it is written rather than twice where
/// it is used.
#[test]
fn follow_items_render_with_a_type_discriminator() {
    let lagged = FollowItem::Lagged { missed: 3 }.to_json();
    assert_eq!(lagged["type"], "lagged");
    assert_eq!(lagged["missed"], 3);
    assert!(
        lagged["note"]
            .as_str()
            .expect("a lagged item carries a note")
            .contains("not a stream error"),
        "the note must say what a lagged frame is not: {lagged}"
    );

    let transport = FollowItem::Transport {
        mode: FollowMode::Polling,
        detail: "the live stream ended".to_string(),
    }
    .to_json();
    assert_eq!(transport["type"], "transport");
    assert_eq!(transport["mode"], "polling");
}
