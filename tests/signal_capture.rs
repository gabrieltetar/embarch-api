//! The three capture calls round-tripped against the mock Core
//! (`tasks/api/143`, `embarch-core` decision 92): the request each puts on
//! the wire, a read that passes `next_offset` back, and Core's `404` / `409`
//! text surfacing unchanged. The mock is not a stateful Core, so replies are
//! scripted; what is under test is the client, not Core's offsets.

mod support;

use embarch_core_client::{CoreClient, CoreConfig};
use serde_json::json;
use support::{Behavior, MockCore};

fn client(mock: &MockCore) -> CoreClient {
    let cfg: CoreConfig = serde_json::from_value(json!({ "token": "t", "base_url": mock.base_url() })).unwrap();
    CoreClient::new(&cfg).unwrap()
}

fn reply(state: &str, text: &str, offset: u64, next: u64) -> serde_json::Value {
    json!({
        "id": "cap-1", "signal": "console", "port": "COM9", "baud": 115200, "state": state,
        "running": state == "running" || state == "reopening", "offset": offset, "next_offset": next,
        "total_bytes": next, "text": text, "elapsed_ms": 1200,
        "gaps": [{ "offset": 3, "lost_at_ms": 100, "reopened_at_ms": 400 }],
    })
}

#[tokio::test]
async fn start_sends_the_caps_and_parses_the_reply() {
    let mock = MockCore::start(Behavior::json_ok(reply("running", "", 0, 0))).await;
    let r = client(&mock).signal_capture_start("console", Some(30), None).await.unwrap();
    assert_eq!((r.id.as_str(), r.state.as_str(), r.running), ("cap-1", "running", true));
    let req = &mock.requests()[0];
    assert_eq!(req.route(), ("POST".to_string(), "/signals/console/capture".to_string()));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&req.body_text()).unwrap(), json!({ "max_s": 30 }));
}

#[tokio::test]
async fn a_read_that_passes_next_offset_back_asks_only_for_what_is_new() {
    let mock = MockCore::start(Behavior::Sequence(vec![
        Behavior::json_ok(reply("running", "hello", 0, 5)),
        Behavior::json_ok(reply("running", " world", 5, 11)),
    ]))
    .await;
    let c = client(&mock);
    let first = c.signal_capture_read("console", None, None, None).await.unwrap();
    assert_eq!((first.text.as_str(), first.next_offset), ("hello", 5));
    let second = c.signal_capture_read("console", Some(first.next_offset), Some(100), Some("cap-1")).await.unwrap();
    assert_eq!(second.text, " world");
    assert_eq!(second.gaps[0].reopened_at_ms, Some(400));
    let reqs = mock.requests();
    assert_eq!(reqs[0].target, "/signals/console/capture");
    assert_eq!(reqs[1].target, "/signals/console/capture?after=5&limit=100&id=cap-1");
}

#[tokio::test]
async fn stop_is_a_delete_with_the_final_state() {
    let mock = MockCore::start(Behavior::json_ok(reply("stopped", "tail", 11, 15))).await;
    let r = client(&mock).signal_capture_stop("console", Some(11), None, None).await.unwrap();
    assert_eq!((r.state.as_str(), r.running, r.text.as_str()), ("stopped", false, "tail"));
    let req = &mock.requests()[0];
    assert_eq!((req.method.as_str(), req.target.as_str()), ("DELETE", "/signals/console/capture?after=11"));
}

#[tokio::test]
async fn a_409_on_start_surfaces_core_s_text() {
    let mock = MockCore::start(Behavior::plain_text_error(409, "Conflict", "signal console already has a running capture cap-0")).await;
    let e = client(&mock).signal_capture_start("console", None, None).await.unwrap_err();
    let t = format!("{e:#}");
    assert!(t.contains("409") && t.contains("already has a running capture cap-0"), "{t}");
}

#[tokio::test]
async fn a_404_on_read_and_stop_surfaces_core_s_text() {
    let mock = MockCore::start(Behavior::plain_text_error(404, "Not Found", "no capture on console")).await;
    let c = client(&mock);
    for e in [
        c.signal_capture_read("console", None, None, None).await.unwrap_err(),
        c.signal_capture_stop("console", None, None, None).await.unwrap_err(),
    ] {
        let t = format!("{e:#}");
        assert!(t.contains("404") && t.contains("no capture on console"), "{t}");
    }
}
