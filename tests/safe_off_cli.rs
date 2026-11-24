//! `declare-instrument` and `declare-signal` carry `safe_off` to Core, and a
//! re-declare that omits it keeps the stored one (`tasks/api/187`, decision
//! 97). Against a mock Core that answers the list first and the write second.

mod support;

use serde_json::{json, Value};
use support::{Behavior, MockCore, RecordedRequest};

const STORED_INSTRUMENT: &str = r#"{"name":"eload","protocol":"scpi","port":{"vid":6790,"pid":29987},"baud":9600,
    "write_terminator":"\n","read_terminator":"\r\n","min_interval_ms":0,"safe_off":["INP OFF","CURR 0"]}"#;

fn stored_signal() -> Value {
    json!({"name":"psu","origin_role":"","origin_board":"0671FF","direction":"bidirectional",
        "route":{"kind":"direct","port_serial":"ABC"},"safe_off":"output off"})
}

/// `reads`: whether the declaration reads the stored table first (it does when
/// the caller gave no safe_off), which decides the order the mock answers in.
async fn run(path: &str, reads: bool, list: Value, write: Behavior, args: &[&str]) -> (std::process::Output, Vec<RecordedRequest>) {
    let mock = MockCore::start(Behavior::Router {
        routes: vec![(path.to_string(), Behavior::Sequence(if reads {
                vec![Behavior::json_ok(list.clone()), write, Behavior::json_ok(list)]
            } else {
                vec![write, Behavior::json_ok(list)]
            }))],
        otherwise: Box::new(Behavior::json_ok(json!({}))),
    })
    .await;
    let dir = support::unique_dir("safeoff");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out, mock.requests())
}

fn body_of(reqs: &[RecordedRequest], method: &str) -> Value {
    let r = reqs.iter().find(|r| r.method == method).unwrap_or_else(|| panic!("no {method} request: {reqs:?}"));
    serde_json::from_slice(&r.body).unwrap()
}

fn instrument_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["declare-instrument", "eload", "--port", "1a86:7523", "--baud", "9600"];
    a.extend_from_slice(extra);
    a
}

async fn declare_instrument(extra: &[&str]) -> (std::process::Output, Vec<RecordedRequest>) {
    let stored: Value = serde_json::from_str(STORED_INSTRUMENT).unwrap();
    run("/instruments", !extra.contains(&"--safe-off") && !extra.contains(&"--clear-safe-off"), json!([stored.clone()]), Behavior::json_ok(stored), &instrument_args(extra)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_instrument_cli_sends_each_safe_off_line_in_order() {
    let (out, reqs) = declare_instrument(&["--safe-off", "OUTP OFF", "--safe-off", "VOLT 0"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(body_of(&reqs, "PUT")["safe_off"], json!(["OUTP OFF", "VOLT 0"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redeclare_without_safe_off_keeps_the_stored_one() {
    let (out, reqs) = declare_instrument(&[]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(body_of(&reqs, "PUT")["safe_off"], json!(["INP OFF", "CURR 0"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_safe_off_drops_the_stored_one_and_reads_nothing() {
    let (out, reqs) = declare_instrument(&["--clear-safe-off"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(reqs.iter().all(|r| r.method != "GET"), "a clear needs no read: {reqs:?}");
    assert!(body_of(&reqs, "PUT").get("safe_off").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_stored_table_stops_a_redeclare_rather_than_dropping_the_line() {
    let stored: Value = serde_json::from_str(STORED_INSTRUMENT).unwrap();
    let mock = MockCore::start(Behavior::Reply {
        status: 500,
        reason: "Internal Server Error".into(),
        content_type: "text/plain".into(),
        body: "boom".into(),
    })
    .await;
    let _ = stored;
    let dir = support::unique_dir("safeoff-err");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(instrument_args(&[]))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.status.success());
    assert!(mock.requests().iter().all(|r| r.method != "PUT"));
}

fn signal_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        "declare-signal", "--name", "psu", "--origin-board", "0671FF", "--direction", "bidirectional", "--route-kind", "direct",
        "--port-serial", "ABC",
    ];
    a.extend_from_slice(extra);
    a
}

async fn declare_signal(extra: &[&str]) -> (std::process::Output, Vec<RecordedRequest>) {
    let write = Behavior::Reply { status: 204, reason: "No Content".into(), content_type: "application/json".into(), body: String::new() };
    run("/signals", !extra.contains(&"--safe-off"), json!([stored_signal()]), write, &signal_args(extra)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_signal_cli_sends_safe_off() {
    let (out, reqs) = declare_signal(&["--safe-off", "outputs off"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(body_of(&reqs, "POST")["safe_off"], json!("outputs off"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signal_redeclare_without_safe_off_keeps_the_stored_one() {
    let (out, reqs) = declare_signal(&[]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(body_of(&reqs, "POST")["safe_off"], json!("output off"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_signal_safe_off_clears_the_stored_one() {
    let (out, reqs) = declare_signal(&["--safe-off", ""]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(body_of(&reqs, "POST").get("safe_off").is_none());
}
