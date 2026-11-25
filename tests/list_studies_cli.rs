//! `list-studies` against a mock Core's `GET /studies` (EMB-405, `tasks/api/192`):
//! newest first, `--limit`, taps with a flat `encoding_kind`, `taps: null`
//! kept, the dead-Core error shape, and no MCP tool of the same name.

mod support;

use serde_json::{json, Value};
use support::{Behavior, MockCore};

fn listing() -> Value {
    json!({
        "keep": 20,
        "studies": [
            {"study_id": "b-none", "status": "completed", "taps": null, "note": "no streams index"},
            {"study_id": "a-none", "status": "failed", "taps": []},
            {"study_id": "old", "study_name": "Old", "status": "completed", "started_utc_ms": 100, "ended_utc_ms": 200,
             "taps": [{"name": "trace", "encoding": "OutpostTrace", "rendered": true}]},
            {"study_id": "new", "study_name": "New", "status": "running", "started_utc_ms": 900,
             "taps": [
                {"name": "adc", "encoding": {"Samples": {"layout": "F32Le", "unit": "Volts", "channel_id": 1}}, "rendered": true},
                {"name": "console", "encoding": "Text", "rendered": true},
                {"name": "gatt", "encoding": "GattTranscript", "rendered": false},
                {"name": "blob", "encoding": "Raw", "rendered": false}
             ]},
            {"study_id": "mid", "status": "completed", "started_utc_ms": 500, "taps": null}
        ]
    })
}

fn run(base_url: &str, args: &[&str]) -> (std::process::Output, Value) {
    let dir = support::unique_dir("list-studies");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{base_url}\"\ntoken = \"t\"\n")).unwrap();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let v = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out, v)
}

#[tokio::test]
async fn lists_newest_first_with_flat_encoding_kinds() {
    let mock = MockCore::start(Behavior::json_ok(listing())).await;
    let base = mock.base_url().to_string();
    let (out, v) = tokio::task::spawn_blocking(move || run(&base, &["--json", "list-studies"])).await.unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(v["success"], true);
    assert_eq!(v["keep"], 20);
    let ids: Vec<&str> = v["studies"].as_array().unwrap().iter().map(|s| s["study_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["new", "mid", "old", "a-none", "b-none"]);
    let taps = v["studies"][0]["taps"].as_array().unwrap();
    let kinds: Vec<&str> = taps.iter().map(|t| t["encoding_kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["samples", "text", "gatt_transcript", "raw"]);
    assert_eq!(taps[0]["encoding"]["Samples"]["channel_id"], 1);
    assert_eq!(v["studies"][2]["taps"][0]["encoding"], "OutpostTrace");
    assert_eq!(v["studies"][2]["taps"][0]["encoding_kind"], "outpost_trace");
    assert!(v["studies"][1]["taps"].is_null(), "unreadable taps stay null");
    assert!(v["studies"][4]["taps"].is_null());
    assert_eq!(mock.requests().len(), 1);
    assert!(mock.requests()[0].header("x-embarch-lease").is_none());
}

#[tokio::test]
async fn limit_keeps_the_first_n_after_sorting() {
    let mock = MockCore::start(Behavior::json_ok(listing())).await;
    let base = mock.base_url().to_string();
    let (_, v) = tokio::task::spawn_blocking(move || run(&base, &["--json", "list-studies", "--limit", "2"])).await.unwrap();
    let ids: Vec<&str> = v["studies"].as_array().unwrap().iter().map(|s| s["study_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["new", "mid"]);
}

#[tokio::test]
async fn a_study_with_no_start_time_follows_those_with_one() {
    let mock = MockCore::start(Behavior::json_ok(listing())).await;
    let base = mock.base_url().to_string();
    let (_, v) = tokio::task::spawn_blocking(move || run(&base, &["--json", "list-studies", "--limit", "4"])).await.unwrap();
    let ids: Vec<&str> = v["studies"].as_array().unwrap().iter().map(|s| s["study_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["new", "mid", "old", "a-none"]);
}

#[tokio::test]
async fn human_text_has_a_line_per_study_with_its_taps_indented() {
    let mock = MockCore::start(Behavior::json_ok(listing())).await;
    let base = mock.base_url().to_string();
    let (out, _) = tokio::task::spawn_blocking(move || run(&base, &["list-studies", "--limit", "1"])).await.unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.starts_with("new  New  running"), "{text}");
    assert!(text.contains("\n    adc (samples)"), "{text}");
}

#[test]
fn an_unreachable_core_gives_the_list_error_shape() {
    let (out, v) = run("http://127.0.0.1:1", &["--json", "list-studies"]);
    assert!(!out.status.success());
    assert_eq!(v["success"], false);
    assert!(v["error"].as_str().unwrap().starts_with("list-studies failed:"), "{v}");
}

#[test]
fn no_mcp_tool_named_list_studies() {
    let src = include_str!("../src/tools.rs");
    assert!(!src.contains("async fn list_studies"), "list-studies is a CLI verb only (EMB-405)");
}
